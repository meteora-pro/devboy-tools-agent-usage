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

/// TTL prompt-кэша Anthropic (5-мин ephemeral). Assumption, не контракт.
const CACHE_TTL_MS: i64 = 300_000;
/// Множители кэша: read 0.1x, write/miss 1.25x базы входа → потеря = 1.15x за токен.
const CACHE_MISS_DELTA: f64 = 1.25 - 0.10;

/// Цена входного токена ($/токен) по модели (грубо; для оценки re-cache$).
fn input_price_per_tok(model: &str) -> f64 {
    let per_mtok = if model.contains("opus") {
        15.0
    } else if model.contains("haiku") {
        0.80
    } else {
        3.0 // sonnet и прочее
    };
    per_mtok / 1_000_000.0
}

#[derive(Default, Debug)]
pub struct TransportReport {
    pub coverage: Option<Coverage>,
    pub n_obs: i64,
    // latency / queue
    pub wait_p50: i64,
    pub wait_p95: i64,
    pub wait_max: i64,
    pub dur_p50: i64,
    pub dur_p95: i64,
    pub sum_wait_ms: i64,
    pub sum_dur_ms: i64,
    // concurrency
    pub peak_inflight: i64,
    // errors
    pub auth_errors: i64,        // 401/403
    pub upstream_errors: i64,    // 429/5xx
    pub hidden_overload: i64,    // status 200 + sse_error
    pub orphan_errors: i64,      // request_id NULL + error status/sse_error (ретраи)
    // re-cache (флагман)
    pub recache_events: i64,
    pub recache_cost_usd: f64,
    pub recache_queue_attributable_usd: f64,
}

fn pct(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Полный транспортный отчёт по окну [from_ms, to_ms].
pub fn transport_report(conn: &Connection, from_ms: i64, to_ms: i64) -> Result<TransportReport> {
    let mut rep = TransportReport {
        coverage: coverage(conn, Some(from_ms), Some(to_ms)).ok(),
        ..Default::default()
    };

    // Тянем наблюдения окна, упорядоченные по сессии+времени (для re-cache).
    let mut stmt = conn.prepare(
        "SELECT session_id, request_id, ts_ms, wait_ms, dur_ms, inflight_at_start,
                status, sse_error, model, cache_read, cache_create
         FROM proxy_observations
         WHERE ts_ms BETWEEN ? AND ?
         ORDER BY session_id, ts_ms",
    )?;
    struct Obs {
        session: String,
        ts: i64,
        wait: i64,
        dur: i64,
        inflight: i64,
        status: i64,
        sse_error: bool,
        has_rid: bool,
        model: String,
        cache_read: i64,
        cache_create: i64,
    }
    let rows: Vec<Obs> = stmt
        .query_map(params![from_ms, to_ms], |r| {
            Ok(Obs {
                session: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                has_rid: r.get::<_, Option<String>>(1)?.is_some(),
                ts: r.get(2)?,
                wait: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                dur: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                inflight: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                status: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                sse_error: r.get::<_, Option<String>>(7)?.is_some(),
                model: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
                cache_read: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                cache_create: r.get::<_, Option<i64>>(10)?.unwrap_or(0),
            })
        })?
        .filter_map(std::result::Result::ok)
        .collect();

    rep.n_obs = rows.len() as i64;
    let mut waits: Vec<i64> = Vec::new();
    let mut durs: Vec<i64> = Vec::new();

    for o in &rows {
        waits.push(o.wait);
        durs.push(o.dur);
        rep.sum_wait_ms += o.wait;
        rep.sum_dur_ms += o.dur;
        rep.peak_inflight = rep.peak_inflight.max(o.inflight);
        if matches!(o.status, 401 | 403) {
            rep.auth_errors += 1;
        }
        if o.status == 429 || (500..600).contains(&o.status) {
            rep.upstream_errors += 1;
        }
        if o.status == 200 && o.sse_error {
            rep.hidden_overload += 1;
        }
        if !o.has_rid && (o.sse_error || o.status == 0 || o.status >= 400) {
            rep.orphan_errors += 1;
        }
    }
    waits.sort_unstable();
    durs.sort_unstable();
    rep.wait_p50 = pct(&waits, 0.50);
    rep.wait_p95 = pct(&waits, 0.95);
    rep.wait_max = waits.last().copied().unwrap_or(0);
    rep.dur_p50 = pct(&durs, 0.50);
    rep.dur_p95 = pct(&durs, 0.95);

    // Re-cache: для последовательных запросов одной сессии — если кэш был тёплым
    // (prev.cache_read>0), а cur пересоздал кэш (cache_create>0) после паузы,
    // превысившей TTL (с учётом нашего wait_ms) → потеря.
    for win in rows.windows(2) {
        let (prev, cur) = (&win[0], &win[1]);
        if prev.session != cur.session {
            continue;
        }
        let gap = cur.ts - prev.ts;
        let effective_gap = gap + cur.wait;
        if effective_gap > CACHE_TTL_MS && cur.cache_create > 0 && prev.cache_read > 0 {
            let waste = cur.cache_create as f64 * CACHE_MISS_DELTA * input_price_per_tok(&cur.model);
            rep.recache_events += 1;
            rep.recache_cost_usd += waste;
            if effective_gap > 0 {
                rep.recache_queue_attributable_usd += waste * cur.wait as f64 / effective_gap as f64;
            }
        }
    }

    Ok(rep)
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
