//! Определение текущего Claude Code аккаунта.
//!
//! Источники identity (по приоритету):
//! 1. ENV `CLAUDE_ACCOUNT` — ручной override (любая стабильная строка).
//! 2. `~/.claude/.credentials.json` → SipHash(refreshToken) → 16 hex chars.
//!
//! Hash от refresh-token даёт stable identifier пока пользователь не сменил логин;
//! при этом сам токен никогда не покидает функцию detect().

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::plan::Plan;

/// Информация о текущем аккаунте.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    /// Stable id: либо из ENV, либо `siphash(refreshToken)[..16]`.
    pub id: String,
    /// План подписки (Pro/Max5/Max20/...). Может быть Unknown при override.
    pub plan: Plan,
    /// Полный rate limit tier из credentials, для referenсe. Пустая строка если override.
    pub rate_limit_tier: String,
}

/// Найти текущий активный аккаунт. None если ни ENV, ни credentials недоступны.
pub fn detect_current() -> Option<AccountInfo> {
    detect_with(
        std::env::var("CLAUDE_ACCOUNT").ok().as_deref(),
        &credentials_path(),
    )
}

/// Версия `detect_current()` для тестов: вместо реального ENV и file path принимает аргументы.
pub fn detect_with(env_override: Option<&str>, credentials_path: &Path) -> Option<AccountInfo> {
    if let Some(id) = env_override {
        if !id.is_empty() {
            return Some(AccountInfo {
                id: id.to_string(),
                plan: Plan::Unknown,
                rate_limit_tier: String::new(),
            });
        }
    }

    match read_credentials(credentials_path) {
        Ok(creds) => {
            let id = stable_id_from_token(&creds.access_token, &creds.refresh_token);
            let plan = Plan::from_credentials(&creds.subscription_type, &creds.rate_limit_tier);
            Some(AccountInfo {
                id,
                plan,
                rate_limit_tier: creds.rate_limit_tier,
            })
        }
        Err(_) => None,
    }
}

/// Записать (или обновить) аккаунт в таблицу `accounts`.
/// Обновляет last_seen_ms и plan/tier при изменении.
pub fn upsert_account(conn: &Connection, info: &AccountInfo, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO accounts (id, plan, first_seen_ms, last_seen_ms, notes)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            plan = excluded.plan,
            last_seen_ms = excluded.last_seen_ms,
            notes = excluded.notes",
        params![
            info.id,
            info.plan.as_str(),
            now_ms,
            now_ms,
            info.rate_limit_tier
        ],
    )?;
    Ok(())
}

fn credentials_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude").join(".credentials.json"))
        .unwrap_or_else(|| PathBuf::from(".credentials.json"))
}

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: OAuthSection,
    #[serde(default)]
    subscription_type: String,
    #[serde(default)]
    rate_limit_tier: String,
}

#[derive(Debug, Deserialize)]
struct OAuthSection {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
    #[serde(rename = "subscriptionType", default)]
    subscription_type: String,
    #[serde(rename = "rateLimitTier", default)]
    rate_limit_tier: String,
}

/// Внутреннее представление после нормализации.
struct ParsedCredentials {
    access_token: String,
    refresh_token: String,
    subscription_type: String,
    rate_limit_tier: String,
}

fn read_credentials(path: &Path) -> Result<ParsedCredentials> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("чтение {}", path.display()))?;
    let parsed: Credentials = serde_json::from_str(&raw).context("парсинг credentials JSON")?;
    Ok(ParsedCredentials {
        access_token: parsed.oauth.access_token,
        refresh_token: parsed.oauth.refresh_token,
        // top-level поля subscription_type/rate_limit_tier для обратной совместимости,
        // основной источник — внутри claudeAiOauth.
        subscription_type: if !parsed.oauth.subscription_type.is_empty() {
            parsed.oauth.subscription_type
        } else {
            parsed.subscription_type
        },
        rate_limit_tier: if !parsed.oauth.rate_limit_tier.is_empty() {
            parsed.oauth.rate_limit_tier
        } else {
            parsed.rate_limit_tier
        },
    })
}

/// Stable hash от пары токенов через std SipHash. Не cryptographic, но достаточный
/// для grouping (вероятность коллизии 1 / 2^64). Сам токен НЕ возвращается.
fn stable_id_from_token(access: &str, refresh: &str) -> String {
    let mut h = DefaultHasher::new();
    // Refresh token живёт неделями → стабильнее access (~1 час).
    // Если refresh пуст — fallback на access.
    if !refresh.is_empty() {
        refresh.hash(&mut h);
    } else {
        access.hash(&mut h);
    }
    let hash = h.finish();
    format!("{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::open_index_at;
    use std::fs;
    use tempfile::tempdir;

    fn write_credentials(path: &Path, refresh: &str, sub: &str, tier: &str) {
        let json = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "dummy-access",
                "refreshToken": refresh,
                "subscriptionType": sub,
                "rateLimitTier": tier,
                "expiresAt": 0,
                "scopes": [],
            }
        });
        fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    #[test]
    fn env_override_takes_precedence() {
        let info = detect_with(Some("custom-id-from-env"), Path::new("/nonexistent")).unwrap();
        assert_eq!(info.id, "custom-id-from-env");
        assert_eq!(info.plan, Plan::Unknown);
    }

    #[test]
    fn empty_env_falls_through() {
        let info = detect_with(Some(""), Path::new("/nonexistent"));
        assert!(info.is_none(), "пустой ENV не должен считаться override");
    }

    #[test]
    fn detect_from_credentials() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        write_credentials(&path, "rt-12345", "max", "claude_max_20x_default");

        let info = detect_with(None, &path).unwrap();
        assert_eq!(info.id.len(), 16);
        assert!(info.id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(info.plan, Plan::Max20);
        assert_eq!(info.rate_limit_tier, "claude_max_20x_default");
    }

    #[test]
    fn same_refresh_token_gives_same_id() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        write_credentials(&path, "stable-refresh", "pro", "claude_pro");

        let a = detect_with(None, &path).unwrap();
        let b = detect_with(None, &path).unwrap();
        assert_eq!(
            a.id, b.id,
            "при одинаковом refresh ID должен быть стабильным"
        );
    }

    #[test]
    fn different_refresh_gives_different_id() {
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("a.json");
        let path2 = dir.path().join("b.json");
        write_credentials(&path1, "refresh-a", "pro", "claude_pro");
        write_credentials(&path2, "refresh-b", "pro", "claude_pro");

        let a = detect_with(None, &path1).unwrap();
        let b = detect_with(None, &path2).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn missing_file_returns_none() {
        assert!(detect_with(None, Path::new("/no/such/file.json")).is_none());
    }

    #[test]
    fn corrupt_json_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, b"{this is not valid json").unwrap();
        assert!(detect_with(None, &path).is_none());
    }

    #[test]
    fn upsert_inserts_and_updates() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let conn = open_index_at(&db).unwrap();

        let info = AccountInfo {
            id: "abc1234567890def".into(),
            plan: Plan::Pro,
            rate_limit_tier: "claude_pro".into(),
        };
        upsert_account(&conn, &info, 1_700_000_000_000).unwrap();
        upsert_account(&conn, &info, 1_700_000_001_000).unwrap();

        // Должна быть ровно одна запись с обновлённым last_seen
        let (first, last): (i64, i64) = conn
            .query_row(
                "SELECT first_seen_ms, last_seen_ms FROM accounts WHERE id = ?",
                params![info.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(first, 1_700_000_000_000);
        assert_eq!(last, 1_700_000_001_000);
    }
}
