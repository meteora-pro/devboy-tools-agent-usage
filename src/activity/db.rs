use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

use super::models::{AfkStatus, AwAfkEvent, AwBucket, AwWindowEvent};

/// Загрузить buckets из ActivityWatch БД
pub fn load_buckets(db_path: &Path) -> Result<Vec<AwBucket>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Не удалось открыть ActivityWatch БД: {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT key, id, type, hostname FROM bucketmodel"
    )?;

    let buckets = stmt
        .query_map([], |row| {
            Ok(AwBucket {
                key: row.get(0)?,
                id: row.get(1)?,
                bucket_type: row.get(2)?,
                hostname: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(buckets)
}

/// Загрузить события активного окна
pub fn load_window_events(
    db_path: &Path,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<AwWindowEvent>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Не удалось открыть ActivityWatch БД: {}", db_path.display()))?;

    // Находим bucket-ы с типом currentwindow
    let bucket_keys: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT key FROM bucketmodel WHERE type = 'currentwindow'"
        )?;
        let keys: Vec<i64> = stmt.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        keys
    };

    if bucket_keys.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders: String = bucket_keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT timestamp, duration, datastr FROM eventmodel \
         WHERE bucket_id IN ({}) {} \
         ORDER BY timestamp",
        placeholders,
        build_time_filter(from, to),
    );

    let mut stmt = conn.prepare(&query)?;

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = bucket_keys
        .iter()
        .map(|k| Box::new(*k) as Box<dyn rusqlite::ToSql>)
        .collect();

    if let Some(f) = from {
        params.push(Box::new(f.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string()));
    }
    if let Some(t) = to {
        params.push(Box::new(t.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string()));
    }

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let events = stmt
        .query_map(param_refs.as_slice(), |row| {
            let timestamp_str: String = row.get(0)?;
            let duration: f64 = row.get(1)?;
            let datastr: String = row.get(2)?;

            Ok((timestamp_str, duration, datastr))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(ts_str, duration, datastr)| {
            let timestamp = parse_aw_timestamp(&ts_str)?;
            let data: serde_json::Value = serde_json::from_str(&datastr).ok()?;

            Some(AwWindowEvent {
                timestamp,
                duration_secs: duration,
                app: data.get("app")?.as_str()?.to_string(),
                title: data.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect();

    Ok(events)
}

/// Загрузить события AFK статуса
pub fn load_afk_events(
    db_path: &Path,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<AwAfkEvent>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Не удалось открыть ActivityWatch БД: {}", db_path.display()))?;

    let bucket_keys: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT key FROM bucketmodel WHERE type = 'afkstatus'"
        )?;
        let keys: Vec<i64> = stmt.query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        keys
    };

    if bucket_keys.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders: String = bucket_keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT timestamp, duration, datastr FROM eventmodel \
         WHERE bucket_id IN ({}) {} \
         ORDER BY timestamp",
        placeholders,
        build_time_filter(from, to),
    );

    let mut stmt = conn.prepare(&query)?;

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = bucket_keys
        .iter()
        .map(|k| Box::new(*k) as Box<dyn rusqlite::ToSql>)
        .collect();

    if let Some(f) = from {
        params.push(Box::new(f.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string()));
    }
    if let Some(t) = to {
        params.push(Box::new(t.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string()));
    }

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let events = stmt
        .query_map(param_refs.as_slice(), |row| {
            let timestamp_str: String = row.get(0)?;
            let duration: f64 = row.get(1)?;
            let datastr: String = row.get(2)?;

            Ok((timestamp_str, duration, datastr))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(ts_str, duration, datastr)| {
            let timestamp = parse_aw_timestamp(&ts_str)?;
            let data: serde_json::Value = serde_json::from_str(&datastr).ok()?;
            let status_str = data.get("status")?.as_str()?;
            let status = match status_str {
                "afk" => AfkStatus::Afk,
                _ => AfkStatus::NotAfk,
            };

            Some(AwAfkEvent {
                timestamp,
                duration_secs: duration,
                status,
            })
        })
        .collect();

    Ok(events)
}

/// Построить SQL фильтр по времени
fn build_time_filter(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> String {
    match (from, to) {
        (Some(_), Some(_)) => " AND timestamp >= ? AND timestamp <= ?".to_string(),
        (Some(_), None) => " AND timestamp >= ?".to_string(),
        (None, Some(_)) => " AND timestamp <= ?".to_string(),
        (None, None) => String::new(),
    }
}

/// Парсинг timestamp из ActivityWatch
/// Формат: "2026-02-20 17:24:03.548000+00:00"
fn parse_aw_timestamp(s: &str) -> Option<DateTime<Utc>> {
    // Пробуем несколько форматов
    if let Ok(dt) = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%:z") {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return Some(dt.with_timezone(&Utc));
    }
    // Без timezone — считаем UTC
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    None
}
