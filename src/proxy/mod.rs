//! cc-proxy correlation — ingest транспортных наблюдений из cc-proxy лога и
//! джойн с `turns` по `request_id` (эпик proxy-correlation).
//!
//! cc-proxy (`~/projects/devboy-tmux/proxy`) пишет per-request JSONL с join-ключами
//! `session_id`/`request_id` и HTTP-уровневыми скалярами (wait_ms/dur_ms/inflight/
//! status/sse_error), которых нет в транскрипте. Здесь мы их инжестим в
//! `proxy_observations` и обогащаем аналитику.

pub mod correlation;
pub mod ingest;

pub use ingest::{default_proxy_log, ingest_proxy_log, ProxyIngestStats};
