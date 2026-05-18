//! Weekly rate-limit windows: окно [anchor + N*7d, anchor + (N+1)*7d).
//!
//! API:
//! - `anchor_ms()` — default 2026-05-15T12:00:00Z UTC, override через ENV.
//! - `window_for_ts(ts, anchor)` — окно для произвольного timestamp.
//! - `current_window(anchor)` — окно для сейчас.
//! - `week_id_sql_expr(anchor)` — SQL CASE для GROUP BY `week_id` напрямую в
//!   queries по `turns` (без materialized таблицы).

use chrono::{DateTime, Utc};
use serde::Serialize;

/// 2026-05-15 12:00 UTC — последний известный публичный rate-limit flush.
pub const DEFAULT_ANCHOR_ISO: &str = "2026-05-15T12:00:00Z";

/// Длина окна в миллисекундах (7 дней).
pub const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// ENV var для override anchor (формат ISO-8601 / RFC 3339).
pub const ANCHOR_ENV: &str = "CLAUDE_RESET_ANCHOR";

/// Текущий anchor в миллисекундах от Unix epoch.
pub fn anchor_ms() -> i64 {
    if let Ok(s) = std::env::var(ANCHOR_ENV) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
            return dt.with_timezone(&Utc).timestamp_millis();
        }
    }
    parse_anchor(DEFAULT_ANCHOR_ISO)
}

fn parse_anchor(iso: &str) -> i64 {
    DateTime::parse_from_rfc3339(iso)
        .expect("DEFAULT_ANCHOR_ISO должен быть валидным RFC 3339")
        .with_timezone(&Utc)
        .timestamp_millis()
}

/// Описание одного weekly window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WeeklyWindow {
    /// "W0", "W1", ... или "pre" для всего до anchor.
    pub id: String,
    /// Начало окна (включительно), миллисекунды Unix.
    pub start_ms: i64,
    /// Конец окна (исключительно), миллисекунды Unix.
    pub end_ms: i64,
    /// Anchor, относительно которого окно вычислено (для verify).
    pub anchor_ms: i64,
}

impl WeeklyWindow {
    /// Окно "pre" — всё до anchor. start = 0 (Unix epoch).
    pub fn pre(anchor_ms: i64) -> Self {
        WeeklyWindow {
            id: "pre".into(),
            start_ms: 0,
            end_ms: anchor_ms,
            anchor_ms,
        }
    }

    /// Окно с номером N (N >= 0).
    pub fn nth(n: i64, anchor_ms: i64) -> Self {
        let start = anchor_ms + n * WEEK_MS;
        WeeklyWindow {
            id: format!("W{}", n),
            start_ms: start,
            end_ms: start + WEEK_MS,
            anchor_ms,
        }
    }

    /// True если ts попадает в это окно.
    pub fn contains(&self, ts_ms: i64) -> bool {
        if self.id == "pre" {
            ts_ms < self.end_ms
        } else {
            ts_ms >= self.start_ms && ts_ms < self.end_ms
        }
    }

    /// Человеко-читаемая дата начала (UTC, YYYY-MM-DD).
    pub fn start_date_utc(&self) -> String {
        DateTime::<Utc>::from_timestamp_millis(self.start_ms)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "epoch".into())
    }
}

/// Окно для произвольного timestamp.
pub fn window_for_ts(ts_ms: i64, anchor_ms: i64) -> WeeklyWindow {
    if ts_ms < anchor_ms {
        return WeeklyWindow::pre(anchor_ms);
    }
    let n = (ts_ms - anchor_ms) / WEEK_MS;
    WeeklyWindow::nth(n, anchor_ms)
}

/// Окно для текущего момента времени.
pub fn current_window(anchor_ms: i64) -> WeeklyWindow {
    let now_ms = Utc::now().timestamp_millis();
    window_for_ts(now_ms, anchor_ms)
}

/// SQL-выражение, вычисляющее week_id из `ts_ms` колонки.
/// Используется в GROUP BY и WHERE без materialized таблицы.
///
/// Пример:
/// ```ignore
/// let expr = week_id_sql_expr(anchor_ms);
/// let sql = format!("SELECT {expr} AS week_id, SUM(cost_usd) FROM turns GROUP BY week_id");
/// ```
pub fn week_id_sql_expr(anchor_ms: i64) -> String {
    format!(
        "CASE WHEN ts_ms < {anchor} THEN 'pre' \
         ELSE 'W' || ((ts_ms - {anchor}) / {week}) END",
        anchor = anchor_ms,
        week = WEEK_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor() -> i64 {
        parse_anchor(DEFAULT_ANCHOR_ISO)
    }

    #[test]
    fn anchor_iso_parses() {
        let a = anchor();
        assert!(a > 1_700_000_000_000, "anchor должен быть после 2023 года");
    }

    #[test]
    fn pre_anchor_returns_pre() {
        let a = anchor();
        let w = window_for_ts(a - 1, a);
        assert_eq!(w.id, "pre");
        assert!(w.contains(a - 1));
        assert!(!w.contains(a));
    }

    #[test]
    fn at_anchor_is_w0() {
        let a = anchor();
        let w = window_for_ts(a, a);
        assert_eq!(w.id, "W0");
        assert_eq!(w.start_ms, a);
        assert_eq!(w.end_ms, a + WEEK_MS);
    }

    #[test]
    fn one_day_after_anchor_is_w0() {
        let a = anchor();
        let one_day_ms = 24 * 60 * 60 * 1000;
        let w = window_for_ts(a + one_day_ms, a);
        assert_eq!(w.id, "W0");
    }

    #[test]
    fn exactly_seven_days_after_anchor_is_w1() {
        let a = anchor();
        let w = window_for_ts(a + WEEK_MS, a);
        assert_eq!(w.id, "W1");
    }

    #[test]
    fn one_day_before_next_window_is_still_w0() {
        let a = anchor();
        let w = window_for_ts(a + WEEK_MS - 1, a);
        assert_eq!(w.id, "W0");
    }

    #[test]
    fn many_weeks_later() {
        let a = anchor();
        let w = window_for_ts(a + 100 * WEEK_MS + 1000, a);
        assert_eq!(w.id, "W100");
    }

    #[test]
    fn contains_is_half_open() {
        let a = anchor();
        let w = WeeklyWindow::nth(0, a);
        assert!(w.contains(a));
        assert!(w.contains(a + WEEK_MS - 1));
        assert!(!w.contains(a + WEEK_MS));
    }

    #[test]
    fn env_override_changes_anchor() {
        std::env::set_var(ANCHOR_ENV, "2025-01-01T00:00:00Z");
        let a = anchor_ms();
        let expected = parse_anchor("2025-01-01T00:00:00Z");
        assert_eq!(a, expected);
        std::env::remove_var(ANCHOR_ENV);
    }

    #[test]
    fn invalid_env_falls_back_to_default() {
        std::env::set_var(ANCHOR_ENV, "not a date");
        let a = anchor_ms();
        let expected = parse_anchor(DEFAULT_ANCHOR_ISO);
        assert_eq!(a, expected);
        std::env::remove_var(ANCHOR_ENV);
    }

    #[test]
    fn sql_expr_includes_anchor_and_week() {
        let a = anchor();
        let s = week_id_sql_expr(a);
        assert!(s.contains(&a.to_string()));
        assert!(s.contains(&WEEK_MS.to_string()));
        assert!(s.contains("'pre'"));
        assert!(s.contains("'W'"));
    }

    #[test]
    fn sql_expr_matches_window_for_ts() {
        // Verify что SQL даёт тот же результат как window_for_ts() для разных ts.
        use rusqlite::Connection;
        let a = anchor();
        let conn = Connection::open_in_memory().unwrap();
        let expr = week_id_sql_expr(a);

        for &offset in &[-1_i64, 0, 1, WEEK_MS - 1, WEEK_MS, WEEK_MS + 1, 5 * WEEK_MS] {
            let ts = a + offset;
            let sql = format!("SELECT {} FROM (SELECT {} AS ts_ms)", expr, ts);
            let sql_result: String = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
            let rust_result = window_for_ts(ts, a).id;
            assert_eq!(
                sql_result, rust_result,
                "несоответствие при offset={}: SQL={} rust={}",
                offset, sql_result, rust_result
            );
        }
    }
}
