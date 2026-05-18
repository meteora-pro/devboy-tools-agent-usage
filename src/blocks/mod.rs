//! 5-часовые rate-limit блоки (как у Anthropic Claude Code).
//!
//! Anthropic сбрасывает rate-limit счётчик через 5 часов после первого
//! сообщения в блоке. То есть блок начинается с первого turn и длится
//! ровно 5 часов; turn после block.end_ms запускает новый блок.
//!
//! Не путать с weekly windows (L2.T6) — это **сессионный** rate limit,
//! более короткий. У ккаунта Max20 одновременно работают оба:
//! 5h sliding burst + 7d cumulative bucket.

pub mod engine;
