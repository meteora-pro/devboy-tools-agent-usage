//! Обнаружение переключения активного Claude Code аккаунта.
//!
//! Стратегия (best-effort, не 100% точность):
//! - При каждом проходе индексера сравниваем текущий `detect_current()` с
//!   последним известным аккаунтом по `accounts.last_seen_ms DESC`.
//! - Если id отличается — записываем switch event в `account_switches`.
//!
//! Confidence:
//! - **high**   — текущая `detect_current()` дала отличный от предыдущего id
//!   при том что предыдущий тоже из credentials.json (не env override).
//! - **medium** — точная информация недоступна (например, прошлый аккаунт был
//!   из env override).
//! - **low**    — отсутствует прошлая запись; первое появление аккаунта.
//!
//! Switch внутри одной JSONL сессии (mid-session refresh) обнаружить
//! невозможно без сохранения истории credentials.json, что выходит за рамки
//! текущей итерации. Per-turn account attribution в этом случае будет
//! неточной для нескольких turn'ов сразу после момента смены.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use super::detection::AccountInfo;

/// Один switch event для возврата вызывающему коду.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchEvent {
    pub ts_ms: i64,
    pub previous_account: Option<String>,
    pub current_account: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// Сравнить текущий обнаруженный аккаунт с последним записанным в `accounts`.
/// Если есть расхождение — записать событие в `account_switches` и вернуть его.
pub fn detect_and_record(
    conn: &Connection,
    current: &AccountInfo,
    now_ms: i64,
) -> Result<Option<SwitchEvent>> {
    // Последний известный аккаунт по last_seen_ms.
    let previous: Option<(String, i64)> = conn
        .query_row(
            "SELECT id, last_seen_ms FROM accounts
             WHERE id != ? ORDER BY last_seen_ms DESC LIMIT 1",
            params![current.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    let any_account_exists: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM accounts)", [], |r| r.get(0))
        .unwrap_or(false);

    let event = match (previous, any_account_exists) {
        // Уже есть accounts, но previous (другой id) нет → у нас был только этот же.
        (None, true) => return Ok(None),
        // Нет accounts вообще → первое появление, low confidence.
        (None, false) => SwitchEvent {
            ts_ms: now_ms,
            previous_account: None,
            current_account: current.id.clone(),
            confidence: Confidence::Low,
        },
        // Есть прошлый, отличный от текущего → high confidence.
        (Some((prev_id, _)), _) => SwitchEvent {
            ts_ms: now_ms,
            previous_account: Some(prev_id),
            current_account: current.id.clone(),
            confidence: Confidence::High,
        },
    };

    // Дедупликация: не пишем повторно тот же (previous, current) если уже было записано
    // в последние 60 секунд — защита от частых reindex.
    let recently: Option<i64> = conn
        .query_row(
            "SELECT ts_ms FROM account_switches
             WHERE (previous_account IS ? OR (previous_account IS NULL AND ? IS NULL))
               AND current_account = ?
             ORDER BY ts_ms DESC LIMIT 1",
            params![
                event.previous_account,
                event.previous_account,
                event.current_account
            ],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(prev_ts) = recently {
        if now_ms - prev_ts < 60_000 {
            return Ok(None);
        }
    }

    conn.execute(
        "INSERT INTO account_switches
         (ts_ms, previous_account, current_account, confidence, detected_at)
         VALUES (?, ?, ?, ?, datetime('now'))",
        params![
            event.ts_ms,
            event.previous_account,
            event.current_account,
            event.confidence.as_str(),
        ],
    )?;

    Ok(Some(event))
}

/// Всего switch events в БД (для CLI и тестов).
pub fn count_switches(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM account_switches", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::detection::upsert_account;
    use crate::account::plan::Plan;
    use crate::index::schema::open_index_at;
    use tempfile::tempdir;

    fn mk_info(id: &str, plan: Plan) -> AccountInfo {
        AccountInfo {
            id: id.into(),
            plan,
            rate_limit_tier: String::new(),
        }
    }

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn first_ever_detection_records_low_confidence() {
        let (_d, conn) = open();
        let acc = mk_info("aaaaaaaaaaaaaaaa", Plan::Pro);
        let ev = detect_and_record(&conn, &acc, 1_700_000_000_000)
            .unwrap()
            .expect("первое появление должно зафиксироваться");
        assert!(ev.previous_account.is_none());
        assert_eq!(ev.current_account, "aaaaaaaaaaaaaaaa");
        assert_eq!(ev.confidence, Confidence::Low);
    }

    #[test]
    fn same_account_again_no_event() {
        let (_d, conn) = open();
        let acc = mk_info("bbbbbbbbbbbbbbbb", Plan::Max20);
        upsert_account(&conn, &acc, 1_700_000_000_000).unwrap();

        // Не было разных accounts → не switch event.
        let ev = detect_and_record(&conn, &acc, 1_700_000_001_000).unwrap();
        assert!(ev.is_none());
    }

    #[test]
    fn different_account_records_high_confidence() {
        let (_d, conn) = open();
        let acc_old = mk_info("oldoldoldoldoldo", Plan::Pro);
        upsert_account(&conn, &acc_old, 1_700_000_000_000).unwrap();

        let acc_new = mk_info("newnewnewnewnewn", Plan::Max5);
        let ev = detect_and_record(&conn, &acc_new, 1_700_000_010_000)
            .unwrap()
            .expect("разный аккаунт → switch event");
        assert_eq!(ev.previous_account.as_deref(), Some("oldoldoldoldoldo"));
        assert_eq!(ev.current_account, "newnewnewnewnewn");
        assert_eq!(ev.confidence, Confidence::High);
        assert_eq!(count_switches(&conn).unwrap(), 1);
    }

    #[test]
    fn dedup_within_60s_window() {
        let (_d, conn) = open();
        let acc_old = mk_info("o1o1o1o1o1o1o1o1", Plan::Pro);
        upsert_account(&conn, &acc_old, 1_700_000_000_000).unwrap();
        let acc_new = mk_info("n1n1n1n1n1n1n1n1", Plan::Pro);

        let _ = detect_and_record(&conn, &acc_new, 1_700_000_010_000)
            .unwrap()
            .unwrap();
        // 30 сек позже — должен быть дедуплицирован
        let again = detect_and_record(&conn, &acc_new, 1_700_000_040_000).unwrap();
        assert!(again.is_none(), "за 30s должен быть дедуплицирован");
        assert_eq!(count_switches(&conn).unwrap(), 1);
    }

    #[test]
    fn dedup_window_expires_after_60s() {
        let (_d, conn) = open();
        let acc_old = mk_info("o2o2o2o2o2o2o2o2", Plan::Pro);
        upsert_account(&conn, &acc_old, 1_700_000_000_000).unwrap();
        let acc_new = mk_info("n2n2n2n2n2n2n2n2", Plan::Pro);

        let _ = detect_and_record(&conn, &acc_new, 1_700_000_010_000)
            .unwrap()
            .unwrap();
        // 70 сек позже — больше окна, должно записаться повторно
        let again = detect_and_record(&conn, &acc_new, 1_700_000_080_000).unwrap();
        assert!(again.is_some());
        assert_eq!(count_switches(&conn).unwrap(), 2);
    }
}
