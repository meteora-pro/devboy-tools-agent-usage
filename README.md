# devboy-tools-agent-usage

CLI tool for analyzing AI agent usage (Claude Code): cost, time, tasks, focus.

Reads JSONL logs from `~/.claude/projects/`, optionally correlates with [ActivityWatch](https://activitywatch.net/) and classifies tasks via LLM.

## Features

- **Incremental SQLite index** — 2 GB of JSONL → 50 MB DB, cold ~10 sec, warm ~50 ms
- **5h rate-limit blocks** — Anthropic-style sliding blocks with burn rate + projection
- **Weekly limits** — % usage per account against plan ceiling (Pro/Max5/Max20).
  Three-tier ceiling: manual override > auto-calibrated from 429 > community estimate.
  Statusline shows `*` for community estimates (`W:5.1%*`)
- **Account detection** — auto-discovered from `~/.claude/.credentials.json` (no JWT parsing,
  no tokens stored — only SipHash of refresh token)
- **Account switching** — heuristic detection of credential changes between indexer runs
- **Biome classification** — aquarium view (🐋🦈🐬🐟🦐🦠) per session
- **tmux statusline** — width-stable one-line summary for status-bar (`🟠 6k/m $81 ⏰4h55m W:5.1% Max20`)
- **Cost** — token usage and USD breakdown by day/week/month with cache write/read
- **Tasks** — auto-grouping by git branch (Jira ID, numeric ID) + LLM classification
- **Tool categories** — tool call breakdown: Read/Write/Bash/MCP/DevBoy
- **AI Title** — short task title (3-7 words) from LLM
- **Focus** — what the user was doing while Claude worked (requires ActivityWatch)
- **Browser** — visited pages analysis: work-related vs distraction
- **Timeline** — detailed per-turn session visualization with context size and compactions
- **Export** — table, JSON, CSV

## Installation

### npm (recommended)

```bash
npm install -g @devboy-tools/agent-usage
# or
pnpm add -g @devboy-tools/agent-usage
```

### From source

```bash
git clone https://github.com/meteora-pro/devboy-tools-agent-usage.git
cd devboy-tools-agent-usage
cargo build --release
```

Binary: `target/release/devboy-tools-agent-usage`

### Requirements

- Claude Code (logs in `~/.claude/projects/`)
- (Optional) ActivityWatch — for focus analysis
- (Optional) LLM API — for task classification and summarization

## Quick Start

```bash
# Summary for today
devboy-tools-agent-usage summary --from 2026-02-23

# Tasks for the week
devboy-tools-agent-usage tasks --from 2026-02-17 --to 2026-02-23

# Tasks with LLM summarization and ActivityWatch
devboy-tools-agent-usage tasks --from 2026-02-20 --with-llm --with-aw

# Cost by day
devboy-tools-agent-usage cost --from 2026-02-01 --group-by day

# Session details
devboy-tools-agent-usage session abc12345

# JSON for integrations
devboy-tools-agent-usage tasks --from 2026-02-20 --format json
```

## Commands

### `summary` — Overview

```bash
devboy-tools-agent-usage summary [--project NAME] [--from DATE] [--to DATE] [-f table|json|csv]
```

Outputs: session count, turns, duration, tokens (in/out/cache), cost.

### `sessions` — List Sessions

```bash
devboy-tools-agent-usage sessions [--project NAME] [--from DATE] [--to DATE] [-l LIMIT] [-f FORMAT]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --limit` | 20 | Maximum sessions |

### `session <ID>` — Session Details

```bash
devboy-tools-agent-usage session <SESSION_ID> [--with-llm] [-f FORMAT]
```

Shows turn-by-turn: time, model, tool calls, tokens, cost. With `--with-llm` adds chunk summaries between groups of 30 turns. With `--correlate` (enabled by default) shows user focus on each turn.

### `projects` — List Projects

```bash
devboy-tools-agent-usage projects [-f FORMAT]
```

### `tasks` — Group by Tasks

```bash
devboy-tools-agent-usage tasks [--project NAME] [--from DATE] [--to DATE] \
    [--with-aw] [--with-llm] [--sort cost|time|sessions|recent] [-f FORMAT]
```

Groups sessions by tasks. Task ID sources (by priority):
1. **Git branch** — `feat/DEV-569-langfuse-integration` -> `DEV-569`
2. **LLM classification** — if `--with-llm`
3. **Session slug** — fallback `~slug-name`

Table columns:
- **Task** — task ID + AI title (if available)
- **Description** — from git branch suffix or LLM summary
- **Date** — date range (MM-DD or MM-DD..MM-DD)
- **Status** — completed / in_progress / blocked (with `--with-llm`)
- **Project, Sessions, Turns, Agent Time**
- **Human Time, Dirty Time** — with `--with-aw`
- **Cost** — cost in USD
- **Total, Read, Write, Bash, MCP, DevBoy** — tool calls by category

Tool categories:
| Category | Tools |
|----------|-------|
| Read | Read, Glob, Grep |
| Write | Edit, Write, NotebookEdit |
| Bash | Bash |
| MCP | all `mcp__*` |
| DevBoy | `mcp__*devboy*` or `mcp__*dev-boy*` (MCP subset) |

### `timeline <ID>` — Detailed Timeline

```bash
devboy-tools-agent-usage timeline <TASK_ID_OR_SESSION_UUID>
```

Shows per-turn table with context size, tool call details, focus info, and compaction events. Accepts task IDs (e.g. `DEV-570`), session UUIDs, or substrings. For tasks with multiple sessions, displays a session chain with gap detection.

### `retitle` — Manual Task Title

```bash
devboy-tools-agent-usage retitle DEV-531 "Multi-project JIRA support"
```

Title priority: manual (retitle) > LLM > none.

### `install` — Install AI Agent Skills

```bash
devboy-tools-agent-usage install [--global] [--force] [--agent claude,cursor,windsurf,cline,copilot]
```

Installs a skill file so AI agents know how to use devboy-tools-agent-usage. Auto-detects which agents are configured in the current directory (looks for `.claude/`, `.cursor/`, `.windsurf/`, `.clinerules`, `.github/`).

| Flag | Description |
|------|-------------|
| `-g, --global` | Install globally (Claude Code only: `~/.claude/skills/`) |
| `-f, --force` | Overwrite existing skill files |
| `-a, --agent` | Target agents (comma-separated), default: auto-detect |

Supported agents and skill paths:

| Agent | Path |
|-------|------|
| Claude Code | `.claude/skills/devboy-tools-agent-usage/SKILL.md` |
| Claude Code (global) | `~/.claude/skills/devboy-tools-agent-usage/SKILL.md` |
| Cursor | `.cursor/rules/devboy-tools-agent-usage.mdc` |
| Windsurf | `.windsurf/rules/devboy-tools-agent-usage.md` |
| Cline | `.clinerules/devboy-tools-agent-usage.md` |
| Copilot | `.github/instructions/devboy-tools-agent-usage.instructions.md` |

### `reclassify` — Re-summarize

```bash
devboy-tools-agent-usage reclassify --from 2026-02-20 --to 2026-02-23 [--project NAME]
```

Clears summarization cache for matching tasks. Then run `tasks --with-llm` to re-summarize.

### `cost` — Cost Report

```bash
devboy-tools-agent-usage cost [--project NAME] [--from DATE] [--to DATE] \
    [--group-by day|week|month|session] [-f FORMAT]
```

### `index` — Incremental SQLite Index

Builds and maintains a SQLite index over `~/.claude/projects/*.jsonl`. Cold parse runs once
(~10s on 2 GB), subsequent runs only process new/changed files via mtime+offset watermark.

```bash
devboy-tools-agent-usage index [--full] [--quiet]
```

DB location: `~/.cache/devboy-tools-agent-usage/index.db` (~50 MB for 2 GB JSONL).

Required by `blocks`, `limits`, `accounts`, `biome`, `statusline`.

### `blocks` — 5-Hour Rate-Limit Blocks

Shows 5h sliding rate-limit blocks (Anthropic semantics: block starts with first turn,
ends 5h later). Active block highlights in green.

```bash
devboy-tools-agent-usage blocks [--active] [--account ID] [--limit N] [-f FORMAT]
```

### `limits` — Weekly Usage Percentage

Per-account weekly usage with plan ceiling. Color-coded: green <70%, yellow 70-90%, red ≥90%.

```bash
devboy-tools-agent-usage limits [--account ID] [--week current|W0|all] [--limit N] [-f FORMAT]
```

Plan ceilings (best-effort): Pro 44M / Max5 88M / Max20 220M tokens per week.
Anchor: 2026-05-15 12:00 UTC (last public Anthropic flush). Override via
`CLAUDE_RESET_ANCHOR=<ISO-8601>`.

### `accounts` — Account List + Switch History

```bash
devboy-tools-agent-usage accounts [--switches] [-f FORMAT]
```

Default: list accounts with plan, first/last_seen, total turns + cost.
With `--switches`: history of account transitions with confidence (low/medium/high).

Account ID derived from `SipHash(refreshToken)` in `~/.claude/.credentials.json`.
Override via `CLAUDE_ACCOUNT=<id>`.

### `biome` — Aquarium Classification

```bash
devboy-tools-agent-usage biome [--account ID] [--from DATE] [--to DATE] [-f FORMAT]
```

Per-session biome classification by assistant turn count (matches
[`analyze-usage` skill](https://github.com/anthropics/claude-code-skills) thresholds):

| Biome | Threshold | Emoji |
|-------|-----------|-------|
| Whale | ≥500 | 🐋 |
| Shark | ≥100 | 🦈 |
| Dolphin | ≥30 | 🐬 |
| Fish | ≥10 | 🐟 |
| Shrimp | ≥3 | 🦐 |
| Plankton | <3 | 🦠 |

### `ceiling` — Honest Weekly Ceiling

Three-tier ceiling resolution:

| Source | When |
|--------|------|
| `manual` | Set via `ceiling --set N` after checking `/status` in Claude Code |
| `calibrated` | Auto-detected from `rate_limit_error` events in JSONL |
| `default-community` | Hardcoded fallback from ccusage / community estimates |

```bash
devboy-tools-agent-usage ceiling                            # show current
devboy-tools-agent-usage ceiling --set 220M --notes "..."   # manual override
devboy-tools-agent-usage ceiling --account ID -f json       # specific account
```

⚠ **The default-community ceilings (Pro=44M, Max5=88M, Max20=220M) are NOT official Anthropic numbers.**
They're educated guesses from community tools like ccusage. Anthropic publishes ceilings as "~N hours
of typical use", not raw token counts. If you want accuracy:

1. Open Claude Code: `claude`
2. Run `/status` — note the percentage shown
3. Compute reverse: ceiling = (your tokens this week) / (their percentage / 100)
4. Set it: `agent-usage ceiling --set 220M`

In tmux statusline, `W:5.1%*` (with asterisk) signals default-community. After manual override,
asterisk disappears: `W:5.1% `.

### `statusline` — Compact tmux Status

```bash
devboy-tools-agent-usage statusline [--account ID] -f tmux|raw|json
```

Width-stable one-line summary for tmux: 5h burn rate + cost + time remaining + weekly % + plan.

Example output: `🟠   6k/m $    81 ⏰4h55m W:  5.1% Max20`

Burn emoji: 🟢 <1k/min, 🟡 1-5k, 🟠 5-15k, 🔴 >15k tokens per minute.

**tmux integration**: copy `scripts/cc-stat.sh.example` to `~/.tmux/scripts/cc-stat.sh`,
then in `~/.tmux.conf`:

```tmux
set -g status-interval 5
set -g status-right "... #(~/.tmux/scripts/cc-stat.sh) ..."
```

The wrapper uses two-tier caching (5s output cache + 30s background indexer) so tmux
refresh stays under 5 ms while data stays fresh.

### `focus` — Focus Analysis

```bash
devboy-tools-agent-usage focus [--project NAME] [--from DATE] [--to DATE] [-f FORMAT]
```

Requires ActivityWatch. Shows: processing time, thinking time, focus %, top apps.

### `browse <ID>` — Browser Analysis

```bash
devboy-tools-agent-usage browse <SESSION_ID> [-f FORMAT]
```

Requires ActivityWatch. Shows visited pages, categories (GitLab, GitHub, ClickUp, Social, ...), work-related percentage.

## Configuration

### Automatic Detection

The tool automatically discovers:
- **Claude logs**: `~/.claude/projects/`
- **ActivityWatch DB**:
  - macOS: `~/Library/Application Support/activitywatch/aw-server/peewee-sqlite.v2.db`
  - Linux: `~/.local/share/activitywatch/aw-server/peewee-sqlite.v2.db`

ActivityWatch is optional — all commands work without it, except `focus` and `browse`.

### LLM for Classification

Configured via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `TRACK_CLAUDE_LLM_API_KEY` | — | API key (required for Anthropic) |
| `TRACK_CLAUDE_LLM_PROVIDER` | `anthropic` | `anthropic` or `openai` (for Ollama/LM Studio/vLLM) |
| `TRACK_CLAUDE_LLM_MODEL` | `claude-3-5-haiku-20241022` | Model name |
| `TRACK_CLAUDE_LLM_URL` | auto | Full endpoint URL (usually not needed) |
| `ANTHROPIC_AUTH_TOKEN` | — | Fallback API key for Anthropic |
| `ANTHROPIC_BASE_URL` | `https://api.z.ai/api/anthropic` | Base URL for Anthropic (appends `/v1/messages`) |

> **Note:** `TRACK_CLAUDE_LLM_URL` is the **full endpoint URL** (e.g. `https://api.z.ai/api/anthropic/v1/messages`), not a base URL. In most cases you don't need to set it — the correct URL is auto-constructed from `ANTHROPIC_BASE_URL` + `/v1/messages`. Setting it to a base URL without the path will cause "Unexpected Anthropic response format" errors.

**Example with Anthropic (simplest):**

```bash
export TRACK_CLAUDE_LLM_API_KEY=sk-ant-...
devboy-tools-agent-usage tasks --from 2026-02-20 --with-llm
```

**Example with Ollama (local, free):**

```bash
export TRACK_CLAUDE_LLM_PROVIDER=openai
export TRACK_CLAUDE_LLM_URL=http://localhost:11434/v1/chat/completions
export TRACK_CLAUDE_LLM_MODEL=qwen2.5:7b
devboy-tools-agent-usage tasks --from 2026-02-20 --with-llm
```

### Cache

Classification and summarization results are cached in SQLite:
- macOS: `~/Library/Caches/devboy-tools-agent-usage/classifications.db`
- Linux: `~/.cache/devboy-tools-agent-usage/classifications.db`

Cache auto-invalidates when data changes (new turns, different hash). Manual reset — `reclassify` command.

## Models and Pricing

Cost estimation uses a simplified flat-rate model based on 4 token types (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`) and hardcoded per-model pricing:

| Model | Input | Output | Cache Write | Cache Read |
|-------|-------|--------|-------------|------------|
| Opus | $15.00 | $75.00 | $18.75 | $1.50 |
| Sonnet | $3.00 | $15.00 | $3.75 | $0.30 |
| Haiku | $0.80 | $4.00 | $1.00 | $0.08 |

> **Note:** This is a simplified calculation. It does not account for tiered pricing (higher rates above 200K context tokens), does not use the `costUSD` field from Claude Code logs, and does not deduplicate entries. Actual costs may differ slightly. More accurate cost analysis may be implemented in the future — see [ccusage](https://github.com/ryoppippi/ccusage) for a more precise approach.

Subagent costs are included: session total is distributed proportionally across turns.

## Output Formats

All commands support `--format`:
- **table** (default) — colored table in terminal
- **json** — pretty-printed JSON
- **csv** — for import to Excel/Google Sheets

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed architecture description.

## License

Apache-2.0
