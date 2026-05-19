//! SQLite schema индекса + миграции.
//!
//! Путь к БД: `$XDG_CACHE_HOME/devboy-tools-agent-usage/index.db`
//! (на Linux: `~/.cache/devboy-tools-agent-usage/index.db`).
//!
//! Schema version хранится в таблице `schema_versions`. При увеличении
//! `SCHEMA_VERSION` нужно добавить новую миграцию в `migrate()`.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Текущая версия схемы. Инкрементируется при добавлении миграций.
pub const SCHEMA_VERSION: u32 = 5;

/// Путь к БД индекса в XDG cache dir.
pub fn index_db_path() -> Result<PathBuf> {
    let cache = dirs::cache_dir().context("Не удалось определить XDG cache directory")?;
    let dir = cache.join("devboy-tools-agent-usage");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Не удалось создать {}", dir.display()))?;
    Ok(dir.join("index.db"))
}

/// Открыть индекс по дефолтному пути (XDG cache), применить миграции.
pub fn open_index() -> Result<Connection> {
    let path = index_db_path()?;
    open_index_at(&path)
}

/// Открыть индекс по конкретному пути. Используется тестами и при кастомных
/// деплойментах (например, общая БД на несколько машин).
pub fn open_index_at(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Не удалось открыть SQLite БД по пути {}", path.display()))?;

    // WAL для concurrent read во время write (statusline читает пока indexer пишет).
    // synchronous=NORMAL — безопасный trade-off между производительностью и durability:
    // потеря последних транзакций возможна только при OS crash, не при процесс-крэше.
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA foreign_keys=ON;
        PRAGMA temp_store=MEMORY;
        ",
    )
    .context("Не удалось настроить PRAGMA")?;

    migrate(&conn).context("Не удалось применить миграции схемы")?;
    Ok(conn)
}

/// Применить миграции схемы по очереди от текущей версии до `SCHEMA_VERSION`.
fn migrate(conn: &Connection) -> Result<()> {
    // schema_versions всегда существует — создаём первой, если БД пустая.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_versions (
            version    INTEGER PRIMARY KEY,
            applied_at TEXT    NOT NULL
        );",
    )?;

    let current: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_versions",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if current < 1 {
        apply_v1(conn)?;
    }
    if current < 2 {
        apply_v2(conn)?;
    }
    if current < 3 {
        apply_v3(conn)?;
    }
    if current < 4 {
        apply_v4(conn)?;
    }
    if current < 5 {
        apply_v5(conn)?;
    }

    Ok(())
}

/// Миграция v1 — создание базовых таблиц для индекса.
///
/// - `parsed_files` — watermark per JSONL (mtime + last_offset для append-only логов).
/// - `accounts` — Claude Code аккаунты (заполняется L2.T4 из credentials.json).
/// - `turns` — фактические записи турнов, главная аналитическая таблица.
///   Индексы на `ts_ms`, `session_id`, `(account_id, ts_ms)` — для быстрых window queries.
fn apply_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V1)?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at) VALUES (1, datetime('now'))",
        [],
    )?;
    Ok(())
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS parsed_files (
    path        TEXT PRIMARY KEY,
    mtime_ns    INTEGER NOT NULL,
    size_bytes  INTEGER NOT NULL,
    last_offset INTEGER NOT NULL DEFAULT 0,
    parsed_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id            TEXT PRIMARY KEY,
    email         TEXT,
    plan          TEXT,              -- Free | Pro | Max5 | Max20 | Unknown
    first_seen_ms INTEGER,
    last_seen_ms  INTEGER,
    notes         TEXT
);

CREATE TABLE IF NOT EXISTS turns (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id          TEXT    NOT NULL,
    project             TEXT,
    ts_ms               INTEGER NOT NULL,
    model               TEXT,
    account_id          TEXT,
    tokens_input        INTEGER NOT NULL DEFAULT 0,
    tokens_output       INTEGER NOT NULL DEFAULT 0,
    tokens_cache_create INTEGER NOT NULL DEFAULT 0,
    tokens_cache_read   INTEGER NOT NULL DEFAULT 0,
    cost_usd            REAL    NOT NULL DEFAULT 0,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);

CREATE INDEX IF NOT EXISTS idx_turns_ts          ON turns(ts_ms);
CREATE INDEX IF NOT EXISTS idx_turns_session     ON turns(session_id);
CREATE INDEX IF NOT EXISTS idx_turns_account_ts  ON turns(account_id, ts_ms);
"#;

/// Миграция v2 — таблица `account_switches` для истории переключения аккаунтов.
///
/// Заполняется индексером: при каждом проходе сравниваем `detect_current()` с
/// последней известной строкой в `accounts`; если account_id различается —
/// фиксируем switch event.
fn apply_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V2)?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at) VALUES (2, datetime('now'))",
        [],
    )?;
    Ok(())
}

const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS account_switches (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms            INTEGER NOT NULL,           -- момент когда обнаружили switch
    previous_account TEXT,                       -- NULL если первый detect когда-либо
    current_account  TEXT    NOT NULL,
    confidence       TEXT    NOT NULL DEFAULT 'medium',  -- low | medium | high
    detected_at      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_switches_ts ON account_switches(ts_ms);
"#;

/// Миграция v3 — таблица `tmux_activity` для tmux-based focus tracking.
///
/// Записывается в результате периодического polling tmux state. Каждый snapshot
/// = одна строка per pane (так удобнее для аналитики; alternative — JSON массив
/// per snapshot, но это сложнее GROUP BY).
///
/// Семантика: pane_active=1 в exactly одной строке per (session, window, ts) —
/// активный pane в этот момент. idle_ms — best-effort; NULL если из SSH/tmux
/// мы не можем дотянуться до GUI session.
fn apply_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V3)?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at) VALUES (3, datetime('now'))",
        [],
    )?;
    Ok(())
}

const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS tmux_activity (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms        INTEGER NOT NULL,         -- момент snapshot
    session      TEXT    NOT NULL,         -- имя tmux session (main, work, ...)
    window_idx   INTEGER NOT NULL,
    window_name  TEXT,
    pane_idx     INTEGER NOT NULL,
    pane_active  INTEGER NOT NULL,         -- 1/0
    command      TEXT,                     -- claude / vim / cargo / bash / ...
    cwd          TEXT,                     -- pane_current_path
    idle_ms      INTEGER                   -- NULL если AFK не определён
);

CREATE INDEX IF NOT EXISTS idx_tmux_activity_ts      ON tmux_activity(ts_ms);
CREATE INDEX IF NOT EXISTS idx_tmux_activity_session ON tmux_activity(session, ts_ms);
"#;

/// Миграция v4 — таблица `plan_overrides` для honest ceiling tracking.
///
/// Source levels:
///   manual            — пользователь установил через `limits ceiling --set N`
///   calibrated        — auto-detected из 429 rate_limit_error в JSONL
///   default-community — fallback из hardcoded ccusage-style оценок
///
/// limits engine читает override first, иначе использует plan.weekly_token_ceiling().
fn apply_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V4)?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at) VALUES (4, datetime('now'))",
        [],
    )?;
    Ok(())
}

const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS plan_overrides (
    account_id            TEXT    PRIMARY KEY,
    weekly_ceiling_tokens INTEGER NOT NULL,
    source                TEXT    NOT NULL,    -- manual | calibrated | default-community
    set_at                TEXT    NOT NULL,
    notes                 TEXT,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);
"#;

/// Миграция v5 — таблица `oauth_usage_snapshots` для истории OAuth endpoint.
///
/// Один snapshot = один успешный fetch /api/oauth/usage. Используется для:
/// - real-time показа в statusline (last snapshot)
/// - trend analysis (delta между snapshots)
/// - reconciliation с local JSONL tokens (oauth.T7)
fn apply_v5(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_V5)?;
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at) VALUES (5, datetime('now'))",
        [],
    )?;
    Ok(())
}

const SCHEMA_V5: &str = r#"
CREATE TABLE IF NOT EXISTS oauth_usage_snapshots (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms                   INTEGER NOT NULL,
    account_id              TEXT,
    five_hour_pct           REAL    NOT NULL,
    five_hour_resets_at     TEXT,
    seven_day_pct           REAL    NOT NULL,
    seven_day_resets_at     TEXT,
    seven_day_sonnet_pct    REAL,
    extra_used_credits      REAL,
    extra_monthly_limit     REAL,
    extra_currency          TEXT,
    FOREIGN KEY (account_id) REFERENCES accounts(id)
);

CREATE INDEX IF NOT EXISTS idx_oauth_usage_ts          ON oauth_usage_snapshots(ts_ms);
CREATE INDEX IF NOT EXISTS idx_oauth_usage_account_ts  ON oauth_usage_snapshots(account_id, ts_ms);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_tmp() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = open_index_at(&path).expect("открытие индекса должно работать");
        (dir, conn)
    }

    #[test]
    fn migration_creates_all_tables() {
        let (_dir, conn) = open_tmp();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        for required in [
            "account_switches",
            "accounts",
            "oauth_usage_snapshots",
            "parsed_files",
            "plan_overrides",
            "schema_versions",
            "tmux_activity",
            "turns",
        ] {
            assert!(
                tables.contains(&required.to_string()),
                "таблица {} должна существовать, актуальные: {:?}",
                required,
                tables
            );
        }
    }

    #[test]
    fn migration_creates_all_indexes() {
        let (_dir, conn) = open_tmp();

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();

        for required in ["idx_turns_account_ts", "idx_turns_session", "idx_turns_ts"] {
            assert!(
                indexes.contains(&required.to_string()),
                "индекс {} должен существовать, актуальные: {:?}",
                required,
                indexes
            );
        }
    }

    #[test]
    fn migration_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Открываем дважды — миграция должна примениться только один раз.
        let _ = open_index_at(&path).unwrap();
        let conn = open_index_at(&path).unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_versions WHERE version = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "версия 1 должна быть применена ровно один раз");
    }

    #[test]
    fn current_schema_version_matches_constant() {
        let (_dir, conn) = open_tmp();
        let max: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_versions",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            max, SCHEMA_VERSION,
            "после открытия БД её версия должна совпадать с SCHEMA_VERSION"
        );
    }

    #[test]
    fn turns_table_accepts_basic_insert() {
        let (_dir, conn) = open_tmp();
        conn.execute(
            "INSERT INTO turns
             (session_id, project, ts_ms, model, tokens_input, tokens_output, cost_usd)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "00000000-0000-0000-0000-000000000001",
                "test/project",
                1_700_000_000_000_i64,
                "claude-sonnet-4-6",
                100,
                50,
                0.0015_f64,
            ],
        )
        .expect("insert должен работать");

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
