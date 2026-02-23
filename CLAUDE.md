# CLAUDE.md — Инструкции для Claude Code

## Проект

CLI-инструмент на Rust для анализа использования Claude Code. Подробная архитектура: [ARCHITECTURE.md](ARCHITECTURE.md)

## Команды сборки

```bash
cargo build              # debug build
cargo build --release    # release build
cargo test               # все тесты
cargo run -- tasks --from 2026-02-20 --with-llm  # запуск
```

## Структура

- `src/claude/` — парсинг JSONL логов из `~/.claude/projects/`
- `src/activity/` — интеграция с ActivityWatch (SQLite)
- `src/classification/` — LLM классификация и кеш (SQLite)
- `src/correlation/` — корреляция сессий, группировка по задачам
- `src/output/` — вывод: table, JSON, CSV, timeline

## Конвенции

- Язык кода: английский (переменные, функции, типы)
- Комментарии в коде: русский с английскими техническими терминами
- Документация (.md файлы): русский
- Пакетный менеджер: pnpm (не применимо к Rust, но для JS-зависимостей если появятся)
- Обработка ошибок: `anyhow::Result` для функций, `thiserror` для типизированных ошибок
- CLI: clap 4 с derive-макросами
- Все команды поддерживают `--format table|json|csv`

## Ключевые модели

- `ClaudeSession` / `Turn` — сессия и ход диалога (`src/claude/session.rs`)
- `TaskStats` / `ToolCallStats` — статистика задач (`src/correlation/models.rs`)
- `TaskSummary` — результат LLM суммаризации: summary + status + title (`src/classification/client.rs`)
- `Classifier` — оркестратор: cache → LLM → fallback (`src/classification/mod.rs`)

## Кеш

SQLite в `~/.cache/devboy-agent-usage/classifications.db`:
- `turn_classifications` — классификация turns
- `task_summaries` — суммаризация задач (title, summary, status)
- `chunk_summaries` — промежуточные суммаризации (hierarchical)
- `manual_titles` — ручные заголовки (команда `retitle`)
