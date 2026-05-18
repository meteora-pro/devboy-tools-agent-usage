//! Биом-классификация сессий: упрощённый Rust-порт из skill `analyze-usage`.
//!
//! Каждая сессия попадает в один из 6 биомов по количеству assistant
//! turn'ов с usage. В Python skill используется `real_prompts` (user
//! events с `real_human` kind), здесь — proxy через assistant turn
//! count: для большинства сессий пропорция близка к 1:1.

pub mod engine;
