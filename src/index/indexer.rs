//! Инкрементальный индексер JSONL → SQLite.
//!
//! Стратегия:
//! 1. discover_jsonl_files — список всех файлов (использует существующий парсер,
//!    дедуплицирует по UUID).
//! 2. Для каждого файла читаем watermark из `parsed_files` (mtime, size, last_offset).
//! 3. Решаем что делать:
//!    - Новый файл → парсим целиком с offset=0.
//!    - Размер уменьшился (truncate/rotation) → инвалидация: удаляем старые turns
//!      этого файла, парсим целиком.
//!    - mtime + size совпадают с watermark → пропуск.
//!    - Иначе → читаем только от `last_offset` до EOF (JSONL append-only).
//! 4. Каждый assistant-event с usage → строка в `turns`.
//! 5. Обновляем watermark в transaction вместе с inserts.
//!
//! Не зависит от ClaudeSession/Turn структур — парсит JSON напрямую через
//! `serde_json::Value`, чтобы избежать жёсткой связки и поддерживать новые поля
//! без обновления типизированных моделей (cache_creation как объект и пр.).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::fs::{metadata, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

use crate::account::detection::{self, AccountInfo};
use crate::claude::parser::{discover_jsonl_files, JsonlFileInfo};
use crate::claude::tokens;

/// Статистика прохода индексации.
#[derive(Debug, Default, Clone)]
pub struct IndexStats {
    pub files_scanned: usize,
    pub files_skipped: usize,
    pub files_parsed_full: usize,
    pub files_parsed_incremental: usize,
    pub files_truncated_reparsed: usize,
    pub files_missing: usize,
    pub files_errored: usize,
    pub turns_inserted: usize,
    pub lines_invalid_json: usize,
}

impl IndexStats {
    /// Краткое summary для CLI / лога.
    pub fn summary(&self) -> String {
        format!(
            "scanned={} skip={} full={} inc={} trunc={} miss={} err={} turns+={} bad_json={}",
            self.files_scanned,
            self.files_skipped,
            self.files_parsed_full,
            self.files_parsed_incremental,
            self.files_truncated_reparsed,
            self.files_missing,
            self.files_errored,
            self.turns_inserted,
            self.lines_invalid_json,
        )
    }
}

/// Watermark одного файла в `parsed_files`.
struct FileWatermark {
    mtime_ns: i64,
    size_bytes: i64,
    last_offset: i64,
}

/// Главный entrypoint: проиндексировать всё в `claude_projects_dir`.
///
/// Если текущий аккаунт определяется (через ENV или credentials.json), он
/// записывается в `accounts` и проставляется новым turn'ам. Иначе turns
/// сохраняются с `account_id = NULL` — заполнится позже через `accounts assign`.
pub fn index_all(conn: &mut Connection, claude_projects_dir: &Path) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let files = discover_jsonl_files(claude_projects_dir).context("discover JSONL")?;
    stats.files_scanned = files.len();

    let current_account = detection::detect_current();
    if let Some(info) = &current_account {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Err(e) = detection::upsert_account(conn, info, now_ms) {
            eprintln!("Warning: не удалось записать account {} ({})", info.id, e);
        }
    }

    for file_info in files {
        if let Err(e) = index_one_file(conn, &file_info, current_account.as_ref(), &mut stats) {
            stats.files_errored += 1;
            eprintln!("Warning: пропускаем {} ({})", file_info.path.display(), e);
        }
    }

    Ok(stats)
}

fn index_one_file(
    conn: &mut Connection,
    file_info: &JsonlFileInfo,
    current_account: Option<&AccountInfo>,
    stats: &mut IndexStats,
) -> Result<()> {
    let path = &file_info.path;
    let path_str = path.to_string_lossy().into_owned();
    let project = &file_info.project_name;

    // 1. Снять текущие mtime/size — если файл исчез, фиксируем и идём дальше.
    let meta = match metadata(path) {
        Ok(m) => m,
        Err(_) => {
            stats.files_missing += 1;
            return Ok(());
        }
    };
    let current_mtime_ns = system_time_to_ns(meta.modified().ok());
    let current_size = meta.len() as i64;

    // 2. Прочитать watermark.
    let watermark: Option<FileWatermark> = conn
        .query_row(
            "SELECT mtime_ns, size_bytes, last_offset FROM parsed_files WHERE path = ?",
            params![path_str],
            |r| {
                Ok(FileWatermark {
                    mtime_ns: r.get(0)?,
                    size_bytes: r.get(1)?,
                    last_offset: r.get(2)?,
                })
            },
        )
        .optional()?;

    // 3. Решить стратегию: skip / full / incremental.
    enum Strategy {
        Skip,
        Full,
        Truncated,
        Incremental(u64),
    }
    let strategy = match &watermark {
        None => Strategy::Full,
        Some(w) if current_size < w.size_bytes => Strategy::Truncated,
        Some(w) if current_size == w.size_bytes && current_mtime_ns == w.mtime_ns => Strategy::Skip,
        Some(w) => Strategy::Incremental(w.last_offset as u64),
    };

    match strategy {
        Strategy::Skip => {
            stats.files_skipped += 1;
            return Ok(());
        }
        Strategy::Truncated => {
            stats.files_truncated_reparsed += 1;
        }
        Strategy::Full => {
            stats.files_parsed_full += 1;
        }
        Strategy::Incremental(_) => {
            stats.files_parsed_incremental += 1;
        }
    }

    let start_offset: u64 = match strategy {
        Strategy::Incremental(off) => off,
        _ => 0,
    };

    // 4. Открыть файл и seek.
    let mut file = File::open(path).context("открытие JSONL")?;
    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset)).context("seek")?;
    }

    let reader = BufReader::new(file);

    let tx = conn.transaction()?;

    // При truncation / full reparse — очищаем старые turns этого session_id
    // (одна сессия = один JSONL файл, имя файла = sessionId).
    if matches!(strategy, Strategy::Truncated | Strategy::Full) {
        if let Some(sid) = path.file_stem().and_then(|s| s.to_str()) {
            tx.execute("DELETE FROM turns WHERE session_id = ?", params![sid])?;
        }
    }

    let mut current_offset = start_offset;
    let mut local_inserted = 0usize;
    let mut local_invalid = 0usize;

    for line_res in reader.lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        // BufRead.lines() съедает \n, добавляем 1 байт обратно к offset
        let line_bytes = line.len() as u64 + 1;

        match parse_assistant_event(&line) {
            Ok(Some(rec)) => {
                tx.execute(
                    "INSERT INTO turns
                     (session_id, project, ts_ms, model, account_id,
                      tokens_input, tokens_output, tokens_cache_create, tokens_cache_read,
                      cost_usd)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        rec.session_id,
                        project,
                        rec.ts_ms,
                        rec.model,
                        current_account.map(|a| a.id.as_str()),
                        rec.input as i64,
                        rec.output as i64,
                        rec.cache_create as i64,
                        rec.cache_read as i64,
                        rec.cost,
                    ],
                )?;
                local_inserted += 1;
            }
            Ok(None) => {}
            Err(_) => {
                local_invalid += 1;
            }
        }
        current_offset += line_bytes;
    }

    // 5. Update watermark в той же транзакции.
    tx.execute(
        "INSERT INTO parsed_files (path, mtime_ns, size_bytes, last_offset, parsed_at)
         VALUES (?, ?, ?, ?, datetime('now'))
         ON CONFLICT(path) DO UPDATE SET
            mtime_ns = excluded.mtime_ns,
            size_bytes = excluded.size_bytes,
            last_offset = excluded.last_offset,
            parsed_at = excluded.parsed_at",
        params![
            path_str,
            current_mtime_ns,
            current_size,
            current_offset as i64,
        ],
    )?;

    tx.commit()?;

    stats.turns_inserted += local_inserted;
    stats.lines_invalid_json += local_invalid;
    Ok(())
}

/// Минимальные данные turn'а, нужные для SQLite-индекса.
struct ExtractedTurn {
    session_id: String,
    ts_ms: i64,
    model: String,
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    cost: f64,
}

/// Распарсить одну строку JSONL. Возвращает Some, только если это assistant-event
/// с usage. Все остальные типы (user, summary, system, tool_result) — None.
fn parse_assistant_event(line: &str) -> Result<Option<ExtractedTurn>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(line)?;

    let event_type = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    if event_type != "assistant" {
        return Ok(None);
    }

    let session_id = v
        .get("sessionId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return Ok(None);
    }

    let ts_str = v.get("timestamp").and_then(|x| x.as_str()).unwrap_or("");
    let ts_ms = match DateTime::parse_from_rfc3339(ts_str) {
        Ok(dt) => dt.with_timezone(&Utc).timestamp_millis(),
        Err(_) => return Ok(None),
    };

    let msg = v.get("message");
    let model = msg
        .and_then(|m| m.get("model"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let usage = msg.and_then(|m| m.get("usage"));

    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    // cache_creation_input_tokens может быть либо число (старый формат),
    // либо отсутствовать с новой структурой cache_creation.{ephemeral_5m, ephemeral_1h}.
    let cache_create_flat = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let (cache_create_5m, cache_create_1h) = match usage.and_then(|u| u.get("cache_creation")) {
        Some(Value::Object(_)) => {
            let obj = usage.unwrap().get("cache_creation").unwrap();
            let m5 = obj
                .get("ephemeral_5m_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let m1h = obj
                .get("ephemeral_1h_input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            (m5, m1h)
        }
        _ => (0, 0),
    };
    let cache_create = cache_create_flat + cache_create_5m + cache_create_1h;

    if input == 0 && output == 0 && cache_create == 0 && cache_read == 0 {
        // assistant event без usage (например thinking-only) — пропускаем,
        // чтобы не плодить пустые строки.
        return Ok(None);
    }

    let cost = tokens::calculate_cost_from_parts(input, output, cache_create, cache_read, &model);

    Ok(Some(ExtractedTurn {
        session_id,
        ts_ms,
        model,
        input,
        output,
        cache_create,
        cache_read,
        cost,
    }))
}

fn system_time_to_ns(t: Option<SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use std::io::Write;
    use tempfile::tempdir;

    /// Создать синтетическую структуру Claude Code projects:
    /// ~/.claude/projects/-project1/<uuid>.jsonl
    fn make_fixture(
        root: &Path,
        project_dir: &str,
        uuid: &str,
        lines: &[&str],
    ) -> std::path::PathBuf {
        let dir = root.join(project_dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{}.jsonl", uuid));
        let mut f = File::create(&file).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        file
    }

    fn assistant_line(uuid: &str, ts: &str, model: &str, input: u64, output: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": uuid,
            "timestamp": ts,
            "message": {
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn full_parse_inserts_turns() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "11111111-1111-1111-1111-111111111111";
        let _ = make_fixture(
            &projects,
            "-some-project",
            uuid,
            &[
                &assistant_line(uuid, "2026-05-18T20:00:00Z", "claude-sonnet-4-6", 100, 50),
                &assistant_line(uuid, "2026-05-18T20:01:00Z", "claude-sonnet-4-6", 200, 80),
            ],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let stats = index_all(&mut conn, &projects).unwrap();

        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_parsed_full, 1);
        assert_eq!(stats.turns_inserted, 2);

        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let watermark_count: u32 = conn
            .query_row("SELECT COUNT(*) FROM parsed_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(watermark_count, 1);
    }

    #[test]
    fn second_run_without_changes_skips() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "22222222-2222-2222-2222-222222222222";
        let _ = make_fixture(
            &projects,
            "-p",
            uuid,
            &[&assistant_line(
                uuid,
                "2026-05-18T20:00:00Z",
                "claude-sonnet-4-6",
                10,
                5,
            )],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let _ = index_all(&mut conn, &projects).unwrap();
        let stats2 = index_all(&mut conn, &projects).unwrap();

        assert_eq!(stats2.files_skipped, 1);
        assert_eq!(stats2.files_parsed_full, 0);
        assert_eq!(stats2.turns_inserted, 0);
    }

    #[test]
    fn incremental_append_parses_only_new_lines() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "33333333-3333-3333-3333-333333333333";
        let path = make_fixture(
            &projects,
            "-p",
            uuid,
            &[&assistant_line(
                uuid,
                "2026-05-18T20:00:00Z",
                "claude-sonnet-4-6",
                10,
                5,
            )],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let _ = index_all(&mut conn, &projects).unwrap();

        // Дописать ещё одну строку
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            f,
            "{}",
            assistant_line(uuid, "2026-05-18T20:01:00Z", "claude-sonnet-4-6", 20, 10)
        )
        .unwrap();
        drop(f);

        let stats2 = index_all(&mut conn, &projects).unwrap();
        assert_eq!(stats2.files_parsed_incremental, 1);
        assert_eq!(
            stats2.turns_inserted, 1,
            "должна быть проиндексирована только одна новая строка"
        );

        let total: u32 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn truncate_triggers_full_reparse() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "44444444-4444-4444-4444-444444444444";
        let path = make_fixture(
            &projects,
            "-p",
            uuid,
            &[
                &assistant_line(uuid, "2026-05-18T20:00:00Z", "claude-sonnet-4-6", 10, 5),
                &assistant_line(uuid, "2026-05-18T20:01:00Z", "claude-sonnet-4-6", 20, 10),
            ],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let _ = index_all(&mut conn, &projects).unwrap();

        // Truncate — оставить только одну строку (меньший размер)
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            assistant_line(uuid, "2026-05-18T20:05:00Z", "claude-sonnet-4-6", 50, 25)
        )
        .unwrap();
        drop(f);

        let stats2 = index_all(&mut conn, &projects).unwrap();
        assert_eq!(stats2.files_truncated_reparsed, 1);
        assert_eq!(stats2.turns_inserted, 1);

        let total: u32 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            total, 1,
            "после truncate старые turns должны быть удалены, остаётся только новая"
        );
    }

    #[test]
    fn invalid_json_line_counts_but_does_not_abort() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "55555555-5555-5555-5555-555555555555";
        let _ = make_fixture(
            &projects,
            "-p",
            uuid,
            &[
                &assistant_line(uuid, "2026-05-18T20:00:00Z", "claude-sonnet-4-6", 10, 5),
                "{this is not valid json",
                &assistant_line(uuid, "2026-05-18T20:01:00Z", "claude-sonnet-4-6", 20, 10),
            ],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let stats = index_all(&mut conn, &projects).unwrap();
        assert_eq!(stats.turns_inserted, 2);
        assert_eq!(stats.lines_invalid_json, 1);
    }

    #[test]
    fn non_assistant_events_are_skipped() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "66666666-6666-6666-6666-666666666666";
        let user_line = serde_json::json!({
            "type": "user",
            "sessionId": uuid,
            "timestamp": "2026-05-18T20:00:00Z",
            "message": {"content": [{"type": "text", "text": "hi"}]}
        })
        .to_string();
        let _ = make_fixture(
            &projects,
            "-p",
            uuid,
            &[
                &user_line,
                &assistant_line(uuid, "2026-05-18T20:00:30Z", "claude-sonnet-4-6", 10, 5),
            ],
        );

        let mut conn = open_index_at(&db_path).unwrap();
        let stats = index_all(&mut conn, &projects).unwrap();
        assert_eq!(
            stats.turns_inserted, 1,
            "только assistant event должен попасть в turns"
        );
    }

    #[test]
    fn ephemeral_cache_fields_are_summed() {
        let tmp = tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let db_path = tmp.path().join("idx.db");

        let uuid = "77777777-7777-7777-7777-777777777777";
        let event = serde_json::json!({
            "type": "assistant",
            "sessionId": uuid,
            "timestamp": "2026-05-18T20:00:00Z",
            "message": {
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 30,
                        "ephemeral_1h_input_tokens": 70
                    },
                    "cache_read_input_tokens": 200
                }
            }
        })
        .to_string();
        let _ = make_fixture(&projects, "-p", uuid, &[&event]);

        let mut conn = open_index_at(&db_path).unwrap();
        let _ = index_all(&mut conn, &projects).unwrap();

        let (cc, cr): (i64, i64) = conn
            .query_row(
                "SELECT tokens_cache_create, tokens_cache_read FROM turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cc, 100, "ephemeral_5m + ephemeral_1h = 30 + 70 = 100");
        assert_eq!(cr, 200);
    }
}
