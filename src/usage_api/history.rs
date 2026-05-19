//! Queries по таблице `oauth_usage_snapshots` для построения time-series
//! и расчёта delta между snapshots.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Один snapshot из истории.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRow {
    pub id: i64,
    pub ts_ms: i64,
    pub account_id: Option<String>,
    pub five_hour_pct: f64,
    pub seven_day_pct: f64,
    pub seven_day_sonnet_pct: Option<f64>,
}

/// Snapshot + delta к предыдущему (Δ utilization за интервал).
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotWithDelta {
    pub snapshot: SnapshotRow,
    /// Δ five_hour за прошедший интервал (None для первого).
    pub delta_5h: Option<f64>,
    /// Δ seven_day за прошедший интервал.
    pub delta_7d: Option<f64>,
    /// Прошедшие секунды между этим и предыдущим snapshot.
    pub gap_secs: Option<i64>,
}

/// Прочитать snapshot rows в диапазоне ts.
pub fn list(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    account: Option<&str>,
    limit: usize,
) -> Result<Vec<SnapshotRow>> {
    let mut conds = vec!["ts_ms >= ?".to_string(), "ts_ms < ?".to_string()];
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(from_ms), Box::new(to_ms)];
    if let Some(a) = account {
        conds.push("account_id = ?".to_string());
        binds.push(Box::new(a.to_string()));
    }
    let where_clause = format!("WHERE {}", conds.join(" AND "));

    let sql = format!(
        "SELECT id, ts_ms, account_id, five_hour_pct, seven_day_pct, seven_day_sonnet_pct
         FROM oauth_usage_snapshots
         {}
         ORDER BY ts_ms ASC
         LIMIT ?",
        where_clause
    );
    binds.push(Box::new(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows: Vec<SnapshotRow> = stmt
        .query_map(rusqlite::params_from_iter(bind_refs.iter().copied()), |r| {
            Ok(SnapshotRow {
                id: r.get(0)?,
                ts_ms: r.get(1)?,
                account_id: r.get(2).ok(),
                five_hour_pct: r.get(3)?,
                seven_day_pct: r.get(4)?,
                seven_day_sonnet_pct: r.get(5).ok(),
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Обогатить список snapshots delta-values относительно предыдущего snapshot.
pub fn with_deltas(snapshots: Vec<SnapshotRow>) -> Vec<SnapshotWithDelta> {
    let mut out = Vec::with_capacity(snapshots.len());
    for i in 0..snapshots.len() {
        let s = &snapshots[i];
        let (delta_5h, delta_7d, gap) = if i == 0 {
            (None, None, None)
        } else {
            let p = &snapshots[i - 1];
            (
                Some(s.five_hour_pct - p.five_hour_pct),
                Some(s.seven_day_pct - p.seven_day_pct),
                Some((s.ts_ms - p.ts_ms) / 1000),
            )
        };
        out.push(SnapshotWithDelta {
            snapshot: s.clone(),
            delta_5h,
            delta_7d,
            gap_secs: gap,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use crate::usage_api::cache::seed_snapshot_for_test;
    use tempfile::tempdir;

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn list_empty_returns_empty() {
        let (_d, c) = open();
        assert!(list(&c, 0, i64::MAX, None, 100).unwrap().is_empty());
    }

    #[test]
    fn list_returns_chronological_order() {
        let (_d, c) = open();
        seed_snapshot_for_test(&c, 3000, 30.0, 60.0).unwrap();
        seed_snapshot_for_test(&c, 1000, 10.0, 40.0).unwrap();
        seed_snapshot_for_test(&c, 2000, 20.0, 50.0).unwrap();

        let rows = list(&c, 0, i64::MAX, None, 100).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].ts_ms, 1000);
        assert_eq!(rows[1].ts_ms, 2000);
        assert_eq!(rows[2].ts_ms, 3000);
    }

    #[test]
    fn list_respects_time_range() {
        let (_d, c) = open();
        seed_snapshot_for_test(&c, 1000, 10.0, 40.0).unwrap();
        seed_snapshot_for_test(&c, 2000, 20.0, 50.0).unwrap();
        seed_snapshot_for_test(&c, 3000, 30.0, 60.0).unwrap();

        let rows = list(&c, 1500, 2500, None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts_ms, 2000);
    }

    #[test]
    fn with_deltas_first_has_none() {
        let (_d, c) = open();
        seed_snapshot_for_test(&c, 1000, 10.0, 40.0).unwrap();
        let rows = list(&c, 0, i64::MAX, None, 100).unwrap();
        let d = with_deltas(rows);
        assert_eq!(d.len(), 1);
        assert!(d[0].delta_5h.is_none());
        assert!(d[0].delta_7d.is_none());
        assert!(d[0].gap_secs.is_none());
    }

    #[test]
    fn with_deltas_computes_correctly() {
        let (_d, c) = open();
        seed_snapshot_for_test(&c, 1000, 10.0, 40.0).unwrap();
        seed_snapshot_for_test(&c, 61_000, 12.5, 41.5).unwrap(); // +60 сек
        seed_snapshot_for_test(&c, 121_000, 18.0, 44.0).unwrap(); // +60 сек

        let rows = list(&c, 0, i64::MAX, None, 100).unwrap();
        let d = with_deltas(rows);
        assert_eq!(d.len(), 3);
        assert!(d[0].delta_5h.is_none());
        assert!((d[1].delta_5h.unwrap() - 2.5).abs() < 0.01);
        assert!((d[1].delta_7d.unwrap() - 1.5).abs() < 0.01);
        assert_eq!(d[1].gap_secs, Some(60));
        assert!((d[2].delta_5h.unwrap() - 5.5).abs() < 0.01);
    }
}
