# Architecture — devboy-agent-usage

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                         CLI (clap)                          │
│  summary | sessions | session | tasks | cost | focus | ...  │
└─────────────────────┬───────────────────────────────────────┘
                      │
┌─────────────────────▼───────────────────────────────────────┐
│                   output/commands.rs                         │
│            Data loading, filtering, orchestration            │
└──────┬──────────────┬──────────────────┬────────────────────┘
       │              │                  │
┌──────▼──────┐ ┌─────▼──────┐ ┌────────▼────────┐
│   claude/   │ │ activity/  │ │ classification/  │
│  JSONL      │ │ActivityWatch│ │   LLM + cache   │
│  parsing    │ │  SQLite    │ │  classification  │
└──────┬──────┘ └─────┬──────┘ └────────┬────────┘
       │              │                  │
┌──────▼──────────────▼──────────────────▼────────────────────┐
│                    correlation/                               │
│         Session correlation with AW, task grouping           │
└─────────────────────┬───────────────────────────────────────┘
                      │
┌─────────────────────▼───────────────────────────────────────┐
│                      output/                                 │
│              table.rs | json.rs | timeline.rs                │
└─────────────────────────────────────────────────────────────┘
```

## Modules

### `src/main.rs`

Entry point. Parses CLI args via `clap::Parser`, creates `Config`, routes commands.

### `src/cli.rs`

All commands and flags defined via clap derive macros. Enum `Commands` with variants: Summary, Sessions, Session, Projects, Tasks, Reclassify, Retitle, Cost, Focus, Timeline, Browse.

### `src/config.rs`

Auto-detect paths:
- `claude_projects_dir` — `~/.claude/projects/` (required)
- `activitywatch_db_path` — platform-dependent path to ActivityWatch SQLite DB (optional)

## Data Pipeline

### 1. Claude Log Parsing (`src/claude/`)

```
~/.claude/projects/-<encoded-path>/*.jsonl
        │
        ▼
    parser.rs: discover_jsonl_files() + parse_jsonl_file()
        │
        ▼  Vec<(FileInfo, Vec<Event>)>
    session.rs: build_sessions()
        │
        ▼  Vec<ClaudeSession>
```

**`parser.rs`** — JSONL file discovery and parsing:
- Glob over `~/.claude/projects/**/*.jsonl`
- Deserialize each line as JSON event
- Event types: `user`, `assistant`, `progress`, `system`, `summary`

**`session.rs`** — session building:
- Group events by `session_id`
- Form turns (user → assistant pairs)
- Extract: tool_calls from ToolUse blocks, usage from token stats, git_branch, slug, model
- Extract tool_call_details with file paths, patterns, commands
- Parse compact_boundary system events into compactions
- Compute context_tokens (input_tokens + cache_read_input_tokens) per turn
- Aggregate `AggregatedUsage` at session level
- Detect subagent sessions (path contains `/subagents/`)

**`tokens.rs`** — cost calculation:
- Models: Opus ($15/$75), Sonnet ($3/$15), Haiku ($0.80/$4)
- Cache: write (1.25x input) and read (0.1x input)
- Model detection by substring in name

**`models.rs`** — deserialization structs:
- `RawEvent` — universal JSON wrapper with `type` field
- `TokenUsage` — input/output/cache_creation/cache_read tokens
- `CompactMetadata` — compact_boundary event metadata (trigger, pre_tokens)
- Typed content blocks: Text, ToolUse, Thinking

### 2. ActivityWatch Data (`src/activity/`)

```
ActivityWatch SQLite DB
        │
        ▼
    db.rs: load_window_events() + load_afk_events()
        │
        ▼  Vec<AwWindowEvent>, Vec<AwAfkEvent>
```

**`db.rs`** — SQLite queries:
- Bucket discovery: `type='currentwindow'` and `type='afk'`
- Load events filtered by date
- Parse JSON data: `{app, title}` for window, `{status}` for AFK

**`models.rs`** — data models:
- `AppCategory`: Development | Communication | Browser | Other
- `BrowserCategory`: GitLab | GitHub | ClickUp | Jira | Claude | ChatGPT | Docs | StackOverflow | DevDocs | Social | Email | Custom | Other

**`classifier.rs`** — classification:
- Apps by process name → AppCategory
- Browser titles by domains/keywords → BrowserCategory
- Browser title cleanup (remove suffixes `- Google Chrome - Profile`)

### 3. Classification and Summarization (`src/classification/`)

```
                  ┌─────────────┐
                  │ Classifier  │
                  │  (mod.rs)   │
                  └──────┬──────┘
                         │
           ┌─────────────┼─────────────┐
           ▼             ▼             ▼
    ┌──────────┐  ┌──────────┐  ┌──────────┐
    │  cache   │  │  client  │  │  config  │
    │ SQLite   │  │ LLM API  │  │ env vars │
    └──────────┘  └──────────┘  └──────────┘
```

**`config.rs`** — LLM provider configuration:
- `LlmProvider`: Anthropic | OpenAiCompatible
- Read from env: `TRACK_CLAUDE_LLM_PROVIDER`, `_URL`, `_API_KEY`, `_MODEL`
- Fallback to `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_BASE_URL`
- Parameters: batch_size=20, concurrency=3, timeout=60s

**`client.rs`** — HTTP client:
- `classify_batch()` — turn classification (activity labels)
- `summarize_task()` — dialog summarization (summary + status + title)
- `summarize_task_chunk()` — chunk summarization (layer 0)
- `combine_summaries()` — combine intermediate summaries (layer 1+)
- Supports Anthropic Messages API and OpenAI Chat Completions API

**`cache.rs`** — SQLite cache:
```
┌──────────────────────────┐
│   turn_classifications   │  session_id + turn_timestamp → activity_label
├──────────────────────────┤
│     task_summaries       │  task_id + turn_count + last_ts → summary + status + title
├──────────────────────────┤
│     chunk_summaries      │  task_id + level + chunk_index → summary (hash-based invalidation)
├──────────────────────────┤
│     manual_titles        │  task_id → title (manual titles via retitle command)
└──────────────────────────┘
```

**`mod.rs`** — `Classifier` orchestrator:
- `classify_turns()`: cache → batch LLM → store (parallel via rayon)
- `summarize_tasks()`: hierarchical summarization with progress bar
- `get_manual_titles()`: manual titles from cache

#### Hierarchical Summarization

```
Turns (N > 30):
┌───────┬───────┬───────┬───────┐
│ chunk │ chunk │ chunk │ chunk │  Layer 0: 30 turns each
│  0    │  1    │  2    │  3    │  → 4 intermediate summaries
└───┬───┴───┬───┴───┬───┴───┬───┘
    │       │       │       │
    └───────┴───┬───┴───────┘
                │
         ┌──────▼──────┐
         │  combine    │  Layer 1: ≤10 summaries → 1 final
         │  (final)    │
         └─────────────┘
```

- **Fast path**: ≤30 turns — single LLM call
- **Layer 0**: split into chunks of `CHUNK_SIZE=30`
- **Layer 1+**: combine by `COMBINE_SIZE=10`
- **Caching**: content hash per chunk, auto-invalidation on change
- **Node formula**: 95 turns → 4+1=5, 469 turns → 16+2+1=19

### 4. Correlation (`src/correlation/`)

**`tasks.rs`** — task grouping:

```
Sessions
    │
    ▼  extract_task_id(git_branch)
Task ID resolution (3-level fallback):
  1. Git branch regex: feat/DEV-569-... → DEV-569
  2. LLM classification → activity label
  3. Session slug → ~slug-name
    │
    ▼  TaskAccumulator (HashMap by task_id)
Aggregation: sessions, turns, agent_time, cost, tool_calls
    │
    ▼  summarize_tasks() → TaskSummary {summary, status, title}
    │
    ▼  manual_titles override (retitle)
    │
    ▼  Vec<TaskStats>
```

Also provides `find_sessions_by_task_id()` for timeline command — searches by exact task_id, display_id, or substring match.

**Subagent cost scaling**: session.total_cost includes subagent cost, but turn-level does not. Scale factor = session_total / sum(turn_costs), applied to each turn.

**`engine.rs`** — ActivityWatch correlation:

```
ClaudeSession + AwWindowEvents + AwAfkEvents
    │
    ▼  correlate_session()
FocusPeriods:
  - Processing (user_ts → assistant_ts): what user was doing
  - UserThinking (assistant_ts → next_user_ts): reading response
    │
    ▼  FocusStats
  - focus_percentage = focused / (focused + distracted) * 100
  - top_apps by time
    │
    ▼  TerminalFocusStats
  - human_focused_secs (watching terminal, not AFK)
  - agent_autonomous_secs (agent working, user not watching)
  - dirty_human_secs (not AFK while agent processing)
```

**Terminal matching**: identifies "own" terminal by app name (Terminal, iTerm2, Alacritty, ...) and "claude" in window title.

**`models.rs`** — data structures:

```rust
TaskStats {
    task_id, title, description, project_name,
    session_count, turn_count, agent_time_secs,
    human_time_secs, dirty_human_time_secs,
    cost_usd, first_seen, last_seen,
    group_source: Branch | Llm | Session,
    status, tool_calls: ToolCallStats,
}

ToolCallStats {
    total, read, write, bash, mcp, devboy
}
```

### 5. Output (`src/output/`)

**`commands.rs`** — command implementations:
- `load_sessions()` — loading with progress bar
- `filter_sessions()` — filter by project/from/to, exclude subagents
- Each command: load → filter → aggregate → format → print
- `reclassify()` — cache cleanup for re-summarization
- `retitle()` — set manual title

**`table.rs`** — tables via `comfy_table`:
- UTF8_FULL preset with colors (green/yellow/red for statuses)
- Dynamic columns (Status — only if present, Human Time — only with --with-aw)
- TOTAL row with aggregates

**`json.rs`** — JSON via `serde_json::json!()`:
- Pretty-printed, all fields included
- Tool calls as nested object: `{"total": N, "read": N, ...}`

**`timeline.rs`** — detailed per-turn visualization:
- Per-turn table with comfy_table: turn number, time, duration, state, context size, focus, description
- Tool call details (file paths, patterns, commands)
- Compaction events with pre-token counts
- Session chain with gap detection
- Human time from ActivityWatch (clean + dirty)

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` 4 | CLI with derive macros |
| `serde` + `serde_json` | JSON deserialization |
| `chrono` | Dates and time |
| `uuid` | Session UUIDs |
| `rusqlite` (bundled) | SQLite for AW and cache |
| `comfy-table` | Terminal tables |
| `colored` | ANSI colors |
| `indicatif` | Progress bar |
| `anyhow` + `thiserror` | Error handling |
| `glob` | JSONL file discovery |
| `rayon` | Parallel classification |
| `ureq` | Sync HTTP for LLM API |
| `dirs` | Platform-dependent paths |
| `regex` | Task ID extraction from branches |

## Data Flows

### Command `tasks --with-llm --with-aw`

```
1. load_sessions()
   └→ discover JSONL → parse → build sessions

2. filter_sessions(project, from, to)
   └→ exclude subagents, apply date/project filters

3. load AW data (window_events, afk_events)

4. build_task_stats()
   ├→ Phase 0: compute_session_cost_scales()
   ├→ Phase 1: collect turns without task ID
   ├→ Phase 2: classify_turns() via Classifier (cache → LLM)
   ├→ Phase 3: accumulate per task (tool_calls.add_tool())
   ├→ Phase 4: summarize_tasks() — hierarchical LLM
   ├→ Phase 5: compute_human_times_per_task() via AW
   └→ Phase 6: resolve titles (manual > llm > None)

5. sort + format (table/json/csv)
```

### Caching

```
First run:
  turns ──[LLM]──→ classifications ──→ cache (turn_classifications)
  tasks ──[LLM]──→ summaries ──→ cache (task_summaries + chunk_summaries)

Subsequent runs:
  turns ──→ cache hit ──→ classifications (no LLM calls)
  tasks ──→ cache hit ──→ summaries (no LLM calls)

Invalidation:
  - turn_classifications: by (session_id, turn_timestamp)
  - task_summaries: by (task_id, turn_count, last_turn_ts)
  - chunk_summaries: by chunk_hash (content changed → recompute)
  - Manual: reclassify command
```

## File Structure

```
src/
├── main.rs                    # Entry point, command routing
├── cli.rs                     # CLI definitions (clap derive)
├── config.rs                  # Path auto-detection
│
├── claude/                    # Claude log parsing
│   ├── mod.rs
│   ├── models.rs              # Event, TokenUsage, content blocks
│   ├── parser.rs              # JSONL discovery and parsing
│   ├── session.rs             # ClaudeSession, Turn building
│   └── tokens.rs              # Cost calculation by model
│
├── activity/                  # ActivityWatch integration
│   ├── mod.rs
│   ├── models.rs              # AppCategory, BrowserCategory
│   ├── classifier.rs          # App and page classification
│   └── db.rs                  # SQLite queries to AW DB
│
├── classification/            # LLM classification and summarization
│   ├── mod.rs                 # Classifier — orchestrator
│   ├── config.rs              # LLM provider config (env vars)
│   ├── client.rs              # HTTP client, prompts, parsing
│   └── cache.rs               # SQLite cache
│
├── correlation/               # Correlation and grouping
│   ├── mod.rs
│   ├── models.rs              # TaskStats, ToolCallStats, FocusStats
│   ├── engine.rs              # Session correlation with AW
│   └── tasks.rs               # Task grouping
│
└── output/                    # Formatting and output
    ├── mod.rs
    ├── commands.rs             # Command implementations
    ├── table.rs                # Tables (comfy_table)
    ├── json.rs                 # JSON output
    └── timeline.rs             # Detailed per-turn timeline
```
