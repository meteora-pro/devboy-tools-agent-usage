//! HTTP клиент для `/api/oauth/usage`.

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};

/// Десериализатор, который трактует и отсутствие поля, и явный `null` как
/// `Default::default()`. Нужен потому что `#[serde(default)]` покрывает только
/// отсутствие ключа, а usage-API стал слать `null` для неактивных полей
/// (`monthly_limit`, `used_credits`, `currency` в `extra_usage`).
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// URL endpoint'а. Не публичный — может измениться без warning.
pub const USAGE_API_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Beta header который требуется для этого endpoint.
pub const BETA_HEADER: &str = "oauth-2025-04-20";

/// User-Agent чтобы Anthropic мог трекать нашу tooling если рейт-лимит зашалит.
const USER_AGENT: &str = concat!("devboy-tools-agent-usage/", env!("CARGO_PKG_VERSION"));

/// Один utilization bucket (5h, 7d, и т.д.).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UsageBucket {
    /// Процент использования (0.0–100.0+).
    #[serde(default)]
    pub utilization: f64,
    /// ISO timestamp когда счётчик сбросится. Может отсутствовать.
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// Информация о overage payments (extra credits в EUR/USD).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ExtraUsage {
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub monthly_limit: f64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub used_credits: f64,
    /// Утилизация если applicable (может быть null).
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub currency: String,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

/// Полный ответ endpoint'а. Поля, которые могут быть null, помечены Option.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UsageResponse {
    pub five_hour: UsageBucket,
    pub seven_day: UsageBucket,
    #[serde(default)]
    pub seven_day_sonnet: Option<UsageBucket>,
    #[serde(default)]
    pub seven_day_opus: Option<UsageBucket>,
    #[serde(default)]
    pub seven_day_oauth_apps: Option<UsageBucket>,
    #[serde(default)]
    pub seven_day_cowork: Option<UsageBucket>,
    #[serde(default)]
    pub seven_day_omelette: Option<UsageBucket>,
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
}

/// Errors при обращении к endpoint'у.
#[derive(Debug, thiserror::Error)]
pub enum UsageApiError {
    #[error("credentials.json не найден или не парсится: {0}")]
    NoCredentials(String),
    #[error("HTTP {status}: {body}")]
    HttpError { status: u16, body: String },
    #[error("rate limited (429): {body}")]
    RateLimited { body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("JSON parse error: {0}")]
    Parse(String),
}

/// Прочитать access token из credentials.json. Возвращается только для
/// немедленного использования в HTTP-запросе; в БД не пишется.
fn read_access_token(path: &Path) -> Result<String> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("чтение {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    v.get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("accessToken отсутствует в credentials"))
}

fn default_credentials_path() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".claude").join(".credentials.json"))
        .unwrap_or_else(|| PathBuf::from(".credentials.json"))
}

/// Получить current usage. Самый прямолинейный вариант.
pub fn fetch_usage() -> std::result::Result<UsageResponse, UsageApiError> {
    let path = default_credentials_path();
    fetch_usage_with_credentials(&path)
}

/// Версия с явным путём к credentials.json (для тестов и кастомных деплойментов).
pub fn fetch_usage_with_credentials(
    credentials_path: &Path,
) -> std::result::Result<UsageResponse, UsageApiError> {
    let token = read_access_token(credentials_path)
        .map_err(|e| UsageApiError::NoCredentials(e.to_string()))?;

    let agent = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();

    let response = agent
        .get(USAGE_API_URL)
        .header("Authorization", &format!("Bearer {}", token))
        .header("anthropic-beta", BETA_HEADER)
        .header("User-Agent", USER_AGENT)
        .call();

    let mut response = match response {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            // ureq 3 не разделяет 429 от других статусов явно через variant.
            // Проверяем text — если "rate limit" или 429 в message — это RateLimited.
            if msg.contains("429") || msg.to_lowercase().contains("rate limit") {
                return Err(UsageApiError::RateLimited { body: msg });
            }
            // HTTP status в ureq 3.x попадает в Error::StatusCode(u16) если non-2xx.
            // Для надёжности используем substring detection.
            if let Some(code) = msg.split_whitespace().find_map(|w| w.parse::<u16>().ok()) {
                if (400..600).contains(&code) {
                    return Err(UsageApiError::HttpError {
                        status: code,
                        body: msg,
                    });
                }
            }
            return Err(UsageApiError::Network(msg));
        }
    };

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| UsageApiError::Network(e.to_string()))?;
    parse_usage_response(&body)
}

/// Распарсить JSON ответ. Отдельной функцией для тестов на синтетических fixtures.
pub fn parse_usage_response(body: &str) -> std::result::Result<UsageResponse, UsageApiError> {
    serde_json::from_str(body).map_err(|e| UsageApiError::Parse(format!("{}: {}", e, body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn fixture_response() -> &'static str {
        r#"{
          "five_hour": {
            "utilization": 18.0,
            "resets_at": "2026-05-19T09:10:00.699888+00:00"
          },
          "seven_day": {
            "utilization": 44.0,
            "resets_at": "2026-05-23T04:00:00.699911+00:00"
          },
          "seven_day_oauth_apps": null,
          "seven_day_opus": null,
          "seven_day_sonnet": {
            "utilization": 0.0,
            "resets_at": "2026-05-23T04:00:00.699922+00:00"
          },
          "seven_day_cowork": null,
          "seven_day_omelette": {
            "utilization": 0.0,
            "resets_at": null
          },
          "extra_usage": {
            "is_enabled": true,
            "monthly_limit": 2000,
            "used_credits": 0.0,
            "utilization": null,
            "currency": "EUR",
            "disabled_reason": null
          }
        }"#
    }

    #[test]
    fn parses_full_response() {
        let r = parse_usage_response(fixture_response()).unwrap();
        assert_eq!(r.five_hour.utilization, 18.0);
        assert_eq!(r.seven_day.utilization, 44.0);
        assert_eq!(r.seven_day_sonnet.as_ref().unwrap().utilization, 0.0);
        assert!(r.seven_day_opus.is_none());
        let extra = r.extra_usage.as_ref().unwrap();
        assert!(extra.is_enabled);
        assert_eq!(extra.monthly_limit, 2000.0);
        assert_eq!(extra.currency, "EUR");
    }

    #[test]
    fn parse_resets_at_preserved() {
        let r = parse_usage_response(fixture_response()).unwrap();
        assert!(r
            .five_hour
            .resets_at
            .as_ref()
            .unwrap()
            .contains("2026-05-19"));
    }

    #[test]
    fn parse_minimal_response_works() {
        let body = r#"{"five_hour":{"utilization":1.5},"seven_day":{"utilization":2.5}}"#;
        let r = parse_usage_response(body).unwrap();
        assert_eq!(r.five_hour.utilization, 1.5);
        assert_eq!(r.seven_day.utilization, 2.5);
        assert!(r.seven_day_sonnet.is_none());
        assert!(r.extra_usage.is_none());
    }

    #[test]
    fn parse_null_extra_usage_fields() {
        // API шлёт null для неактивных полей extra_usage — не должно ронять парсинг.
        let body = r#"{
            "five_hour":{"utilization":5.0},
            "seven_day":{"utilization":64.0},
            "seven_day_opus":null,
            "extra_usage":{"is_enabled":false,"monthly_limit":null,
                "used_credits":null,"utilization":null,"currency":null,
                "disabled_reason":"out_of_credits"}
        }"#;
        let r = parse_usage_response(body).unwrap();
        let extra = r.extra_usage.as_ref().unwrap();
        assert_eq!(extra.monthly_limit, 0.0);
        assert_eq!(extra.used_credits, 0.0);
        assert_eq!(extra.utilization, None);
        assert_eq!(extra.currency, "");
        assert_eq!(extra.disabled_reason.as_deref(), Some("out_of_credits"));
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let r = parse_usage_response("{not valid json");
        assert!(matches!(r, Err(UsageApiError::Parse(_))));
    }

    #[test]
    fn read_access_token_from_real_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-test","refreshToken":"rt","expiresAt":0}}"#,
        )
        .unwrap();
        let t = read_access_token(&path).unwrap();
        assert_eq!(t, "sk-ant-test");
    }

    #[test]
    fn missing_credentials_returns_no_credentials() {
        let r = fetch_usage_with_credentials(Path::new("/nonexistent/path.json"));
        assert!(matches!(r, Err(UsageApiError::NoCredentials(_))));
    }

    #[test]
    fn corrupt_credentials_returns_no_credentials() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("creds.json");
        fs::write(&path, b"{not json").unwrap();
        let r = fetch_usage_with_credentials(&path);
        assert!(matches!(r, Err(UsageApiError::NoCredentials(_))));
    }
}
