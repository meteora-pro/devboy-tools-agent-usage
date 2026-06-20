# Эпик: cc-proxy ⋈ transcript — транспортная экономика

> Ветка: `epic/proxy-correlation` (база: 0.6.1-линия с `src/index/` indexer).
> Цель: подключить логи cc-proxy (HTTP-уровень: очередь, конкурентность, latency,
> кэш-экономика, overload) как второй источник, **джойнить с турнами по `request_id`**,
> и добавить метрики, которые транскрипт физически не содержит.

## Контекст

`cc-proxy` (`~/projects/devboy-tmux/proxy`) — локальный observability-прокси Claude Code,
пишет JSONL на `/mnt/cold-360/cc-proxy-logs/cc-proxy.jsonl`. Каждая строка уже несёт
join-ключи `session_id` + `request_id` и `usage{cache_read/creation}`. Транскрипт
(`~/.claude/projects/<sid>.jsonl`) у assistant-сообщений несёт `requestId` + `message.usage`.

**Что джойнит ТОЛЬКО эта связка** (транскрипт не содержит): `wait_ms` (время в очереди
прокси), `dur_ms` (round-trip latency), `inflight_at_start` (глубина конкурентности),
HTTP-статус (401/429/502/529), `sse_error` (overloaded_error внутри HTTP-200 SSE).

## Принципы (зафиксированы)

- **LEFT JOIN, никогда INNER** — orphan proxy-строки (ретраи, чей `request_id` не дошёл
  до транскрипта) = сигнал троттлинга, не шум.
- **Транскрипт — authoritative по токенам/стоимости.** Прокси-токены НЕ суммируем в
  биллинг (двойной счёт); прокси `usage` используем для каузальной атрибуции внутри хода.
- **`proxy_coverage_pct`** на каждой панели — прокси работает только когда CC запущен
  через `cc-proxied.sh`, статы смещены к проксированным сессиям.
- **TTL кэша = assumption** (config `CACHE_TTL_MS=300000`), метрики — оценка, не биллинг.
- **mount-absent → log-and-skip**, никогда не ошибка индекс-прохода (как optional-AW).
- **Никогда не грузить тела запроса/ответа в БД** (PII/секреты) — только top-level скаляры.

## Фазы

| # | Что | Файлы | Migration |
|---|---|---|---|
| 0 | `turns.request_id` (join-ключ на стороне транскрипта) | `index/indexer.rs`, `index/schema.rs` | v6→v7 |
| 1 | `src/proxy/` ingest + correlation (`proxy_observations`, watermark, LEFT-JOIN view, coverage%) | new `src/proxy/`, `index/schema.rs`, wire в index-проход | v7→v8 |
| 2 | `proxy` subcommand + флагман-метрика re-cache$ + аддитивные колонки | `cli.rs`, `output/commands.rs`, `output/table.rs`, `output/timeline.rs` | — |
| 3 | drift-split (`reconcile.rs`) + concurrency-аннотации блоков/biome | `usage_api/reconcile.rs`, `blocks/`, `limits/`, `biome/` | — |
| 4 | live statusline из `cc-inflight` файла | `scripts/cc-stat.sh` | — |
| 5 | analyze-usage skill панели (gated на `proxy.parquet`) | skill `extract_proxy.py` + `render.py` | — |

## Топ-6 «уловных» метрик (только через джойн)

1. **Backpressure re-cache cost ($)** — флагман. `effective_gap = gap_ms + wait_ms > CACHE_TTL ∧ cache_create>0 ∧ prev.cache_read>0` → `waste$ = Σ cache_create × (1.25−0.1) × price`; `queue_attributable = waste$ × wait_ms/effective_gap`.
2. **Retry-storm** — orphan proxy-строки (status 429/529/502 или sse_error, `request_id` не в turns) между успешными ходами.
3. **Latency-атрибуция** — `Σwait_ms` (наша очередь) vs `Σdur_ms` (Anthropic) vs human-review (по wall-clock задачи).
4. **Польза кэша в latency** — регрессия `dur_ms/1k_in` vs cache-hit-ratio bucket.
5. **Concurrency-peak ↔ overload** — гистограмма `inflight_at_start` × overload-rate → эмпирический размер `CC_PROXY_MAX_CONCURRENCY`.
6. **Hidden-overload** — `status=200 ∧ sse_error≠null` (транскрипт видит «успех») + transport-error-drift в reconcile.

## Прототип

`~/projects/devboy-tmux/proxy/correlate.py` — standalone-прототип Phase 1-2 (джойн +
cache-vs-wait). Эмпирика на реальных данных: при текущей нагрузке hold НЕ портит кэш
(потери при wait_ms=0, не от очереди). Нативная интеграция = этот эпик.

## Источник дизайна

Multi-agent workflow `improve-catch-collection` (synthesis от 2026-06-20). Карта
архитектуры верифицирована против реального 0.6.1-кода (workflow читал 0.6.1 working tree;
`main`=0.4.0 устарел и инфраструктуры не имеет).
