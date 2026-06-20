//! Reconciliation: сопоставление local JSONL tokens с endpoint utilization %.
//!
//! Идея: для каждой пары соседних oauth snapshots вычислить:
//! 1. Δ utilization (5h и 7d)
//! 2. local_tokens — SUM(input+output) из turns за тот же интервал
//! 3. implied conversion (если local_tokens > 0 и Δ% > 0)
//! 4. drift — момент когда Δ% > 0, а local ≈ 0 (работа с другой машины)

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use super::history;

/// Один interval между snapshots с computed reconciliation метриками.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileInterval {
    pub from_ts_ms: i64,
    pub to_ts_ms: i64,
    pub duration_secs: i64,
    /// Δ utilization в этом интервале (per metric).
    pub delta_5h_pct: f64,
    pub delta_7d_pct: f64,
    /// Local input+output tokens из turns в этом интервале.
    pub local_tokens: i64,
    /// Сколько local tokens соответствует 1% endpoint Δ. None если Δ% ≤ 0.
    pub tokens_per_pct_7d: Option<f64>,
    /// True если Δ% > 0, но local_tokens очень мало (другая машина).
    pub drift: bool,
}

/// Aggregate отчёт.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcileReport {
    pub intervals: Vec<ReconcileInterval>,
    pub total_delta_7d_pct: f64,
    pub total_local_tokens: i64,
    /// Сколько интервалов с положительной Δ% использовались для среднего.
    pub samples_used: usize,
    /// Среднее (mean) tokens per 1% Δ.
    pub mean_tokens_per_pct: Option<f64>,
    /// % интервалов которые были drift (другие машины тратили).
    pub drift_share_pct: f64,
    /// cc-proxy наблюдений с failed/degraded запросами в окне (status≥400 / 0 / sse_error).
    /// Часть «drift» может объясняться нашими же failed-запросами (квота потрачена,
    /// local turn не создан), а не другими машинами. Эпик proxy-correlation.
    pub transport_error_obs: i64,
}

const DRIFT_TOKEN_THRESHOLD: i64 = 1000;
const DRIFT_PCT_THRESHOLD: f64 = 0.2;

/// Построить report.
pub fn compute(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    account: Option<&str>,
) -> Result<ReconcileReport> {
    let snapshots = history::list(conn, from_ms, to_ms, account, 10_000)?;
    let with_d = history::with_deltas(snapshots);

    let mut intervals = Vec::new();
    let mut total_delta_7d = 0.0;
    let mut total_local_tokens: i64 = 0;
    let mut tokens_per_pct_samples: Vec<f64> = Vec::new();
    let mut drift_count = 0usize;
    let mut nonzero_count = 0usize;

    for d in &with_d {
        let gap = match d.gap_secs {
            Some(g) if g > 0 => g,
            _ => continue, // первый snapshot не имеет prev
        };
        let prev_ts = d.snapshot.ts_ms - gap * 1000;
        let local_tokens = sum_local_tokens(conn, account, prev_ts, d.snapshot.ts_ms)?;

        let delta_5h = d.delta_5h.unwrap_or(0.0);
        let delta_7d = d.delta_7d.unwrap_or(0.0);

        let tokens_per_pct = if delta_7d > 0.0 && local_tokens > 0 {
            Some(local_tokens as f64 / delta_7d)
        } else {
            None
        };

        let drift = delta_7d > DRIFT_PCT_THRESHOLD && local_tokens < DRIFT_TOKEN_THRESHOLD;
        if drift {
            drift_count += 1;
        }
        if delta_7d > 0.0 {
            nonzero_count += 1;
        }
        if let Some(r) = tokens_per_pct {
            tokens_per_pct_samples.push(r);
        }

        total_delta_7d += delta_7d.max(0.0);
        total_local_tokens += local_tokens;

        intervals.push(ReconcileInterval {
            from_ts_ms: prev_ts,
            to_ts_ms: d.snapshot.ts_ms,
            duration_secs: gap,
            delta_5h_pct: delta_5h,
            delta_7d_pct: delta_7d,
            local_tokens,
            tokens_per_pct_7d: tokens_per_pct,
            drift,
        });
    }

    let mean_tokens_per_pct = if tokens_per_pct_samples.is_empty() {
        None
    } else {
        Some(tokens_per_pct_samples.iter().sum::<f64>() / tokens_per_pct_samples.len() as f64)
    };

    let drift_share_pct = if nonzero_count > 0 {
        (drift_count as f64 / nonzero_count as f64) * 100.0
    } else {
        0.0
    };

    // Transport-error drift: failed/degraded cc-proxy запросы в окне. proxy_observations
    // может отсутствовать на старых индексах — COALESCE через optional query.
    let transport_error_obs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM proxy_observations
             WHERE ts_ms BETWEEN ? AND ?
               AND (status >= 400 OR status = 0 OR sse_error IS NOT NULL)",
            rusqlite::params![from_ms, to_ms],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(ReconcileReport {
        intervals,
        total_delta_7d_pct: total_delta_7d,
        total_local_tokens,
        samples_used: tokens_per_pct_samples.len(),
        mean_tokens_per_pct,
        drift_share_pct,
        transport_error_obs,
    })
}

fn sum_local_tokens(
    conn: &Connection,
    account: Option<&str>,
    from_ms: i64,
    to_ms: i64,
) -> Result<i64> {
    let sum: Option<i64> = match account {
        Some(a) => conn.query_row(
            "SELECT SUM(tokens_input + tokens_output) FROM turns
             WHERE account_id = ? AND ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![a, from_ms, to_ms],
            |r| r.get(0),
        ),
        None => conn.query_row(
            "SELECT SUM(tokens_input + tokens_output) FROM turns
             WHERE ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![from_ms, to_ms],
            |r| r.get(0),
        ),
    }
    .unwrap_or(Some(0));
    Ok(sum.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use crate::usage_api::cache::seed_snapshot_for_test;
    use rusqlite::params;
    use tempfile::tempdir;

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        (dir, conn)
    }

    fn insert_account(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO accounts (id, plan) VALUES (?, 'Pro')",
            params![id],
        )
        .unwrap();
    }

    fn insert_turn(conn: &Connection, ts: i64, acct: &str, input: i64, output: i64) {
        conn.execute(
            "INSERT INTO turns (session_id, ts_ms, account_id, tokens_input, tokens_output, cost_usd)
             VALUES ('s', ?, ?, ?, ?, 0.001)",
            params![ts, acct, input, output],
        )
        .unwrap();
    }

    #[test]
    fn empty_returns_zero_report() {
        let (_d, c) = open();
        let r = compute(&c, 0, i64::MAX, None).unwrap();
        assert!(r.intervals.is_empty());
        assert_eq!(r.samples_used, 0);
        assert!(r.mean_tokens_per_pct.is_none());
    }

    #[test]
    fn computes_tokens_per_pct() {
        let (_d, c) = open();
        insert_account(&c, "acc");
        // snapshot1 @ 1000ms (5h=0, 7d=10)
        // snapshot2 @ 61_000ms (+60s) (5h=0, 7d=11)
        // 50k tokens между ними
        seed_snapshot_for_test(&c, 1_000, 0.0, 10.0).unwrap();
        seed_snapshot_for_test(&c, 61_000, 0.0, 11.0).unwrap();
        insert_turn(&c, 30_000, "acc", 30_000, 20_000);

        let r = compute(&c, 0, i64::MAX, None).unwrap();
        assert_eq!(r.intervals.len(), 1);
        let i = &r.intervals[0];
        assert!((i.delta_7d_pct - 1.0).abs() < 0.001);
        assert_eq!(i.local_tokens, 50_000);
        // 50000 tokens / 1% = 50000 tokens/%
        assert!((i.tokens_per_pct_7d.unwrap() - 50_000.0).abs() < 0.01);
        assert!(!i.drift);
    }

    #[test]
    fn detects_drift_when_endpoint_grows_without_local() {
        let (_d, c) = open();
        insert_account(&c, "acc");
        seed_snapshot_for_test(&c, 1_000, 0.0, 10.0).unwrap();
        seed_snapshot_for_test(&c, 61_000, 0.0, 12.0).unwrap();
        // No local tokens → drift event

        let r = compute(&c, 0, i64::MAX, None).unwrap();
        assert_eq!(r.intervals.len(), 1);
        assert!(r.intervals[0].drift);
        assert!(r.intervals[0].tokens_per_pct_7d.is_none());
        assert_eq!(r.drift_share_pct, 100.0);
    }

    #[test]
    fn mean_tokens_per_pct_averaged_over_samples() {
        let (_d, c) = open();
        insert_account(&c, "acc");
        seed_snapshot_for_test(&c, 1_000, 0.0, 10.0).unwrap();
        seed_snapshot_for_test(&c, 61_000, 0.0, 11.0).unwrap();
        seed_snapshot_for_test(&c, 121_000, 0.0, 12.0).unwrap();
        insert_turn(&c, 30_000, "acc", 10_000, 10_000); // 20k between s1-s2
        insert_turn(&c, 90_000, "acc", 30_000, 10_000); // 40k between s2-s3

        let r = compute(&c, 0, i64::MAX, None).unwrap();
        assert_eq!(r.samples_used, 2);
        // 20k / 1% = 20000, 40k / 1% = 40000, mean = 30000
        let mean = r.mean_tokens_per_pct.unwrap();
        assert!((mean - 30_000.0).abs() < 0.01);
    }

    #[test]
    fn zero_delta_not_counted_in_samples() {
        let (_d, c) = open();
        insert_account(&c, "acc");
        seed_snapshot_for_test(&c, 1_000, 0.0, 10.0).unwrap();
        seed_snapshot_for_test(&c, 61_000, 0.0, 10.0).unwrap(); // No change
        insert_turn(&c, 30_000, "acc", 1000, 1000);

        let r = compute(&c, 0, i64::MAX, None).unwrap();
        assert_eq!(r.intervals.len(), 1);
        assert!(r.intervals[0].tokens_per_pct_7d.is_none());
        assert_eq!(r.samples_used, 0);
    }
}
