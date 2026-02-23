# devboy-agent-usage

CLI tool for analyzing AI agent usage (Claude Code): cost, time, tasks, focus.

Reads JSONL logs from `~/.claude/projects/`, optionally correlates with [ActivityWatch](https://activitywatch.net/) and classifies tasks via LLM.

## Features

- **Cost** — token usage and USD breakdown by day/week/month with cache write/read
- **Tasks** — auto-grouping by git branch (Jira ID, numeric ID) + LLM classification
- **Tool categories** — tool call breakdown: Read/Write/Bash/MCP/DevBoy
- **AI Title** — short task title (3-7 words) from LLM
- **Focus** — what the user was doing while Claude worked (requires ActivityWatch)
- **Browser** — visited pages analysis: work-related vs distraction
- **Timeline** — detailed per-turn session visualization with context size and compactions
- **Export** — table, JSON, CSV

## Installation

```bash
git clone https://github.com/meteora-pro/devboy-agent-usage.git
cd devboy-agent-usage
cargo build --release
```

Binary: `target/release/devboy-agent-usage`

### Requirements

- Rust 1.70+ (edition 2021)
- Claude Code (logs in `~/.claude/projects/`)
- (Optional) ActivityWatch — for focus analysis
- (Optional) LLM API — for task classification and summarization

## Quick Start

```bash
# Summary for today
devboy-agent-usage summary --from 2026-02-23

# Tasks for the week
devboy-agent-usage tasks --from 2026-02-17 --to 2026-02-23

# Tasks with LLM summarization and ActivityWatch
devboy-agent-usage tasks --from 2026-02-20 --with-llm --with-aw

# Cost by day
devboy-agent-usage cost --from 2026-02-01 --group-by day

# Session details
devboy-agent-usage session abc12345

# JSON for integrations
devboy-agent-usage tasks --from 2026-02-20 --format json
```

## Commands

### `summary` — Overview

```bash
devboy-agent-usage summary [--project NAME] [--from DATE] [--to DATE] [-f table|json|csv]
```

Outputs: session count, turns, duration, tokens (in/out/cache), cost.

### `sessions` — List Sessions

```bash
devboy-agent-usage sessions [--project NAME] [--from DATE] [--to DATE] [-l LIMIT] [-f FORMAT]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --limit` | 20 | Maximum sessions |

### `session <ID>` — Session Details

```bash
devboy-agent-usage session <SESSION_ID> [--with-llm] [-f FORMAT]
```

Shows turn-by-turn: time, model, tool calls, tokens, cost. With `--with-llm` adds chunk summaries between groups of 30 turns. With `--correlate` (enabled by default) shows user focus on each turn.

### `projects` — List Projects

```bash
devboy-agent-usage projects [-f FORMAT]
```

### `tasks` — Group by Tasks

```bash
devboy-agent-usage tasks [--project NAME] [--from DATE] [--to DATE] \
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
devboy-agent-usage timeline <TASK_ID_OR_SESSION_UUID>
```

Shows per-turn table with context size, tool call details, focus info, and compaction events. Accepts task IDs (e.g. `DEV-570`), session UUIDs, or substrings. For tasks with multiple sessions, displays a session chain with gap detection.

### `retitle` — Manual Task Title

```bash
devboy-agent-usage retitle DEV-531 "Multi-project JIRA support"
```

Title priority: manual (retitle) > LLM > none.

### `reclassify` — Re-summarize

```bash
devboy-agent-usage reclassify --from 2026-02-20 --to 2026-02-23 [--project NAME]
```

Clears summarization cache for matching tasks. Then run `tasks --with-llm` to re-summarize.

### `cost` — Cost Report

```bash
devboy-agent-usage cost [--project NAME] [--from DATE] [--to DATE] \
    [--group-by day|week|month|session] [-f FORMAT]
```

### `focus` — Focus Analysis

```bash
devboy-agent-usage focus [--project NAME] [--from DATE] [--to DATE] [-f FORMAT]
```

Requires ActivityWatch. Shows: processing time, thinking time, focus %, top apps.

### `browse <ID>` — Browser Analysis

```bash
devboy-agent-usage browse <SESSION_ID> [-f FORMAT]
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
| `TRACK_CLAUDE_LLM_PROVIDER` | `anthropic` | `anthropic` or `openai` |
| `TRACK_CLAUDE_LLM_URL` | depends on provider | API endpoint URL |
| `TRACK_CLAUDE_LLM_API_KEY` | — | API key (required for Anthropic) |
| `TRACK_CLAUDE_LLM_MODEL` | `claude-3-5-haiku-20241022` | Model |
| `ANTHROPIC_AUTH_TOKEN` | — | Alternative key for Anthropic |
| `ANTHROPIC_BASE_URL` | `https://api.z.ai/api/anthropic` | Base URL for Anthropic |

**Example with Ollama (local, free):**

```bash
export TRACK_CLAUDE_LLM_PROVIDER=openai
export TRACK_CLAUDE_LLM_URL=http://localhost:11434/v1/chat/completions
export TRACK_CLAUDE_LLM_MODEL=qwen2.5:7b
```

**Example with Anthropic:**

```bash
export TRACK_CLAUDE_LLM_API_KEY=sk-ant-...
```

### Cache

Classification and summarization results are cached in SQLite:
- macOS: `~/Library/Caches/devboy-agent-usage/classifications.db`
- Linux: `~/.cache/devboy-agent-usage/classifications.db`

Cache auto-invalidates when data changes (new turns, different hash). Manual reset — `reclassify` command.

## Models and Pricing

Supported Claude models and pricing (per 1M tokens):

| Model | Input | Output | Cache Write | Cache Read |
|-------|-------|--------|-------------|------------|
| Opus | $15.00 | $75.00 | $18.75 | $1.50 |
| Sonnet | $3.00 | $15.00 | $3.75 | $0.30 |
| Haiku | $0.80 | $4.00 | $1.00 | $0.08 |

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
