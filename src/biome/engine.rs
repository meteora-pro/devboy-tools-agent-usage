//! Биом-классификация per-session + агрегации.
//!
//! Thresholds совпадают с Python skill `analyze-usage`:
//! - ≥500   Whale     🐋
//! - ≥100   Shark     🦈
//! - ≥30    Dolphin   🐬
//! - ≥10    Fish      🐟
//! - ≥3     Shrimp    🦐
//! - <3     Plankton  🦠

use anyhow::Result;
#[cfg(test)]
use rusqlite::params;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

/// 6 биомов, отсортированных по интенсивности.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Hash)]
pub enum Biome {
    Plankton,
    Shrimp,
    Fish,
    Dolphin,
    Shark,
    Whale,
}

impl Biome {
    /// Классификация по числу turn'ов (assistant events с usage).
    pub fn of(turn_count: i64) -> Self {
        match turn_count {
            n if n >= 500 => Biome::Whale,
            n if n >= 100 => Biome::Shark,
            n if n >= 30 => Biome::Dolphin,
            n if n >= 10 => Biome::Fish,
            n if n >= 3 => Biome::Shrimp,
            _ => Biome::Plankton,
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Biome::Whale => "🐋",
            Biome::Shark => "🦈",
            Biome::Dolphin => "🐬",
            Biome::Fish => "🐟",
            Biome::Shrimp => "🦐",
            Biome::Plankton => "🦠",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Biome::Whale => "Whale",
            Biome::Shark => "Shark",
            Biome::Dolphin => "Dolphin",
            Biome::Fish => "Fish",
            Biome::Shrimp => "Shrimp",
            Biome::Plankton => "Plankton",
        }
    }

    pub fn all() -> [Biome; 6] {
        [
            Biome::Whale,
            Biome::Shark,
            Biome::Dolphin,
            Biome::Fish,
            Biome::Shrimp,
            Biome::Plankton,
        ]
    }
}

impl fmt::Display for Biome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.emoji(), self.name())
    }
}

/// Биом одной сессии.
#[derive(Debug, Clone, Serialize)]
pub struct SessionBiome {
    pub session_id: String,
    pub biome: Biome,
    pub turns: i64,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub account_id: Option<String>,
    pub project: Option<String>,
    pub cost_usd: f64,
}

/// Опции для classify_sessions.
#[derive(Debug, Default, Clone)]
pub struct BiomeFilter<'a> {
    pub account_id: Option<&'a str>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
}

/// Классифицировать каждую сессию в индексе. Возвращает в хронологическом
/// порядке (по first_ts).
pub fn classify_sessions(conn: &Connection, filter: &BiomeFilter) -> Result<Vec<SessionBiome>> {
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
        "SELECT session_id,
                COUNT(*) AS turns,
                MIN(ts_ms),
                MAX(ts_ms),
                MAX(account_id),
                MAX(project),
                SUM(cost_usd)
         FROM turns
         {where_clause}
         GROUP BY session_id
         ORDER BY MIN(ts_ms) ASC",
    );

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let mut out = Vec::new();
    let rows = stmt.query_map(rusqlite::params_from_iter(bind_refs.iter().copied()), |r| {
        let turns: i64 = r.get(1)?;
        Ok(SessionBiome {
            session_id: r.get(0)?,
            biome: Biome::of(turns),
            turns,
            first_ts_ms: r.get(2)?,
            last_ts_ms: r.get(3)?,
            account_id: r.get(4).ok(),
            project: r.get(5).ok(),
            cost_usd: r.get(6)?,
        })
    })?;
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Сводка: сколько сессий в каждом биоме (по всем 6, даже если 0).
#[derive(Debug, Clone, Serialize)]
pub struct BiomeSummary {
    pub counts: BTreeMap<String, i64>,
    pub total_sessions: i64,
    pub total_turns: i64,
    pub total_cost_usd: f64,
}

/// Aquarium-style сводка.
pub fn summary(conn: &Connection, filter: &BiomeFilter) -> Result<BiomeSummary> {
    let sessions = classify_sessions(conn, filter)?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for b in Biome::all() {
        counts.insert(b.name().to_string(), 0);
    }
    let mut total_turns = 0_i64;
    let mut total_cost = 0.0;
    for s in &sessions {
        *counts.entry(s.biome.name().to_string()).or_insert(0) += 1;
        total_turns += s.turns;
        total_cost += s.cost_usd;
    }
    Ok(BiomeSummary {
        counts,
        total_sessions: sessions.len() as i64,
        total_turns,
        total_cost_usd: total_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use tempfile::tempdir;

    #[test]
    fn boundary_thresholds() {
        assert_eq!(Biome::of(0), Biome::Plankton);
        assert_eq!(Biome::of(2), Biome::Plankton);
        assert_eq!(Biome::of(3), Biome::Shrimp);
        assert_eq!(Biome::of(9), Biome::Shrimp);
        assert_eq!(Biome::of(10), Biome::Fish);
        assert_eq!(Biome::of(29), Biome::Fish);
        assert_eq!(Biome::of(30), Biome::Dolphin);
        assert_eq!(Biome::of(99), Biome::Dolphin);
        assert_eq!(Biome::of(100), Biome::Shark);
        assert_eq!(Biome::of(499), Biome::Shark);
        assert_eq!(Biome::of(500), Biome::Whale);
        assert_eq!(Biome::of(10_000), Biome::Whale);
    }

    #[test]
    fn ordering_intuitive() {
        assert!(Biome::Plankton < Biome::Whale);
        assert!(Biome::Shrimp < Biome::Fish);
    }

    #[test]
    fn classify_groups_by_session() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        // session "alpha": 5 turns → Shrimp
        for i in 0..5 {
            conn.execute(
                "INSERT INTO turns (session_id, ts_ms, tokens_input, cost_usd)
                 VALUES ('alpha', ?, 1, 0.01)",
                params![1000 + i],
            )
            .unwrap();
        }
        // session "beta": 50 turns → Dolphin
        for i in 0..50 {
            conn.execute(
                "INSERT INTO turns (session_id, ts_ms, tokens_input, cost_usd)
                 VALUES ('beta', ?, 1, 0.02)",
                params![2000 + i],
            )
            .unwrap();
        }

        let s = classify_sessions(&conn, &BiomeFilter::default()).unwrap();
        assert_eq!(s.len(), 2);

        let alpha = s.iter().find(|x| x.session_id == "alpha").unwrap();
        assert_eq!(alpha.biome, Biome::Shrimp);
        assert_eq!(alpha.turns, 5);

        let beta = s.iter().find(|x| x.session_id == "beta").unwrap();
        assert_eq!(beta.biome, Biome::Dolphin);
        assert_eq!(beta.turns, 50);
    }

    #[test]
    fn summary_counts_per_biome() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        let mk = |sid: &str, n: i64| {
            for i in 0..n {
                conn.execute(
                    "INSERT INTO turns (session_id, ts_ms, tokens_input, cost_usd)
                     VALUES (?, ?, 1, 0.001)",
                    params![sid, i],
                )
                .unwrap();
            }
        };
        mk("s_plankton", 1);
        mk("s_shrimp", 5);
        mk("s_fish", 15);
        mk("s_dolphin", 35);
        mk("s_shark", 200);
        mk("s_whale", 600);

        let sum = summary(&conn, &BiomeFilter::default()).unwrap();
        assert_eq!(sum.total_sessions, 6);
        assert_eq!(*sum.counts.get("Plankton").unwrap(), 1);
        assert_eq!(*sum.counts.get("Shrimp").unwrap(), 1);
        assert_eq!(*sum.counts.get("Fish").unwrap(), 1);
        assert_eq!(*sum.counts.get("Dolphin").unwrap(), 1);
        assert_eq!(*sum.counts.get("Shark").unwrap(), 1);
        assert_eq!(*sum.counts.get("Whale").unwrap(), 1);
    }

    #[test]
    fn filter_by_account() {
        let dir = tempdir().unwrap();
        let conn = open_index_at(&dir.path().join("idx.db")).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, plan) VALUES ('aa', 'Pro'), ('bb', 'Pro')",
            [],
        )
        .unwrap();
        let mk = |sid: &str, acc: &str, n: i64| {
            for i in 0..n {
                conn.execute(
                    "INSERT INTO turns (session_id, ts_ms, account_id, tokens_input, cost_usd)
                     VALUES (?, ?, ?, 1, 0.001)",
                    params![sid, i, acc],
                )
                .unwrap();
            }
        };
        mk("s1", "aa", 5);
        mk("s2", "bb", 5);

        let only_aa = classify_sessions(
            &conn,
            &BiomeFilter {
                account_id: Some("aa"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only_aa.len(), 1);
        assert_eq!(only_aa[0].session_id, "s1");
    }

    #[test]
    fn emoji_unique_per_biome() {
        let emojis: std::collections::HashSet<_> = Biome::all().iter().map(|b| b.emoji()).collect();
        assert_eq!(emojis.len(), 6);
    }
}
