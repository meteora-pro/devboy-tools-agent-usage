//! Аккаунты Claude Code: определение текущего, запись в индекс, маппинг план/tier.
//!
//! Источники identity (по приоритету):
//! 1. ENV `CLAUDE_ACCOUNT` — ручной override, удобно для тестов и smoke-сценариев.
//! 2. `~/.claude/.credentials.json` → SipHash(refreshToken)[..16] — стабильный
//!    в пределах одной OAuth-сессии (refresh-token живёт неделями).
//!
//! Внимание: токены никогда не логируются и не пишутся в БД. В индекс попадает
//! только производный hash (account_id) + публичные метаданные (план, tier).

pub mod detection;
pub mod plan;
pub mod switching;
