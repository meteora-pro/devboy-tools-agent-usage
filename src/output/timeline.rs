use std::collections::HashMap;

use chrono::{DateTime, Utc};
use colored::Colorize;
use comfy_table::{presets, Cell, CellAlignment, Color, ContentArrangement, Table};

use crate::claude::session::ClaudeSession;
use crate::correlation::models::{TerminalFocusStats, TurnFocusInfo};

/// Данные одной сессии для детального timeline
pub struct SessionTimelineData<'a> {
    pub session: &'a ClaudeSession,
    pub turn_focus: Option<Vec<TurnFocusInfo>>,
    /// Статистика фокуса терминала (из collect_terminal_focus_stats)
    pub terminal_stats: Option<TerminalFocusStats>,
    /// Номер сессии в цепочке (1-based)
    pub index: usize,
    /// Общее количество сессий в цепочке
    pub total: usize,
    /// Gap info от предыдущей сессии
    pub gap_info: Option<String>,
}

/// Вывести детальный timeline для задачи (одна или несколько сессий)
pub fn print_detailed_timeline(
    task_header: &str,
    sessions_data: &[SessionTimelineData],
    total_cost: f64,
) {
    // Заголовок задачи
    println!("{}", task_header.bold());
    println!();

    let mut grand_agent_secs: f64 = 0.0;
    let mut grand_human_clean_secs: f64 = 0.0;
    let mut grand_human_dirty_secs: f64 = 0.0;
    let mut grand_has_aw = false;
    let mut grand_turns: usize = 0;
    let mut grand_compactions: usize = 0;
    let mut grand_tool_counts: HashMap<String, usize> = HashMap::new();

    for sd in sessions_data {
        let session = sd.session;
        let session_id = &session.session_id.to_string()[..8];

        // Заголовок сессии
        let gap_str = sd.gap_info.as_deref().unwrap_or("");
        let gap_display = if gap_str.is_empty() {
            String::new()
        } else {
            format!(" {}", gap_str.dimmed())
        };

        println!(
            "{}",
            format!(
                "═══ Session {}/{}: {} | {} | {}→{}{}═══",
                sd.index,
                sd.total,
                session_id,
                session.project_name,
                session.start_time.format("%Y-%m-%d %H:%M"),
                session.end_time.format("%H:%M"),
                gap_display,
            )
            .bold()
        );
        println!();

        // Строим таблицу per-turn
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        table.set_content_arrangement(ContentArrangement::Dynamic);

        table.set_header(vec![
            Cell::new("#").set_alignment(CellAlignment::Right),
            Cell::new("Time"),
            Cell::new("Dur").set_alignment(CellAlignment::Right),
            Cell::new("State"),
            Cell::new("Ctx").set_alignment(CellAlignment::Right),
            Cell::new("Focus"),
            Cell::new("Description"),
        ]);

        // Собираем все timeline items (turns + compactions + human gaps) в хронологическом порядке
        let items = build_timeline_items(session, sd.turn_focus.as_deref());

        let mut session_agent_secs: f64 = 0.0;
        let mut session_compaction_count: usize = 0;

        for item in &items {
            match item {
                TimelineItem::AgentTurn {
                    turn_num,
                    time,
                    duration_secs,
                    context_tokens,
                    focus_app,
                    description,
                    tool_summary,
                } => {
                    session_agent_secs += duration_secs;
                    grand_agent_secs += duration_secs;

                    // Считаем tool calls для grand total
                    // (tool_summary уже сгруппирован, но нам нужны raw counts)

                    let ctx_str = context_tokens
                        .map(|t| format_ctx_tokens(t))
                        .unwrap_or_default();

                    let focus_str = focus_app.as_deref().unwrap_or("N/A");

                    let mut desc_lines = Vec::new();
                    if !description.is_empty() {
                        // Truncate description для таблицы
                        let short: String = description.chars().take(60).collect();
                        let truncated = if description.chars().count() > 60 {
                            format!("\"{}...\"", short)
                        } else {
                            format!("\"{}\"", short)
                        };
                        desc_lines.push(truncated);
                    }
                    if !tool_summary.is_empty() {
                        desc_lines.push(format!("→ {}", tool_summary));
                    }

                    table.add_row(vec![
                        Cell::new(turn_num.to_string())
                            .set_alignment(CellAlignment::Right)
                            .fg(Color::Cyan),
                        Cell::new(time),
                        Cell::new(format_duration_short(*duration_secs))
                            .set_alignment(CellAlignment::Right),
                        Cell::new("Agent").fg(Color::Green),
                        Cell::new(&ctx_str).set_alignment(CellAlignment::Right),
                        Cell::new(focus_str),
                        Cell::new(desc_lines.join("\n")),
                    ]);
                }
                TimelineItem::HumanGap {
                    time,
                    duration_secs,
                    focus_app,
                    is_idle,
                } => {
                    let focus_str = focus_app.as_deref().unwrap_or("");

                    let (state_label, marker) = if *is_idle {
                        ("Idle", "\u{23F8}") // ⏸
                    } else {
                        ("Human", "-")
                    };

                    table.add_row(vec![
                        Cell::new(marker)
                            .set_alignment(CellAlignment::Right)
                            .fg(Color::DarkGrey),
                        Cell::new(time).fg(Color::DarkGrey),
                        Cell::new(format_duration_short(*duration_secs))
                            .set_alignment(CellAlignment::Right)
                            .fg(Color::DarkGrey),
                        Cell::new(state_label).fg(Color::DarkGrey),
                        Cell::new(""),
                        Cell::new(focus_str).fg(Color::DarkGrey),
                        Cell::new(""),
                    ]);
                }
                TimelineItem::Compaction {
                    time,
                    trigger,
                    pre_tokens,
                } => {
                    session_compaction_count += 1;
                    grand_compactions += 1;

                    let ctx_str = pre_tokens
                        .map(|t| format!("{}→", format_ctx_tokens(t)))
                        .unwrap_or_default();

                    table.add_row(vec![
                        Cell::new("\u{27F3}")
                            .set_alignment(CellAlignment::Right)
                            .fg(Color::Yellow),
                        Cell::new(time).fg(Color::Yellow),
                        Cell::new("").fg(Color::Yellow),
                        Cell::new("Compact").fg(Color::Yellow),
                        Cell::new(&ctx_str)
                            .set_alignment(CellAlignment::Right)
                            .fg(Color::Yellow),
                        Cell::new(""),
                        Cell::new(trigger).fg(Color::Yellow),
                    ]);
                }
            }
        }

        println!("{table}");

        // Per-session summary с данными из TerminalFocusStats (если есть AW)
        let mut summary_parts = vec![
            format!("Agent {}", format_duration_short(session_agent_secs)),
        ];

        if let Some(ref ts) = sd.terminal_stats {
            grand_has_aw = true;
            // Clean human time: фокус на ЭТОМ терминале, не AFK
            summary_parts.push(format!("Human {}", format_duration_short(ts.human_focused_secs)));
            // Dirty human time: не AFK пока агент работал (любое приложение)
            summary_parts.push(format!("Dirty {}", format_duration_short(ts.dirty_human_secs)));
            grand_human_clean_secs += ts.human_focused_secs;
            grand_human_dirty_secs += ts.dirty_human_secs;

            // Focus %: время на этом терминале / total processing
            if ts.total_processing_secs > 0.0 {
                let focus_pct = (ts.human_focused_secs / ts.total_processing_secs * 100.0).min(100.0);
                summary_parts.push(format!("Focus {:.0}%", focus_pct));
            }
        } else {
            // Без AW — показываем wall time как ориентир
            let wall_secs = (session.end_time - session.start_time).num_seconds() as f64;
            summary_parts.push(format!("Wall {}", format_duration_short(wall_secs)));
        }

        if session_compaction_count > 0 {
            summary_parts.push(format!("{} compaction{}", session_compaction_count,
                if session_compaction_count > 1 { "s" } else { "" }));
        }
        println!();
        println!(
            "Session: {}",
            summary_parts.join(" | ").dimmed()
        );
        println!();

        grand_turns += session.turns.len();

        // Собираем tool calls для grand total
        for turn in &session.turns {
            for tc in &turn.tool_calls {
                *grand_tool_counts.entry(tc.clone()).or_default() += 1;
            }
        }
    }

    // Grand total
    {
        let tool_summary = format_tool_counts_from_map(&grand_tool_counts);
        let mut total_parts = vec![
            format!("Agent {}", format_duration_short(grand_agent_secs)),
        ];
        if grand_has_aw {
            total_parts.push(format!("Human {}", format_duration_short(grand_human_clean_secs)));
            total_parts.push(format!("Dirty {}", format_duration_short(grand_human_dirty_secs)));
        }
        total_parts.push(format!("{} turns", grand_turns));
        if grand_compactions > 0 {
            total_parts.push(format!("{} compactions", grand_compactions));
        }
        if !tool_summary.is_empty() {
            total_parts.push(tool_summary);
        }
        if total_cost > 0.0 {
            total_parts.push(format!("${:.2}", total_cost));
        }
        println!(
            "{}",
            format!("Total: {}", total_parts.join(" | ")).bold()
        );
    }
}

/// Элемент timeline (agent turn, human gap, или compaction)
enum TimelineItem {
    AgentTurn {
        turn_num: usize,
        time: String,
        duration_secs: f64,
        context_tokens: Option<u64>,
        focus_app: Option<String>,
        description: String,
        tool_summary: String,
    },
    HumanGap {
        time: String,
        duration_secs: f64,
        focus_app: Option<String>,
        /// Gap > 30 мин — скорее всего перерыв, не считаем как active human time
        is_idle: bool,
    },
    Compaction {
        time: String,
        trigger: String,
        pre_tokens: Option<u64>,
    },
}

/// Построить хронологический список timeline items из turns и compactions
fn build_timeline_items(
    session: &ClaudeSession,
    turn_focus: Option<&[TurnFocusInfo]>,
) -> Vec<TimelineItem> {
    let mut items: Vec<(DateTime<Utc>, TimelineItem)> = Vec::new();

    // Добавляем agent turns и human gaps
    for (i, turn) in session.turns.iter().enumerate() {
        let assistant_ts = turn.assistant_timestamp;

        let processing_secs = assistant_ts
            .map(|at| (at - turn.user_timestamp).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0)
            .max(0.0);

        // Focus info для этого turn
        // Показываем что реально делал пользователь, различая ЭТОТ терминал от других
        let focus_app = turn_focus.and_then(|f| {
            f.get(i).and_then(|fi| {
                if fi.was_afk {
                    Some("AFK".to_string())
                } else if fi.was_watching_terminal {
                    // Пользователь смотрел на ЭТОТ терминал (с Claude)
                    Some("Terminal *".to_string())
                } else {
                    // Пользователь был в другом приложении
                    fi.primary_app.clone()
                }
            })
        });

        let description = turn
            .user_message_preview
            .as_deref()
            .unwrap_or("")
            .to_string();

        let tool_summary = format_tool_calls_with_details(&turn.tool_call_details);

        items.push((
            turn.user_timestamp,
            TimelineItem::AgentTurn {
                turn_num: i + 1,
                time: turn.user_timestamp.format("%H:%M").to_string(),
                duration_secs: processing_secs,
                context_tokens: turn.context_tokens,
                focus_app,
                description,
                tool_summary,
            },
        ));

        // Human gap: от assistant_ts до next turn user_ts
        if let Some(at) = assistant_ts {
            if let Some(next_turn) = session.turns.get(i + 1) {
                let gap_secs = (next_turn.user_timestamp - at).num_seconds() as f64;
                if gap_secs > 1.0 {
                    // Focus during gap
                    let gap_focus = turn_focus.and_then(|f| {
                        f.get(i + 1).and_then(|fi| {
                            if fi.was_afk {
                                Some("AFK".to_string())
                            } else if fi.was_watching_terminal {
                                Some("Terminal *".to_string())
                            } else {
                                fi.primary_app.clone()
                            }
                        })
                    });

                    // Пометка idle для длинных промежутков (>30 мин — скорее всего перерыв)
                    let is_idle = gap_secs > 30.0 * 60.0;

                    items.push((
                        at,
                        TimelineItem::HumanGap {
                            time: at.format("%H:%M").to_string(),
                            duration_secs: gap_secs,
                            focus_app: gap_focus,
                            is_idle,
                        },
                    ));
                }
            }
        }
    }

    // Добавляем compaction events
    for c in &session.compactions {
        items.push((
            c.timestamp,
            TimelineItem::Compaction {
                time: c.timestamp.format("%H:%M").to_string(),
                trigger: c.trigger.clone(),
                pre_tokens: c.pre_tokens,
            },
        ));
    }

    // Сортируем по timestamp
    items.sort_by_key(|(ts, _)| *ts);

    items.into_iter().map(|(_, item)| item).collect()
}

/// Группировать tool calls: ["Read", "Read", "Edit", "Bash"] -> "Read ×2, Edit, Bash"
pub fn format_tool_calls(tools: &[String]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut counts: Vec<(String, usize)> = Vec::new();
    for tool in tools {
        if let Some(entry) = counts.iter_mut().find(|(name, _)| name == tool) {
            entry.1 += 1;
        } else {
            counts.push((tool.clone(), 1));
        }
    }

    counts
        .iter()
        .map(|(name, count)| {
            // Сокращаем MCP tool names
            let short_name = shorten_tool_name(name);
            if *count > 1 {
                format!("{} \u{00d7}{}", short_name, count)
            } else {
                short_name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Форматировать tool calls с деталями для timeline
///
/// Каждый tool call на отдельной строке с деталями:
/// "Read .../src/main.rs", "Bash cargo build", "Grep pattern in .../src"
fn format_tool_calls_with_details(details: &[(String, String)]) -> String {
    if details.is_empty() {
        return String::new();
    }

    // Группируем одинаковые tool calls с одинаковыми деталями
    let mut entries: Vec<(String, String, usize)> = Vec::new(); // (short_name, detail, count)
    for (name, detail) in details {
        let short_name = shorten_tool_name(name).to_string();
        if let Some(entry) = entries.iter_mut().find(|(n, d, _)| *n == short_name && *d == *detail) {
            entry.2 += 1;
        } else {
            entries.push((short_name, detail.clone(), 1));
        }
    }

    entries
        .iter()
        .map(|(name, detail, count)| {
            let count_str = if *count > 1 {
                format!(" \u{00d7}{}", count)
            } else {
                String::new()
            };
            if detail.is_empty() {
                format!("{}{}", name, count_str)
            } else {
                format!("{} {}{}", name, detail, count_str)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Форматировать HashMap tool counts в строку
fn format_tool_counts_from_map(counts: &HashMap<String, usize>) -> String {
    if counts.is_empty() {
        return String::new();
    }

    let mut sorted: Vec<(&String, &usize)> = counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));

    // Группируем по категориям
    let mut read = 0usize;
    let mut write = 0usize;
    let mut bash = 0usize;

    for (name, count) in &sorted {
        match name.as_str() {
            "Read" | "Glob" | "Grep" => read += *count,
            "Edit" | "Write" | "NotebookEdit" => write += *count,
            "Bash" => bash += *count,
            _ => {}
        }
    }

    let mut parts = Vec::new();
    if read > 0 {
        parts.push(format!("R:{}", read));
    }
    if write > 0 {
        parts.push(format!("W:{}", write));
    }
    if bash > 0 {
        parts.push(format!("B:{}", bash));
    }

    parts.join(" ")
}

/// Сокращённое имя tool (MCP tools слишком длинные)
fn shorten_tool_name(name: &str) -> &str {
    if name.starts_with("mcp__") {
        // mcp__claude_ai_dev-boy-devboy-tools-cloud__get_issues -> get_issues
        if let Some(pos) = name.rfind("__") {
            &name[pos + 2..]
        } else {
            name
        }
    } else {
        name
    }
}

/// Форматировать длительность: "45s" / "2m" / "1.5h"
pub fn format_duration_short(secs: f64) -> String {
    let s = secs.round() as i64;
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        let m = s / 60;
        let rem = s % 60;
        if rem == 0 {
            format!("{}m", m)
        } else {
            format!("{}m{}s", m, rem)
        }
    } else {
        let h = s as f64 / 3600.0;
        if (h * 10.0).round() == (h.round() * 10.0) {
            format!("{}h", h.round() as i64)
        } else {
            format!("{:.1}h", h)
        }
    }
}

/// Форматировать количество токенов: "22K" / "167K" / "1.2M"
pub fn format_ctx_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{}K", tokens / 1_000)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

/// Определить session chain info: gap description
pub fn session_chain_gap(prev_end: DateTime<Utc>, next_start: DateTime<Utc>) -> String {
    let gap_secs = (next_start - prev_end).num_seconds();
    if gap_secs < 0 {
        return String::new();
    }

    if gap_secs < 30 * 60 {
        // < 30 min
        let minutes = gap_secs / 60;
        if minutes < 1 {
            format!("(continued, gap {}s)", gap_secs)
        } else {
            format!("(continued, gap {}m)", minutes)
        }
    } else {
        // >= 30 min
        let hours = gap_secs as f64 / 3600.0;
        if hours < 1.0 {
            format!("({}m later)", gap_secs / 60)
        } else {
            format!("({:.1}h later)", hours)
        }
    }
}
