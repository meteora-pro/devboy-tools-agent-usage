//! Недельные окна (rolling 7-day rate limit cycle) и лимиты по планам.
//!
//! Anthropic не публикует точные часы reset, но известно, что 15 мая 2026
//! был глобальный flush. Anchor по умолчанию — 2026-05-15 12:00 UTC.
//! Override через ENV `CLAUDE_RESET_ANCHOR=<ISO-8601>`.
//!
//! Окна нумеруются как `W0`, `W1`, ... начиная от anchor. Turns раньше
//! anchor попадают в bucket `pre`. Materializing таблицу не нужно — id
//! вычисляется на лету через SQL CASE для дешёвого GROUP BY.

pub mod engine;
pub mod weekly;
