//! Расчёт usage % внутри weekly window с учётом ceiling плана.
//!
//! Используемые токены = `input + output` (без cache_read/cache_create —
//! Anthropic не считает кэш в weekly rate limit). Cache отображается
//! отдельно как побочная информация.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::weekly::{self, WeeklyWindow};
use crate::account::plan::Plan;

/// Использование недельного бюджета конкретного аккаунта.
#[derive(Debug, Clone, Serialize)]
pub struct WeeklyUsage {
    pub account_id: String,
    pub plan: Plan,
    pub window: WeeklyWindow,
    /// Только `input + output`, без cache_*.
    pub used_tokens: i64,
    /// Дополнительно — cache_create + cache_read (для информации).
    pub cache_tokens: i64,
    /// Полный turn count.
    pub turns: i64,
    /// Cost в USD (по нашей simplified модели).
    pub cost_usd: f64,
    /// Потолок (None для Unknown plan).
    pub ceiling: Option<u64>,
    /// Процент использования (None если ceiling неизвестен).
    pub percent: Option<f64>,
}

/// Вычислить использование для конкретного account + window.
pub fn usage_for(conn: &Connection, account_id: &str, window: WeeklyWindow) -> Result<WeeklyUsage> {
    let plan_str: Option<String> = conn
        .query_row(
            "SELECT plan FROM accounts WHERE id = ?",
            params![account_id],
            |r| r.get(0),
        )
        .optional()?;
    let plan = plan_str
        .as_deref()
        .map(Plan::parse)
        .unwrap_or(Plan::Unknown);

    let row: (Option<i64>, Option<i64>, Option<i64>, Option<f64>) = conn.query_row(
        "SELECT
             SUM(tokens_input + tokens_output) AS used,
             SUM(tokens_cache_create + tokens_cache_read) AS cache,
             COUNT(*) AS turns,
             SUM(cost_usd) AS cost
         FROM turns
         WHERE account_id = ? AND ts_ms >= ? AND ts_ms < ?",
        params![account_id, window.start_ms, window.end_ms],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    let used = row.0.unwrap_or(0);
    let cache = row.1.unwrap_or(0);
    let turns = row.2.unwrap_or(0);
    let cost = row.3.unwrap_or(0.0);

    let ceiling = plan.weekly_token_ceiling();
    let percent = ceiling.map(|c| {
        if c == 0 {
            0.0
        } else {
            (used as f64 / c as f64) * 100.0
        }
    });

    Ok(WeeklyUsage {
        account_id: account_id.to_string(),
        plan,
        window,
        used_tokens: used,
        cache_tokens: cache,
        turns,
        cost_usd: cost,
        ceiling,
        percent,
    })
}

/// Вычислить usage для текущего активного окна аккаунта.
pub fn current_usage(conn: &Connection, account_id: &str) -> Result<WeeklyUsage> {
    let anchor = weekly::anchor_ms();
    let window = weekly::current_window(anchor);
    usage_for(conn, account_id, window)
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

    fn insert_account(conn: &Connection, id: &str, plan: Plan) {
        conn.execute(
            "INSERT INTO accounts (id, plan) VALUES (?, ?)",
            params![id, plan.as_str()],
        )
        .unwrap();
    }

    fn insert_turn(conn: &Connection, ts: i64, account: &str, input: i64, output: i64) {
        conn.execute(
            "INSERT INTO turns (session_id, ts_ms, account_id, tokens_input, tokens_output, cost_usd)
             VALUES (?, ?, ?, ?, ?, ?)",
            params!["s", ts, account, input, output, 0.001],
        )
        .unwrap();
    }

    #[test]
    fn empty_db_zero_usage() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Pro);
        let anchor = weekly::anchor_ms();
        let win = WeeklyWindow::nth(0, anchor);
        let u = usage_for(&c, "a", win).unwrap();
        assert_eq!(u.used_tokens, 0);
        assert_eq!(u.turns, 0);
        assert_eq!(u.percent, Some(0.0));
    }

    #[test]
    fn within_ceiling_percent_below_100() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Pro);
        let anchor = weekly::anchor_ms();
        // 22 миллиона tokens (половина Pro ceiling = 44M)
        insert_turn(&c, anchor + 1000, "a", 10_000_000, 12_000_000);

        let win = WeeklyWindow::nth(0, anchor);
        let u = usage_for(&c, "a", win).unwrap();
        assert_eq!(u.used_tokens, 22_000_000);
        assert_eq!(u.ceiling, Some(44_000_000));
        assert!((u.percent.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn over_ceiling_shows_percent_above_100() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Pro);
        let anchor = weekly::anchor_ms();
        // 88M tokens (200% от Pro 44M)
        insert_turn(&c, anchor + 1000, "a", 40_000_000, 48_000_000);

        let win = WeeklyWindow::nth(0, anchor);
        let u = usage_for(&c, "a", win).unwrap();
        assert!(u.percent.unwrap() > 100.0, "ожидался overflow >100%");
        assert!((u.percent.unwrap() - 200.0).abs() < 0.01);
    }

    #[test]
    fn unknown_plan_returns_none_percent() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Unknown);
        let anchor = weekly::anchor_ms();
        insert_turn(&c, anchor + 1000, "a", 100, 50);

        let win = WeeklyWindow::nth(0, anchor);
        let u = usage_for(&c, "a", win).unwrap();
        assert_eq!(u.used_tokens, 150);
        assert!(u.ceiling.is_none());
        assert!(u.percent.is_none());
    }

    #[test]
    fn filters_by_window_bounds() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Max20);
        let anchor = weekly::anchor_ms();
        let week = 7 * 24 * 60 * 60 * 1000;

        // По 100 tokens в окнах W0, W1, W2
        insert_turn(&c, anchor + 100, "a", 100, 0);
        insert_turn(&c, anchor + week + 100, "a", 200, 0);
        insert_turn(&c, anchor + 2 * week + 100, "a", 400, 0);

        let u0 = usage_for(&c, "a", WeeklyWindow::nth(0, anchor)).unwrap();
        let u1 = usage_for(&c, "a", WeeklyWindow::nth(1, anchor)).unwrap();
        let u2 = usage_for(&c, "a", WeeklyWindow::nth(2, anchor)).unwrap();

        assert_eq!(u0.used_tokens, 100);
        assert_eq!(u1.used_tokens, 200);
        assert_eq!(u2.used_tokens, 400);
    }

    #[test]
    fn cache_tokens_counted_separately() {
        let (_d, c) = open();
        insert_account(&c, "a", Plan::Pro);
        let anchor = weekly::anchor_ms();
        c.execute(
            "INSERT INTO turns (session_id, ts_ms, account_id,
                tokens_input, tokens_output, tokens_cache_create, tokens_cache_read, cost_usd)
             VALUES ('s', ?, 'a', 100, 50, 5000, 50000, 0.001)",
            params![anchor + 1000],
        )
        .unwrap();

        let u = usage_for(&c, "a", WeeklyWindow::nth(0, anchor)).unwrap();
        assert_eq!(u.used_tokens, 150, "used = input + output");
        assert_eq!(u.cache_tokens, 55_000, "cache = create + read");
    }

    #[test]
    fn missing_account_treated_as_unknown_plan() {
        let (_d, c) = open();
        // accounts table пустая
        // turn пишем напрямую (FK отключим для теста — или через unknown account)
        c.execute("PRAGMA foreign_keys=OFF", []).unwrap();
        let anchor = weekly::anchor_ms();
        c.execute(
            "INSERT INTO turns (session_id, ts_ms, account_id, tokens_input, tokens_output, cost_usd)
             VALUES ('s', ?, 'phantom', 100, 50, 0.0)",
            params![anchor + 1000],
        )
        .unwrap();

        let u = usage_for(&c, "phantom", WeeklyWindow::nth(0, anchor)).unwrap();
        assert_eq!(u.used_tokens, 150);
        assert_eq!(u.plan, Plan::Unknown);
        assert!(u.percent.is_none());
    }
}
