//! Инкрементальный SQLite-индекс по JSONL-логам Claude Code.
//!
//! Используется как материализованный кеш над `~/.claude/projects/*.jsonl`.
//! Парсинг 2 GB JSONL — холодная операция (~30-60 сек), последующие
//! обращения работают за миллисекунды через `parsed_files` watermark
//! (mtime + last_offset для append-only JSONL).
//!
//! Модули:
//! - `schema` — DDL и миграции
//! - `indexer` — инкрементальное обновление (добавляется в L1.T2)

pub mod schema;
