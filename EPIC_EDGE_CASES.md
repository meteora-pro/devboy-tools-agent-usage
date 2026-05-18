# Epic: statusline + biome — edge cases checklist

> Временный документ для отслеживания edge cases. Живёт в ветке `epic/statusline-biome`,
> удаляется при merge/drop ветки.

Каждый subtask добавляет свои edge cases. Проверяются в конце соответствующего слоя
(L1 / L2 / L3 / L4 / L5). Финальный smoke pass — после L4 (когда есть рабочий tmux output).

---

## L1: Storage

### L1.T1 — SQLite schema

- [ ] Cold start: `~/.cache/devboy-tools-agent-usage/` не существует → создаётся автоматически
- [ ] Открытие БД на разных FS: ext4, btrfs, NTFS (HotStorage/ColdStorage)
- [ ] Concurrent open: два процесса одновременно (WAL должен разруливать)
- [ ] Power loss во время миграции: schema_versions либо имеет запись, либо нет (atomicity through INSERT)
- [ ] Поломанная БД (corrupted) — graceful error message, не паник
- [ ] Read-only FS (например `/mnt/cold-360` если случайно) — clear error

### L1.T2 — Incremental indexer

- [ ] Файл удалён между discovery и parse → skip с warning, не паник
- [ ] Файл уменьшился (truncate, rotation) — invalidate watermark и reparse целиком
- [ ] Файл изменил mtime но size совпадает — reparse от last_offset (append)
- [ ] Битый JSON в середине JSONL — пропуск строки + продолжение (не аборт)
- [ ] Очень длинная строка (>10MB JSON) — не OOM
- [ ] Параллельный indexer и watch mode — flock или WAL serialization
- [ ] Старые JSONL до v2.1.128 wipe (апр 2026) — могут отсутствовать, поведение `since` flag
- [ ] cache_creation.ephemeral_5m vs ephemeral_1h — оба должны учитываться в tokens_cache_create

### L1.T3 — `index` CLI

- [ ] `--full` — переиндексация всего, очистка `parsed_files`
- [ ] `--since DATE` — пропуск файлов с mtime < DATE
- [ ] Прогресс-бар — корректно при пустом наборе файлов
- [ ] Performance: cold parse 2.1 GB — <60 сек на этой машине
- [ ] Warm update: 0-1 файлов изменено — <500 ms

---

## L2: Domain

### L2.T4 — Account detection

- [ ] `~/.claude/.credentials.json` отсутствует → fallback `unknown` account
- [ ] Credentials поломан (не валидный JSON) → warning + fallback
- [ ] JWT не парсится (не три части или невалидный base64) → fallback
- [ ] Email отсутствует в JWT payload → use `sub` field как id
- [ ] `CLAUDE_ACCOUNT` env override — переопределяет credentials
- [ ] Manual `accounts set <id>` — создаёт запись без credentials

### L2.T5 — Account switching

- [ ] Single account для всех turn → 0 switch events
- [ ] Очень частые мини-gap'ы (<1h) — не считаются switches
- [ ] Confidence score = low при отсутствии независимых признаков
- [ ] Сессия начата одним, продолжена другим — boundary внутри session

### L2.T6 — Weekly windows

- [ ] Turn попадает ровно на reset anchor (00:00) — assigned to следующая неделя
- [ ] Anchor override через config — корректно пересчитывает все windows
- [ ] Smear/timezone: anchor в UTC, не в локальном времени
- [ ] Очень старые turns (до anchor - N лет) — windows строятся в прошлое корректно

### L2.T7 — 5h blocks

- [ ] Sessions с gap ровно 5h — спорно, выберем "≥5h → split"
- [ ] Empty session (без turns) — пропускаем
- [ ] Активный блок: now < block_end_time
- [ ] Burn rate при 0 turns < N — N/A
- [ ] Subagent turns — не считаются в users block? Или отдельный block per agent?

### L2.T8 — Plan ceilings

- [ ] Plan Unknown → не считаем %, показываем absolute tokens
- [ ] Token usage > ceiling — показываем >100% (overflow), не cap
- [ ] Ceilings конфигурируемы (на случай если Anthropic поменяет цифры)

### L2.T9 — Biome

- [ ] Session с 0 real_prompts — Plankton 🦠
- [ ] Subagent sessions — отдельный bin по `asst_count`, не `real_prompts`
- [ ] Граничные значения thresholds (например ровно 50) — стабильный класс

---

## L3: CLI commands

- [ ] Все команды поддерживают `--format json|table`
- [ ] Все команды работают на пустом индексе (выводят empty result)
- [ ] Все команды работают БЕЗ запуска `index` (auto-trigger через flag?)
- [ ] `--account ID` фильтр — корректный JOIN на accounts
- [ ] Локализация дат: ISO-8601 в JSON, human-readable в table

---

## L4: Tmux integration

- [ ] cc-stat.sh при отсутствии индекса — не падает, показывает `--`
- [ ] Cache TTL 30 сек — atomic update через mv (нет partial reads)
- [ ] Tmux читает cache <1 ms — не блокирует refresh
- [ ] Width-stable output — все статусы влезают в 80-char terminal
- [ ] Multiple tmux clients — все видят актуальные данные

---

## L5: Watch mode

- [ ] inotify на ~/.claude/projects/ — реагирует на новые файлы и append
- [ ] Daemon выживает после смены сессии (SIGHUP)
- [ ] Graceful shutdown через SIGTERM
- [ ] Restart picks up watermark из БД, не теряет данные

---

## Финальный smoke test (после L4)

После L4 запускаем сценарий:

1. `cargo install --path .` — собирается release бинарь
2. `devboy-tools-agent-usage index --full` — холодная индексация 2.1 GB
3. `devboy-tools-agent-usage blocks --active --format json` — выводит текущий блок
4. `devboy-tools-agent-usage limits --format json` — выводит weekly %
5. `devboy-tools-agent-usage statusline --format tmux` — компактная строка
6. Подключаем cc-stat.sh в `~/.tmux.conf` — видим обновления каждые 5 сек
7. Запускаем долгий Claude Code разговор → проверяем рост burn rate в статус-баре
