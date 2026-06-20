//! Корреляция turns ⋈ proxy_observations.
//!
//! Базовый джойн живёт во view `v_turns_proxy` (LEFT JOIN, schema v8). Здесь —
//! helper'ы поверх него: honesty-метрика `proxy_coverage_pct` (доля турнов с
//! джойном — статы транспорта смещены к проксированным сессиям, это надо показывать).

use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug, Clone, Copy)]
pub struct Coverage {
    pub total_turns: i64,
    pub matched: i64,
}

impl Coverage {
    pub fn pct(&self) -> f64 {
        if self.total_turns == 0 {
            0.0
        } else {
            100.0 * self.matched as f64 / self.total_turns as f64
        }
    }
}

/// Доля турнов в окне [from_ms, to_ms], у которых есть джойн с proxy_observations.
/// Окно None → весь диапазон.
pub fn coverage(conn: &Connection, from_ms: Option<i64>, to_ms: Option<i64>) -> Result<Coverage> {
    let from = from_ms.unwrap_or(i64::MIN);
    let to = to_ms.unwrap_or(i64::MAX);
    let (total, matched): (i64, i64) = conn.query_row(
        "SELECT
            COUNT(*),
            SUM(CASE WHEN p.request_id IS NOT NULL THEN 1 ELSE 0 END)
         FROM turns t
         LEFT JOIN proxy_observations p
                ON t.host = p.host AND t.request_id = p.request_id
         WHERE t.ts_ms BETWEEN ? AND ?",
        params![from, to],
        |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    )?;
    Ok(Coverage {
        total_turns: total,
        matched,
    })
}

/// Сколько proxy-наблюдений в БД (для диагностики).
pub fn proxy_obs_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM proxy_observations", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use tempfile::tempdir;

    #[test]
    fn coverage_left_join_counts_matched() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        // 2 турна: один с request_id который есть в proxy, другой без джойна.
        conn.execute(
            "INSERT INTO turns (session_id, request_id, ts_ms, host) VALUES ('s','req_X',1000,'local')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns (session_id, request_id, ts_ms, host) VALUES ('s','req_Y',2000,'local')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO proxy_observations (host, request_id, session_id, ts_ms) VALUES ('local','req_X','s',1000)",
            [],
        )
        .unwrap();

        let c = coverage(&conn, None, None).unwrap();
        assert_eq!(c.total_turns, 2);
        assert_eq!(c.matched, 1);
        assert_eq!(c.pct() as i64, 50);
    }
}
