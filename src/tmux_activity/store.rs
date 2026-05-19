//! Запись snapshot tmux activity в SQLite.

use anyhow::Result;
use rusqlite::{params, Connection};

use super::poller::PaneSnapshot;

/// Записать batch pane snapshots с общим timestamp.
/// idle_ms — общий для всех pane в этом snapshot (берётся из GUI session,
/// а не из tmux pane активности).
pub fn insert_snapshot(
    conn: &Connection,
    ts_ms: i64,
    panes: &[PaneSnapshot],
    idle_ms: Option<i64>,
) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut count = 0;
    for p in panes {
        tx.execute(
            "INSERT INTO tmux_activity
             (ts_ms, session, window_idx, window_name, pane_idx, pane_active,
              command, cwd, idle_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ts_ms,
                p.session,
                p.window_idx,
                p.window_name,
                p.pane_idx,
                if p.pane_active { 1_i64 } else { 0 },
                p.command,
                p.cwd,
                idle_ms,
            ],
        )?;
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}

/// Количество snapshot строк (для smoke / тестов).
pub fn total_rows(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM tmux_activity", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use tempfile::tempdir;

    fn mk(session: &str, window_idx: i64, pane_idx: i64, active: bool, cmd: &str) -> PaneSnapshot {
        PaneSnapshot {
            session: session.into(),
            window_idx,
            window_name: format!("w{}", window_idx),
            pane_idx,
            pane_active: active,
            command: cmd.into(),
            cwd: "/test".into(),
        }
    }

    #[test]
    fn inserts_all_panes() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        let panes = vec![
            mk("main", 0, 0, true, "claude"),
            mk("main", 0, 1, false, "bash"),
            mk("work", 1, 0, true, "vim"),
        ];
        let n = insert_snapshot(&conn, 1_700_000_000_000, &panes, Some(0)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(total_rows(&conn).unwrap(), 3);
    }

    #[test]
    fn idle_ms_persisted_or_null() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        let panes = vec![mk("s", 0, 0, true, "bash")];
        insert_snapshot(&conn, 1, &panes, Some(5000)).unwrap();
        insert_snapshot(&conn, 2, &panes, None).unwrap();

        let rows: Vec<Option<i64>> = conn
            .prepare("SELECT idle_ms FROM tmux_activity ORDER BY ts_ms")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(rows, vec![Some(5000), None]);
    }

    #[test]
    fn empty_snapshot_is_noop() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        let n = insert_snapshot(&conn, 1, &[], None).unwrap();
        assert_eq!(n, 0);
        assert_eq!(total_rows(&conn).unwrap(), 0);
    }
}
