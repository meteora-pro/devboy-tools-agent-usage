//! Клиент для Anthropic OAuth Usage API.
//!
//! Endpoint: `https://api.anthropic.com/api/oauth/usage`
//! Headers: `Authorization: Bearer <accessToken>`, `anthropic-beta: oauth-2025-04-20`.
//!
//! Это **undocumented** API, но именно его читает встроенный `/status` в
//! Claude Code. Цифры здесь — те же что показывает /status (5h/week %).
//!
//! Token читается из `~/.claude/.credentials.json` каждый запрос (он там
//! актуальный — Claude Code сам refresh-ит). Сам token нигде не сохраняем.

pub mod client;
