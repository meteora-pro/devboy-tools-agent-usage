//! Кеширующая обёртка над `client::fetch_usage`.
//!
//! Стратегия:
//! 1. Читаем последний snapshot из `oauth_usage_snapshots`.
//! 2. Если age <= ttl_secs → возвращаем cached без HTTP-запроса.
//! 3. Иначе fetch_usage(), INSERT в БД, return.
//! 4. При ошибке fetch (429, network) — fallback к latest snapshot если он
//!    есть (даже устаревший), иначе Err.
//!
//! Это защищает от rate-limit endpoint'а и даёт работоспособность даже
//! при кратковременной потере сети.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use super::client::{self, ExtraUsage, UsageApiError, UsageBucket, UsageResponse};

/// Источник данных в результате.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    /// Свежий fetch из endpoint'а.
    Fresh,
    /// Cached snapshot из БД (в пределах TTL).
    Cached,
    /// Cached snapshot за пределами TTL (но fetch упал, используем stale).
    Stale,
}

/// Результат `fetch_cached` — собственно usage + откуда он пришёл + ts_ms.
#[derive(Debug, Clone)]
pub struct CachedUsage {
    pub usage: UsageResponse,
    pub source: UsageSource,
    pub ts_ms: i64,
}

/// Получить usage с учётом cache.
///
/// `ttl_secs` = за какое время cached snapshot считается свежим.
/// `account_id` пишется в БД, может быть None если detect failed.
pub fn fetch_cached(
    conn: &Connection,
    ttl_secs: i64,
    account_id: Option<&str>,
) -> Result<CachedUsage> {
    let now_ms = Utc::now().timestamp_millis();
    let ttl_ms = ttl_secs * 1000;

    let latest = read_latest_snapshot(conn)?;

    // 1. Cache hit (свежий)?
    if let Some((cached_ts, cached_usage)) = latest.as_ref() {
        if now_ms - cached_ts <= ttl_ms {
            return Ok(CachedUsage {
                usage: cached_usage.clone(),
                source: UsageSource::Cached,
                ts_ms: *cached_ts,
            });
        }
    }

    // 2. Cache miss — пробуем fetch.
    match client::fetch_usage() {
        Ok(usage) => {
            // INSERT в БД
            insert_snapshot(conn, now_ms, account_id, &usage)?;
            Ok(CachedUsage {
                usage,
                source: UsageSource::Fresh,
                ts_ms: now_ms,
            })
        }
        Err(e) => {
            // 3. Fetch упал — fallback к stale cache если есть.
            if let Some((cached_ts, cached_usage)) = latest {
                eprintln!(
                    "Warning: usage fetch failed ({}), using stale cache age={}s",
                    e,
                    (now_ms - cached_ts) / 1000
                );
                Ok(CachedUsage {
                    usage: cached_usage,
                    source: UsageSource::Stale,
                    ts_ms: cached_ts,
                })
            } else {
                Err(anyhow::anyhow!(
                    "usage fetch failed and no cached snapshot available: {}",
                    e
                ))
            }
        }
    }
}

/// Прочитать последний snapshot из БД, восстановить как UsageResponse.
fn read_latest_snapshot(conn: &Connection) -> Result<Option<(i64, UsageResponse)>> {
    let row: Option<(
        i64,
        f64,
        Option<String>,
        f64,
        Option<String>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT ts_ms, five_hour_pct, five_hour_resets_at,
                    seven_day_pct, seven_day_resets_at,
                    seven_day_sonnet_pct, extra_used_credits,
                    extra_monthly_limit, extra_currency
             FROM oauth_usage_snapshots
             ORDER BY ts_ms DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2).ok(),
                    r.get(3)?,
                    r.get(4).ok(),
                    r.get(5).ok(),
                    r.get(6).ok(),
                    r.get(7).ok(),
                    r.get(8).ok(),
                ))
            },
        )
        .optional()?;

    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    let mut usage = UsageResponse::default();
    usage.five_hour = UsageBucket {
        utilization: row.1,
        resets_at: row.2,
    };
    usage.seven_day = UsageBucket {
        utilization: row.3,
        resets_at: row.4,
    };
    if let Some(s) = row.5 {
        usage.seven_day_sonnet = Some(UsageBucket {
            utilization: s,
            resets_at: None,
        });
    }
    if row.6.is_some() || row.7.is_some() {
        usage.extra_usage = Some(ExtraUsage {
            is_enabled: true,
            used_credits: row.6.unwrap_or(0.0),
            monthly_limit: row.7.unwrap_or(0.0),
            currency: row.8.unwrap_or_default(),
            utilization: None,
            disabled_reason: None,
        });
    }

    Ok(Some((row.0, usage)))
}

/// Записать snapshot. Public — используется при принудительной refresh
/// (например через CLI `usage --refresh`).
pub fn insert_snapshot(
    conn: &Connection,
    ts_ms: i64,
    account_id: Option<&str>,
    usage: &UsageResponse,
) -> Result<()> {
    let extra = usage.extra_usage.as_ref();
    conn.execute(
        "INSERT INTO oauth_usage_snapshots
         (ts_ms, account_id, five_hour_pct, five_hour_resets_at,
          seven_day_pct, seven_day_resets_at, seven_day_sonnet_pct,
          extra_used_credits, extra_monthly_limit, extra_currency)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            ts_ms,
            account_id,
            usage.five_hour.utilization,
            usage.five_hour.resets_at,
            usage.seven_day.utilization,
            usage.seven_day.resets_at,
            usage.seven_day_sonnet.as_ref().map(|s| s.utilization),
            extra.map(|e| e.used_credits),
            extra.map(|e| e.monthly_limit),
            extra.map(|e| e.currency.clone()),
        ],
    )?;
    Ok(())
}

/// Удобный helper: получить только utilization% для statusline без полной
/// UsageResponse.
pub fn current_pcts(
    conn: &Connection,
    ttl_secs: i64,
    account_id: Option<&str>,
) -> Result<(f64, f64, UsageSource)> {
    let c = fetch_cached(conn, ttl_secs, account_id)?;
    Ok((
        c.usage.five_hour.utilization,
        c.usage.seven_day.utilization,
        c.source,
    ))
}

// Используется только для unit-тестов: позволяет затолкать произвольный
// snapshot в БД (имитация cache state без HTTP).
#[cfg(test)]
pub fn seed_snapshot_for_test(
    conn: &Connection,
    ts_ms: i64,
    five_pct: f64,
    seven_pct: f64,
) -> Result<()> {
    let mut u = UsageResponse::default();
    u.five_hour.utilization = five_pct;
    u.seven_day.utilization = seven_pct;
    insert_snapshot(conn, ts_ms, None, &u)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use tempfile::tempdir;

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn read_empty_returns_none() {
        let (_d, c) = open();
        assert!(read_latest_snapshot(&c).unwrap().is_none());
    }

    #[test]
    fn insert_then_read_roundtrip() {
        let (_d, c) = open();
        let mut u = UsageResponse::default();
        u.five_hour.utilization = 17.5;
        u.five_hour.resets_at = Some("2026-05-19T10:00:00Z".to_string());
        u.seven_day.utilization = 44.0;
        u.seven_day.resets_at = Some("2026-05-23T04:00:00Z".to_string());
        u.seven_day_sonnet = Some(UsageBucket {
            utilization: 5.0,
            resets_at: None,
        });
        u.extra_usage = Some(ExtraUsage {
            is_enabled: true,
            monthly_limit: 2000.0,
            used_credits: 123.45,
            currency: "EUR".into(),
            utilization: None,
            disabled_reason: None,
        });

        c.execute("INSERT INTO accounts (id, plan) VALUES ('acc', 'Pro')", [])
            .unwrap();
        insert_snapshot(&c, 1_700_000_000_000, Some("acc"), &u).unwrap();
        let (ts, restored) = read_latest_snapshot(&c).unwrap().unwrap();
        assert_eq!(ts, 1_700_000_000_000);
        assert_eq!(restored.five_hour.utilization, 17.5);
        assert_eq!(restored.seven_day.utilization, 44.0);
        assert_eq!(restored.seven_day_sonnet.unwrap().utilization, 5.0);
        let extra = restored.extra_usage.unwrap();
        assert_eq!(extra.monthly_limit, 2000.0);
        assert_eq!(extra.used_credits, 123.45);
        assert_eq!(extra.currency, "EUR");
    }

    #[test]
    fn latest_returns_most_recent() {
        let (_d, c) = open();
        seed_snapshot_for_test(&c, 1000, 5.0, 10.0).unwrap();
        seed_snapshot_for_test(&c, 2000, 6.0, 11.0).unwrap();
        seed_snapshot_for_test(&c, 1500, 99.0, 99.0).unwrap();
        let (ts, u) = read_latest_snapshot(&c).unwrap().unwrap();
        assert_eq!(ts, 2000);
        assert_eq!(u.five_hour.utilization, 6.0);
    }
}
