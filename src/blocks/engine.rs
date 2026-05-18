//! Алгоритм построения 5h блоков из turns.
//!
//! Семантика (как у Anthropic):
//! - Блок начинается с первого turn (start_ms = ts_ms).
//! - Блок длится ровно 5 часов: end_ms = start_ms + 5h.
//! - Turn с ts_ms >= block.end_ms → закрытие текущего, новый блок от ts.
//! - Внутри блока все turns суммируются.
//!
//! Active block: now < block.end_ms.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;

/// Длина блока в миллисекундах (5 часов).
pub const BLOCK_MS: i64 = 5 * 60 * 60 * 1000;

/// Один 5h-блок: агрегаты + время.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    pub is_active: bool,
    pub account_id: Option<String>,
    pub turns: i64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_create: i64,
    pub tokens_cache_read: i64,
    pub cost_usd: f64,
    /// Burn rate (input+output) tokens per minute. None если elapsed < 1 минуты.
    pub burn_rate_tpm: Option<f64>,
}

impl Block {
    /// Прошедшее время с начала блока (в минутах).
    pub fn elapsed_minutes(&self, now_ms: i64) -> f64 {
        let end_for_elapsed = if self.is_active { now_ms } else { self.end_ms };
        ((end_for_elapsed - self.start_ms).max(0) as f64) / 60_000.0
    }
}

/// Опции фильтрации для `build_blocks`.
#[derive(Debug, Default, Clone)]
pub struct BlockFilter<'a> {
    pub account_id: Option<&'a str>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
}

/// Построить блоки из turns. Возвращает их в хронологическом порядке.
pub fn build_blocks(conn: &Connection, filter: &BlockFilter) -> Result<Vec<Block>> {
    let now_ms = Utc::now().timestamp_millis();
    build_blocks_at(conn, filter, now_ms)
}

/// Версия для тестов: фиксирует "сейчас" аргументом.
pub fn build_blocks_at(conn: &Connection, filter: &BlockFilter, now_ms: i64) -> Result<Vec<Block>> {
    // Собираем WHERE предикат.
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(acct) = filter.account_id {
        conds.push("account_id = ?".to_string());
        binds.push(Box::new(acct.to_string()));
    }
    if let Some(from) = filter.from_ms {
        conds.push("ts_ms >= ?".to_string());
        binds.push(Box::new(from));
    }
    if let Some(to) = filter.to_ms {
        conds.push("ts_ms < ?".to_string());
        binds.push(Box::new(to));
    }
    let where_clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };

    let sql = format!(
        "SELECT ts_ms, account_id, tokens_input, tokens_output,
                tokens_cache_create, tokens_cache_read, cost_usd
         FROM turns
         {where_clause}
         ORDER BY ts_ms ASC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs.iter().copied()), |r| {
        Ok(TurnRow {
            ts_ms: r.get(0)?,
            account_id: r.get(1).ok(),
            tokens_input: r.get(2)?,
            tokens_output: r.get(3)?,
            tokens_cache_create: r.get(4)?,
            tokens_cache_read: r.get(5)?,
            cost_usd: r.get(6)?,
        })
    })?;

    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;

    for row in rows {
        let t = row?;
        // Решаем — расширяем текущий блок или открываем новый.
        let need_new = match &current {
            None => true,
            Some(b) => t.ts_ms >= b.end_ms,
        };
        if need_new {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            current = Some(Block {
                start_ms: t.ts_ms,
                end_ms: t.ts_ms + BLOCK_MS,
                is_active: false,
                account_id: t.account_id.clone(),
                turns: 0,
                tokens_input: 0,
                tokens_output: 0,
                tokens_cache_create: 0,
                tokens_cache_read: 0,
                cost_usd: 0.0,
                burn_rate_tpm: None,
            });
        }
        let b = current.as_mut().unwrap();
        b.turns += 1;
        b.tokens_input += t.tokens_input;
        b.tokens_output += t.tokens_output;
        b.tokens_cache_create += t.tokens_cache_create;
        b.tokens_cache_read += t.tokens_cache_read;
        b.cost_usd += t.cost_usd;
        // account_id берём из первого turn; если позже встретится другой, оставляем — это
        // означает что переключение произошло внутри блока (редко, но возможно).
    }
    if let Some(b) = current.take() {
        blocks.push(b);
    }

    // Финализация: is_active + burn_rate.
    for b in &mut blocks {
        b.is_active = now_ms < b.end_ms;
        let elapsed_min = b.elapsed_minutes(now_ms);
        if elapsed_min >= 1.0 {
            let total = (b.tokens_input + b.tokens_output) as f64;
            b.burn_rate_tpm = Some(total / elapsed_min);
        }
    }

    Ok(blocks)
}

/// Удобный шорткат: вернуть активный блок (где now < end_ms).
pub fn find_active(conn: &Connection, filter: &BlockFilter, now_ms: i64) -> Result<Option<Block>> {
    let blocks = build_blocks_at(conn, filter, now_ms)?;
    Ok(blocks.into_iter().rev().find(|b| b.is_active))
}

struct TurnRow {
    ts_ms: i64,
    account_id: Option<String>,
    tokens_input: i64,
    tokens_output: i64,
    tokens_cache_create: i64,
    tokens_cache_read: i64,
    cost_usd: f64,
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

    fn ensure_account(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO accounts (id, plan) VALUES (?, 'Unknown')",
            params![id],
        )
        .unwrap();
    }

    fn insert(conn: &Connection, ts_ms: i64, acct: Option<&str>, in_t: i64, out_t: i64) {
        if let Some(a) = acct {
            ensure_account(conn, a);
        }
        conn.execute(
            "INSERT INTO turns (session_id, ts_ms, account_id, tokens_input, tokens_output, cost_usd)
             VALUES (?, ?, ?, ?, ?, ?)",
            params!["s", ts_ms, acct, in_t, out_t, 0.001 * (in_t + out_t) as f64],
        )
        .unwrap();
    }

    #[test]
    fn empty_db_returns_empty() {
        let (_d, c) = open();
        let blocks = build_blocks_at(&c, &BlockFilter::default(), 0).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn single_turn_one_block() {
        let (_d, c) = open();
        insert(&c, 1_000_000, Some("acc1"), 100, 50);

        let now = 1_000_000 + 60_000; // 1 минута после
        let blocks = build_blocks_at(&c, &BlockFilter::default(), now).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start_ms, 1_000_000);
        assert_eq!(blocks[0].end_ms, 1_000_000 + BLOCK_MS);
        assert!(blocks[0].is_active);
        assert_eq!(blocks[0].turns, 1);
        assert_eq!(blocks[0].tokens_input, 100);
        assert_eq!(blocks[0].tokens_output, 50);
    }

    #[test]
    fn two_close_turns_one_block() {
        let (_d, c) = open();
        insert(&c, 1_000_000, Some("a"), 100, 50);
        insert(&c, 1_000_000 + 30 * 60_000, Some("a"), 200, 100); // 30 минут позже

        let blocks = build_blocks_at(&c, &BlockFilter::default(), 1_000_000 + 60 * 60_000).unwrap();
        assert_eq!(blocks.len(), 1, "30-минутный gap → один блок");
        assert_eq!(blocks[0].turns, 2);
        assert_eq!(blocks[0].tokens_input, 300);
    }

    #[test]
    fn gap_over_5h_splits_blocks() {
        let (_d, c) = open();
        insert(&c, 1_000_000, Some("a"), 100, 50);
        // Turn после конца первого блока (5h + 1ms)
        insert(&c, 1_000_000 + BLOCK_MS + 1, Some("a"), 200, 100);

        let now = 1_000_000 + BLOCK_MS + 60_000;
        let blocks = build_blocks_at(&c, &BlockFilter::default(), now).unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].is_active, "первый закрыт (now >= end_ms)");
        assert!(blocks[1].is_active);
    }

    #[test]
    fn long_continuous_within_5h_stays_single_block() {
        // Даже при 4h59m непрерывной активности — всё ещё один блок.
        let (_d, c) = open();
        let start = 2_000_000_000_000_i64;
        for i in 0..20 {
            insert(&c, start + i * 15 * 60_000, Some("a"), 10, 5); // каждые 15 минут
        }

        let blocks = build_blocks_at(&c, &BlockFilter::default(), start + 5 * 60 * 60_000).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].turns, 20);
    }

    #[test]
    fn filter_by_account() {
        let (_d, c) = open();
        insert(&c, 1_000_000, Some("a"), 100, 50);
        insert(&c, 2_000_000, Some("b"), 200, 100);
        insert(&c, 3_000_000, Some("a"), 50, 25);

        let blocks = build_blocks_at(
            &c,
            &BlockFilter {
                account_id: Some("a"),
                ..Default::default()
            },
            10_000_000,
        )
        .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].turns, 2);
        assert_eq!(blocks[0].tokens_input, 150);
        assert_eq!(blocks[0].account_id.as_deref(), Some("a"));
    }

    #[test]
    fn filter_by_from_to() {
        let (_d, c) = open();
        insert(&c, 1_000, None, 1, 1);
        insert(&c, 2_000, None, 10, 10);
        insert(&c, 3_000, None, 100, 100);

        let blocks = build_blocks_at(
            &c,
            &BlockFilter {
                from_ms: Some(2_000),
                to_ms: Some(3_000),
                ..Default::default()
            },
            10_000,
        )
        .unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].turns, 1);
        assert_eq!(blocks[0].tokens_input, 10);
    }

    #[test]
    fn burn_rate_computed_after_one_minute() {
        let (_d, c) = open();
        let start = 5_000_000_000_000_i64;
        insert(&c, start, Some("a"), 600, 0); // 600 input tokens at start
        insert(&c, start + 60_000, Some("a"), 0, 600); // +1 min, 600 output

        let now = start + 2 * 60_000; // 2 минуты после
        let blocks = build_blocks_at(&c, &BlockFilter::default(), now).unwrap();
        assert_eq!(blocks.len(), 1);
        let rate = blocks[0].burn_rate_tpm.unwrap();
        // Всего 1200 tokens / 2 min = 600 tpm
        assert!(
            (rate - 600.0).abs() < 1.0,
            "burn_rate ожидался ~600, получили {}",
            rate
        );
    }

    #[test]
    fn find_active_returns_last_active() {
        let (_d, c) = open();
        let start = 7_000_000_000_000_i64;
        insert(&c, start, Some("a"), 10, 5);
        insert(&c, start + BLOCK_MS + 1, Some("a"), 20, 10);

        // now внутри второго блока
        let now = start + BLOCK_MS + 60_000;
        let active = find_active(&c, &BlockFilter::default(), now)
            .unwrap()
            .unwrap();
        assert_eq!(active.start_ms, start + BLOCK_MS + 1);
        assert!(active.is_active);
    }

    #[test]
    fn find_active_returns_none_when_all_closed() {
        let (_d, c) = open();
        insert(&c, 1_000_000, Some("a"), 10, 5);
        // now далеко в будущем
        let now = 1_000_000 + 10 * BLOCK_MS;
        let active = find_active(&c, &BlockFilter::default(), now).unwrap();
        assert!(active.is_none());
    }
}
