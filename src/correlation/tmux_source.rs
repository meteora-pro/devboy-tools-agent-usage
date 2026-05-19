//! Focus stats источник на базе tmux_activity таблицы.
//!
//! Это альтернатива ActivityWatch для случаев когда:
//! - AW не установлен (легче зависимостей)
//! - Работа целиком в терминале (AW window events избыточны)
//! - SSH-сессия без GUI (AW недоступен, tmux есть)
//!
//! Концепция: каждый snapshot tmux_activity = "квант времени" работы. Если
//! pane_active=1 и command='claude' → пользователь "видит Claude". Если
//! idle_ms > threshold → GUI AFK (но Claude может работать автономно).
//!
//! Эта функция возвращает FocusStats совместимый с correlation::engine, чтобы
//! в будущем `focus --source tmux` мог использовать её drop-in.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;

/// Focus stats аналог из tmux данных. По смыслу аналогичен
/// correlation::engine::FocusStats, но computed из tmux_activity.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TmuxFocusStats {
    /// Сколько уникальных snapshot-таймстампов в диапазоне.
    pub total_snapshots: i64,
    /// Сколько snapshots где idle_ms > threshold (default 5 min).
    pub idle_snapshots: i64,
    /// Сколько snapshots где данные про idle доступны (idle_ms IS NOT NULL).
    pub idle_data_available: i64,
    /// Активные pane'ы по командам (top N).
    pub top_commands: Vec<(String, i64)>,
    /// Активные pane'ы по сессиям.
    pub top_sessions: Vec<(String, i64)>,
    /// Был ли claude активен в каждом snapshot ≥1 раз.
    pub claude_visible_snapshots: i64,
    /// idle_threshold_ms — для воспроизводимости.
    pub idle_threshold_ms: i64,
}

impl TmuxFocusStats {
    /// % времени пользователь физически смотрел на claude
    /// (= claude_visible / total).
    pub fn claude_visibility_pct(&self) -> Option<f64> {
        if self.total_snapshots == 0 {
            None
        } else {
            Some((self.claude_visible_snapshots as f64 / self.total_snapshots as f64) * 100.0)
        }
    }

    /// % AFK среди snapshot где есть данные про idle.
    pub fn idle_pct(&self) -> Option<f64> {
        if self.idle_data_available == 0 {
            None
        } else {
            Some((self.idle_snapshots as f64 / self.idle_data_available as f64) * 100.0)
        }
    }
}

/// Вычислить TmuxFocusStats из БД для заданного периода.
pub fn compute(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    idle_threshold_ms: i64,
) -> Result<TmuxFocusStats> {
    let (total, idle, idle_known): (i64, i64, i64) = conn
        .query_row(
            "SELECT
                COUNT(DISTINCT ts_ms),
                COUNT(DISTINCT CASE WHEN idle_ms > ? THEN ts_ms END),
                COUNT(DISTINCT CASE WHEN idle_ms IS NOT NULL THEN ts_ms END)
             FROM tmux_activity
             WHERE ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![idle_threshold_ms, from_ms, to_ms],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((0, 0, 0));

    let claude_visible: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT ts_ms) FROM tmux_activity
             WHERE ts_ms >= ? AND ts_ms < ?
               AND pane_active = 1 AND command = 'claude'",
            rusqlite::params![from_ms, to_ms],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let top_commands = top_by(
        conn, "command", from_ms, to_ms, /*active_only=*/ true, 10,
    )?;
    let top_sessions = top_by(
        conn, "session", from_ms, to_ms, /*active_only=*/ true, 10,
    )?;

    Ok(TmuxFocusStats {
        total_snapshots: total,
        idle_snapshots: idle,
        idle_data_available: idle_known,
        top_commands,
        top_sessions,
        claude_visible_snapshots: claude_visible,
        idle_threshold_ms,
    })
}

fn top_by(
    conn: &Connection,
    column: &str,
    from_ms: i64,
    to_ms: i64,
    active_only: bool,
    limit: usize,
) -> Result<Vec<(String, i64)>> {
    let active_filter = if active_only {
        " AND pane_active = 1 "
    } else {
        ""
    };
    let sql = format!(
        "SELECT {col}, COUNT(*) AS n FROM tmux_activity
         WHERE ts_ms >= ? AND ts_ms < ? {active}
         GROUP BY {col} ORDER BY n DESC LIMIT ?",
        col = column,
        active = active_filter,
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![from_ms, to_ms, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Hybrid resolution: возвращает true если tmux данных достаточно для
/// данного периода (например >= min_snapshots). Иначе caller должен
/// fallback на AW.
pub fn has_sufficient_data(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    min_snapshots: i64,
) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT ts_ms) FROM tmux_activity
             WHERE ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![from_ms, to_ms],
            |r| r.get(0),
        )
        .unwrap_or(0);
    n >= min_snapshots
}

/// Map (session, window) → top command (для построения hybrid focus reports).
pub fn pane_command_distribution(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT command, COUNT(*) FROM tmux_activity
         WHERE ts_ms >= ? AND ts_ms < ?
         GROUP BY command",
    )?;
    let rows: HashMap<String, i64> = stmt
        .query_map(rusqlite::params![from_ms, to_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use rusqlite::params;
    use tempfile::tempdir;

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        (dir, conn)
    }

    fn insert(
        conn: &Connection,
        ts: i64,
        session: &str,
        window: i64,
        pane: i64,
        active: bool,
        cmd: &str,
        idle_ms: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO tmux_activity
             (ts_ms, session, window_idx, window_name, pane_idx, pane_active, command, cwd, idle_ms)
             VALUES (?, ?, ?, 'w', ?, ?, ?, '/', ?)",
            params![
                ts,
                session,
                window,
                pane,
                if active { 1 } else { 0 },
                cmd,
                idle_ms
            ],
        )
        .unwrap();
    }

    #[test]
    fn empty_returns_zeros() {
        let (_d, c) = open();
        let stats = compute(&c, 0, i64::MAX, 5 * 60_000).unwrap();
        assert_eq!(stats.total_snapshots, 0);
        assert!(stats.claude_visibility_pct().is_none());
        assert!(stats.idle_pct().is_none());
    }

    #[test]
    fn computes_basic_stats() {
        let (_d, c) = open();
        // 3 snapshots, в каждом по 2 pane: 1 claude active, 1 bash inactive
        for i in 0..3 {
            let ts = 1_000_000 + i * 1000;
            insert(&c, ts, "main", 0, 0, true, "claude", Some(100));
            insert(&c, ts, "main", 0, 1, false, "bash", Some(100));
        }
        let stats = compute(&c, 0, i64::MAX, 5 * 60_000).unwrap();
        assert_eq!(stats.total_snapshots, 3);
        assert_eq!(stats.claude_visible_snapshots, 3);
        assert!((stats.claude_visibility_pct().unwrap() - 100.0).abs() < 0.01);

        // commands: только pane_active=1 учитывается → 3 × claude, 0 × bash
        let claude = stats
            .top_commands
            .iter()
            .find(|(c, _)| c == "claude")
            .unwrap();
        assert_eq!(claude.1, 3);
    }

    #[test]
    fn idle_threshold_classification() {
        let (_d, c) = open();
        // 4 snapshots, idle 100/200/600_000/700_000
        for (i, idle) in [100, 200, 600_000, 700_000].iter().enumerate() {
            let ts = i as i64 + 1;
            insert(&c, ts, "s", 0, 0, true, "x", Some(*idle));
        }
        let stats = compute(&c, 0, i64::MAX, 5 * 60_000).unwrap();
        assert_eq!(stats.total_snapshots, 4);
        assert_eq!(stats.idle_data_available, 4);
        assert_eq!(stats.idle_snapshots, 2, "только idle > 5min считается");
        assert!((stats.idle_pct().unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn has_sufficient_data_threshold() {
        let (_d, c) = open();
        for i in 0..5 {
            insert(&c, i + 1, "s", 0, 0, true, "x", None);
        }
        assert!(has_sufficient_data(&c, 0, i64::MAX, 3));
        assert!(has_sufficient_data(&c, 0, i64::MAX, 5));
        assert!(!has_sufficient_data(&c, 0, i64::MAX, 6));
    }

    #[test]
    fn time_range_filter() {
        let (_d, c) = open();
        insert(&c, 100, "a", 0, 0, true, "x", None);
        insert(&c, 200, "a", 0, 0, true, "x", None);
        insert(&c, 300, "a", 0, 0, true, "x", None);
        let stats = compute(&c, 150, 250, 5 * 60_000).unwrap();
        assert_eq!(stats.total_snapshots, 1, "только snapshot ts=200 попадает");
    }
}
