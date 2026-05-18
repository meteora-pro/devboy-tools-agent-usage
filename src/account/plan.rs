//! План подписки Claude Code и связанные с ним недельные потолки.
//!
//! Цифры потолков — best-effort на основе публичных оценок (ccusage, форумы).
//! Anthropic не публикует точные tier'ы официально, поэтому держим их в одном
//! месте, чтобы легко обновлять. См. `weekly_token_ceiling` ниже.

use serde::Serialize;
use std::fmt;

/// План подписки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Plan {
    Free,
    Pro,
    Max5,
    Max20,
    Unknown,
}

impl Plan {
    /// Определить план по `subscriptionType` + `rateLimitTier` из credentials.json.
    ///
    /// `subscriptionType` обычно короткий ("pro" / "max" / "free"), а
    /// `rateLimitTier` содержит детализацию ("claude_max_20x_default" и т.п.).
    pub fn from_credentials(subscription_type: &str, rate_limit_tier: &str) -> Self {
        let sub = subscription_type.to_lowercase();
        let tier = rate_limit_tier.to_lowercase();
        match sub.as_str() {
            "free" => Plan::Free,
            "pro" => Plan::Pro,
            "max" => {
                if tier.contains("20x") {
                    Plan::Max20
                } else if tier.contains("5x") {
                    Plan::Max5
                } else {
                    // Default для max — берём Max5 как более консервативный
                    Plan::Max5
                }
            }
            _ => Plan::Unknown,
        }
    }

    /// Грубая оценка недельного token-потолка (используется в L2.T8 limits %).
    /// Значения — best-effort, конфигурируются в `limits::plan` позже.
    pub fn weekly_token_ceiling(self) -> Option<u64> {
        match self {
            Plan::Free => Some(0),
            Plan::Pro => Some(44_000_000),    // 44M
            Plan::Max5 => Some(88_000_000),   // 88M
            Plan::Max20 => Some(220_000_000), // 220M
            Plan::Unknown => None,
        }
    }

    /// Машинно-читаемая строка для хранения в БД и JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "Free",
            Plan::Pro => "Pro",
            Plan::Max5 => "Max5",
            Plan::Max20 => "Max20",
            Plan::Unknown => "Unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "Free" => Plan::Free,
            "Pro" => Plan::Pro,
            "Max5" => Plan::Max5,
            "Max20" => Plan::Max20,
            _ => Plan::Unknown,
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pro_detected() {
        assert_eq!(Plan::from_credentials("pro", "claude_pro"), Plan::Pro);
    }

    #[test]
    fn max20_detected_by_tier_suffix() {
        assert_eq!(
            Plan::from_credentials("max", "claude_max_20x_default"),
            Plan::Max20
        );
    }

    #[test]
    fn max5_detected_by_tier_suffix() {
        assert_eq!(
            Plan::from_credentials("max", "claude_max_5x_default"),
            Plan::Max5
        );
    }

    #[test]
    fn max_without_marker_defaults_to_max5() {
        assert_eq!(Plan::from_credentials("max", "claude_max"), Plan::Max5);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(Plan::from_credentials("PRO", "Claude_Pro"), Plan::Pro);
    }

    #[test]
    fn unknown_subscription_returns_unknown() {
        assert_eq!(Plan::from_credentials("enterprise", ""), Plan::Unknown);
    }

    #[test]
    fn ceilings_monotonic() {
        // Pro < Max5 < Max20
        let pro = Plan::Pro.weekly_token_ceiling().unwrap();
        let max5 = Plan::Max5.weekly_token_ceiling().unwrap();
        let max20 = Plan::Max20.weekly_token_ceiling().unwrap();
        assert!(pro < max5);
        assert!(max5 < max20);
    }

    #[test]
    fn unknown_has_no_ceiling() {
        assert!(Plan::Unknown.weekly_token_ceiling().is_none());
    }

    #[test]
    fn roundtrip_string() {
        for p in [
            Plan::Free,
            Plan::Pro,
            Plan::Max5,
            Plan::Max20,
            Plan::Unknown,
        ] {
            assert_eq!(Plan::parse(p.as_str()), p);
        }
    }
}
