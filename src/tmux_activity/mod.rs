//! Сбор активности из tmux: какие pane'ы открыты, что в них работает,
//! какая pane активна в каждой window.
//!
//! Источник — `tmux list-panes -a -F "..."`. Опрашиваем периодически
//! (раз в 10-30 сек) и пишем в таблицу `tmux_activity` SQLite индекса.
//!
//! Модули:
//! - `poller` — снятие snapshot tmux state
//! - `idle`   — best-effort AFK detection
//! - `store`  — INSERT в БД
//!
//! Используется в correlation engine как альтернатива ActivityWatch
//! (тяжелее, требует Python daemon) — особенно полезно когда работа
//! целиком ведётся в терминале.

pub mod poller;
