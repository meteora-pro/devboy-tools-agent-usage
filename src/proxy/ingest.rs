//! Инкрементальный ingest cc-proxy JSONL → `proxy_observations`.
//!
//! Переиспользует watermark-машинерию `parsed_files` (mtime+size+offset), как
//! индексер транскриптов. Лог cc-proxy append-only; на ротации (>300 ГБ) файл
//! пересоздаётся → size меньше watermark → strategy=truncated → перечитываем с 0.
//! `INSERT OR IGNORE` (частичный UNIQUE по host+request_id) делает re-ingest идемпотентным.
//!
//! Берём ТОЛЬКО top-level скаляры — никаких тел запроса/ответа (PII/секреты).
//! Если лог отсутствует (mount не примонтирован) — skip, НЕ ошибка.

use std::fs::{metadata, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

#[derive(Default, Debug)]
pub struct ProxyIngestStats {
    pub present: bool,
    pub strategy: &'static str,
    pub rows_inserted: usize,
    pub rows_dup: usize,
    pub rows_skipped: usize,
    pub bad_json: usize,
}

impl ProxyIngestStats {
    pub fn summary(&self) -> String {
        if !self.present {
            return "proxy: log absent (skip)".to_string();
        }
        format!(
            "proxy: {} ins={} dup={} skip={} bad={}",
            self.strategy, self.rows_inserted, self.rows_dup, self.rows_skipped, self.bad_json
        )
    }
}

/// Путь к cc-proxy логу: `CC_PROXY_LOG` или дефолт на cold-360.
pub fn default_proxy_log() -> PathBuf {
    std::env::var("CC_PROXY_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt/cold-360/cc-proxy-logs/cc-proxy.jsonl"))
}

pub fn ingest_proxy_log(
    conn: &mut Connection,
    log_path: &Path,
    host: &str,
) -> Result<ProxyIngestStats> {
    let mut stats = ProxyIngestStats::default();

    let meta = match metadata(log_path) {
        Ok(m) => m,
        Err(_) => return Ok(stats), // mount absent → skip-not-error
    };
    stats.present = true;
    let current_mtime_ns = sys_ns(meta.modified().ok());
    let current_size = meta.len() as i64;
    let path_str = log_path.to_string_lossy().into_owned();

    let wm: Option<(i64, i64, i64)> = conn
        .query_row(
            "SELECT mtime_ns, size_bytes, last_offset FROM parsed_files WHERE path = ?",
            params![path_str],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let start_offset: u64 = match &wm {
        None => {
            stats.strategy = "full";
            0
        }
        Some((_, sz, _)) if current_size < *sz => {
            stats.strategy = "truncated"; // ротация лога
            0
        }
        Some((mt, sz, _)) if current_size == *sz && current_mtime_ns == *mt => {
            stats.strategy = "skip";
            return Ok(stats);
        }
        Some((_, _, off)) => {
            stats.strategy = "incremental";
            *off as u64
        }
    };

    let mut file = File::open(log_path).context("открытие cc-proxy лога")?;
    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset)).context("seek")?;
    }
    let reader = BufReader::new(file);

    let tx = conn.transaction()?;
    let mut offset = start_offset;
    {
        let mut ins = tx.prepare(
            "INSERT OR IGNORE INTO proxy_observations
             (host, request_id, session_id, ts_ms, wait_ms, dur_ms, inflight_at_start,
              status, sse_error, model, cache_read, cache_create)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )?;

        for line_res in reader.lines() {
            let line = match line_res {
                Ok(l) => l,
                Err(_) => break,
            };
            offset += line.len() as u64 + 1; // BufRead.lines() ест \n
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    stats.bad_json += 1;
                    continue;
                }
            };

            let request_id = v
                .get("request_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty());
            let session_id = v
                .get("session_id")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty());
            // connectivity-пробы (HEAD /) без обоих ключей — пропускаем
            if request_id.is_none() && session_id.is_none() {
                stats.rows_skipped += 1;
                continue;
            }

            let ts_ms = match v
                .get("ts")
                .and_then(|x| x.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            {
                Some(dt) => dt.with_timezone(&Utc).timestamp_millis(),
                None => {
                    stats.rows_skipped += 1;
                    continue;
                }
            };

            let usage = v.get("usage");
            let cache_read = usage
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(|x| x.as_i64());
            let cache_create = usage
                .and_then(|u| u.get("cache_creation_input_tokens"))
                .and_then(|x| x.as_i64());

            let n = ins.execute(params![
                host,
                request_id,
                session_id,
                ts_ms,
                v.get("wait_ms").and_then(|x| x.as_i64()),
                v.get("dur_ms").and_then(|x| x.as_i64()),
                v.get("inflight_at_start").and_then(|x| x.as_i64()),
                v.get("status").and_then(|x| x.as_i64()),
                v.get("sse_error").and_then(|x| x.as_str()),
                v.get("model").and_then(|x| x.as_str()),
                cache_read,
                cache_create,
            ])?;
            if n > 0 {
                stats.rows_inserted += 1;
            } else {
                stats.rows_dup += 1; // уже есть (idempotent re-ingest по request_id)
            }
        }
    }

    tx.execute(
        "INSERT INTO parsed_files (path, mtime_ns, size_bytes, last_offset, parsed_at, host)
         VALUES (?,?,?,?,datetime('now'),?)
         ON CONFLICT(path) DO UPDATE SET
            mtime_ns=excluded.mtime_ns, size_bytes=excluded.size_bytes,
            last_offset=excluded.last_offset, parsed_at=excluded.parsed_at",
        params![path_str, current_mtime_ns, current_size, offset as i64, host],
    )?;
    tx.commit()?;
    Ok(stats)
}

fn sys_ns(t: Option<SystemTime>) -> i64 {
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

    fn proxy_line(rid: Option<&str>, sid: &str, ts: &str, status: i64, wait: i64) -> String {
        let mut v = serde_json::json!({
            "ts": ts, "session_id": sid, "status": status, "wait_ms": wait,
            "dur_ms": 1234, "inflight_at_start": 2, "model": "claude-opus-4-8",
            "usage": {"cache_read_input_tokens": 100, "cache_creation_input_tokens": 0}
        });
        v["request_id"] = match rid {
            Some(r) => serde_json::Value::String(r.to_string()),
            None => serde_json::Value::Null,
        };
        v.to_string()
    }

    #[test]
    fn ingests_proxy_rows_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let log = dir.path().join("cc-proxy.jsonl");
        {
            let mut f = File::create(&log).unwrap();
            writeln!(f, "{}", proxy_line(Some("req_A"), "sid1", "2026-06-20T01:00:00Z", 200, 50)).unwrap();
            writeln!(f, "{}", proxy_line(None, "sid1", "2026-06-20T01:00:01Z", 502, 0)).unwrap(); // orphan
            writeln!(f, r#"{{"path":"/","status":404}}"#).unwrap(); // проба — skip
        }
        let mut conn = open_index_at(&db).unwrap();
        let s1 = ingest_proxy_log(&mut conn, &log, "local").unwrap();
        assert_eq!(s1.rows_inserted, 2, "req_A + orphan");
        assert_eq!(s1.rows_skipped, 1, "проба /");

        // повторный прогон без изменений — skip
        let s2 = ingest_proxy_log(&mut conn, &log, "local").unwrap();
        assert_eq!(s2.strategy, "skip");

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_observations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn absent_log_is_skip_not_error() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let mut conn = open_index_at(&db).unwrap();
        let s = ingest_proxy_log(&mut conn, &dir.path().join("nope.jsonl"), "local").unwrap();
        assert!(!s.present);
    }
}
