use comfy_table::{presets::UTF8_FULL, Cell, Color, ContentArrangement, Table};

use crate::claude::session::{AggregatedUsage, ClaudeSession};
use crate::claude::tokens;
use crate::correlation::models::{BrowseStats, CorrelatedSession, TaskGroupSource, TaskStats, TerminalFocusStats, TurnFocusInfo};

/// Таблица списка проектов
pub fn projects_table(projects: &[(String, usize, AggregatedUsage)]) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Project", "Sessions", "Tokens (in)", "Tokens (out)", "Cost"]);

    for (name, sessions, usage) in projects {
        table.add_row(vec![
            Cell::new(name),
            Cell::new(sessions),
            Cell::new(tokens::format_tokens(usage.input_tokens)),
            Cell::new(tokens::format_tokens(usage.output_tokens)),
            Cell::new(tokens::format_cost(usage.estimated_cost_usd)),
        ]);
    }

    println!("{table}");
}

/// Таблица списка сессий
pub fn sessions_table(sessions: &[&ClaudeSession]) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "Session ID",
            "Project",
            "Date",
            "Duration",
            "Turns",
            "Tokens (in/out)",
            "Cost",
        ]);

    for session in sessions {
        let id_short = &session.session_id.to_string()[..8];
        let date = session.start_time.format("%Y-%m-%d %H:%M").to_string();
        let tok = format!(
            "{}/{}",
            tokens::format_tokens(session.total_usage.input_tokens),
            tokens::format_tokens(session.total_usage.output_tokens)
        );

        table.add_row(vec![
            Cell::new(id_short),
            Cell::new(&session.project_name),
            Cell::new(&date),
            Cell::new(session.duration_display()),
            Cell::new(session.turns.len()),
            Cell::new(&tok),
            Cell::new(tokens::format_cost(session.total_usage.estimated_cost_usd)),
        ]);
    }

    println!("{table}");
}

/// Таблица сводки
pub fn summary_table(
    total_sessions: usize,
    total_turns: usize,
    total_usage: &AggregatedUsage,
    total_duration_secs: i64,
) {
    let duration_str = if total_duration_secs >= 3600 {
        format!(
            "{}h {}m",
            total_duration_secs / 3600,
            (total_duration_secs % 3600) / 60
        )
    } else {
        format!("{}m", total_duration_secs / 60)
    };

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Metric", "Value"]);

    table.add_row(vec!["Sessions", &total_sessions.to_string()]);
    table.add_row(vec!["Total turns", &total_turns.to_string()]);
    table.add_row(vec!["Total duration", &duration_str]);
    table.add_row(vec!["Requests", &total_usage.request_count.to_string()]);
    table.add_row(vec![
        "Input tokens",
        &tokens::format_tokens(total_usage.input_tokens),
    ]);
    table.add_row(vec![
        "Output tokens",
        &tokens::format_tokens(total_usage.output_tokens),
    ]);
    table.add_row(vec![
        "Cache write tokens",
        &tokens::format_tokens(total_usage.cache_creation_tokens),
    ]);
    table.add_row(vec![
        "Cache read tokens",
        &tokens::format_tokens(total_usage.cache_read_tokens),
    ]);
    table.add_row(vec![
        "Estimated cost",
        &tokens::format_cost(total_usage.estimated_cost_usd),
    ]);

    println!("{table}");
}

/// Таблица фокуса
pub fn focus_table(sessions: &[CorrelatedSession]) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "Session",
            "Project",
            "Processing",
            "Thinking",
            "Focus %",
            "Top App",
        ]);

    for cs in sessions {
        let stats = &cs.focus_stats;
        let id_short = &cs.session.session_id.to_string()[..8];

        let processing = format_duration_secs(stats.total_processing_time_secs);
        let thinking = format_duration_secs(stats.total_thinking_time_secs);
        let focus = if stats.focus_percentage > 0.0 {
            format!("{:.0}%", stats.focus_percentage)
        } else {
            "N/A".to_string()
        };
        let top_app = stats
            .top_apps
            .first()
            .map(|(app, _)| app.as_str())
            .unwrap_or("N/A");

        let focus_cell = if stats.focus_percentage >= 70.0 {
            Cell::new(&focus).fg(Color::Green)
        } else if stats.focus_percentage >= 40.0 {
            Cell::new(&focus).fg(Color::Yellow)
        } else if stats.focus_percentage > 0.0 {
            Cell::new(&focus).fg(Color::Red)
        } else {
            Cell::new(&focus)
        };

        table.add_row(vec![
            Cell::new(id_short),
            Cell::new(&cs.session.project_name),
            Cell::new(&processing),
            Cell::new(&thinking),
            focus_cell,
            Cell::new(top_app),
        ]);
    }

    println!("{table}");

    // Общая статистика по приложениям
    let mut total_app_time: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();
    for cs in sessions {
        for (app, time) in &cs.focus_stats.top_apps {
            *total_app_time.entry(app.clone()).or_default() += time;
        }
    }

    let mut apps: Vec<(String, f64)> = total_app_time.into_iter().collect();
    apps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    if !apps.is_empty() {
        println!("\nTop applications during Claude processing:");
        let mut app_table = Table::new();
        app_table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["App", "Time", "Category"]);

        for (app, time) in apps.iter().take(10) {
            let category = crate::activity::classifier::classify_app(app);
            app_table.add_row(vec![
                Cell::new(app),
                Cell::new(format_duration_secs(*time)),
                Cell::new(category.label()),
            ]);
        }

        println!("{app_table}");
    }
}

/// Таблица стоимости
pub fn cost_table(rows: &[(String, AggregatedUsage)]) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "Period",
            "Requests",
            "Input tokens",
            "Output tokens",
            "Cache write",
            "Cache read",
            "Cost",
        ]);

    let mut total = AggregatedUsage::default();

    for (period, usage) in rows {
        table.add_row(vec![
            Cell::new(period),
            Cell::new(usage.request_count),
            Cell::new(tokens::format_tokens(usage.input_tokens)),
            Cell::new(tokens::format_tokens(usage.output_tokens)),
            Cell::new(tokens::format_tokens(usage.cache_creation_tokens)),
            Cell::new(tokens::format_tokens(usage.cache_read_tokens)),
            Cell::new(tokens::format_cost(usage.estimated_cost_usd)),
        ]);
        total.merge(usage);
    }

    // Итого
    table.add_row(vec![
        Cell::new("TOTAL").fg(Color::Cyan),
        Cell::new(total.request_count).fg(Color::Cyan),
        Cell::new(tokens::format_tokens(total.input_tokens)).fg(Color::Cyan),
        Cell::new(tokens::format_tokens(total.output_tokens)).fg(Color::Cyan),
        Cell::new(tokens::format_tokens(total.cache_creation_tokens)).fg(Color::Cyan),
        Cell::new(tokens::format_tokens(total.cache_read_tokens)).fg(Color::Cyan),
        Cell::new(tokens::format_cost(total.estimated_cost_usd)).fg(Color::Cyan),
    ]);

    println!("{table}");
}

/// Таблица задач
pub fn tasks_table(tasks: &[TaskStats], with_aw: bool) {
    let has_any_status = tasks.iter().any(|t| t.status.is_some());

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);

    // Собираем заголовки динамически
    let mut headers: Vec<&str> = vec!["Task ID", "Title"];
    headers.extend_from_slice(&["Description", "Date"]);
    if has_any_status {
        headers.push("Status");
    }
    headers.extend_from_slice(&["Project", "Session IDs", "Turns", "H.Turns", "Agent Time"]);
    if with_aw {
        headers.push("Human Time");
        headers.push("Dirty Time");
    }
    headers.extend_from_slice(&["Cost", "Total", "Read", "Write", "Bash", "MCP", "DevBoy"]);
    table.set_header(headers);

    let mut total_sessions = 0usize;
    let mut total_turns = 0usize;
    let mut total_human_turns = 0usize;
    let mut total_agent_time = 0.0f64;
    let mut total_human_time = 0.0f64;
    let mut total_dirty_time = 0.0f64;
    let mut total_cost = 0.0f64;
    let mut total_tc = crate::correlation::models::ToolCallStats::default();
    let mut has_any_human_time = false;
    let mut has_any_dirty_time = false;

    for t in tasks {
        total_sessions += t.session_count;
        total_turns += t.turn_count;
        total_human_turns += t.human_turn_count;
        total_agent_time += t.agent_time_secs;
        total_cost += t.cost_usd;
        if let Some(ht) = t.human_time_secs {
            total_human_time += ht;
            has_any_human_time = true;
        }
        if let Some(dt) = t.dirty_human_time_secs {
            total_dirty_time += dt;
            has_any_dirty_time = true;
        }
        total_tc.merge(&t.tool_calls);

        let desc = t.description.as_deref().unwrap_or("-");
        let agent_time = format_duration_secs(t.agent_time_secs);
        let cost = tokens::format_cost(t.cost_usd);

        // Task ID cell: display_id (DEV-xxx для branch, session ID для остальных)
        let id_cell = match t.group_source {
            TaskGroupSource::Session => Cell::new(&t.display_id).fg(Color::DarkGrey),
            TaskGroupSource::Llm => Cell::new(&t.display_id),
            TaskGroupSource::Branch => Cell::new(&t.display_id),
        };

        let mut row = vec![id_cell];

        // Title (всегда показываем — теперь заполняется для всех групп)
        let title_str = t.title.as_deref().unwrap_or("-");
        let title_cell = match t.group_source {
            TaskGroupSource::Session => Cell::new(title_str).fg(Color::DarkGrey),
            TaskGroupSource::Llm => Cell::new(format!("{} [AI]", title_str)),
            _ => Cell::new(title_str),
        };
        row.push(title_cell);

        // Description
        let desc_cell = match t.group_source {
            TaskGroupSource::Session => Cell::new(desc).fg(Color::DarkGrey),
            _ => Cell::new(desc),
        };
        row.push(desc_cell);

        // Дата: если first_seen и last_seen в один день — "02-22", иначе "02-20..02-22"
        let date_cell = {
            let first_day = t.first_seen.format("%m-%d").to_string();
            let last_day = t.last_seen.format("%m-%d").to_string();
            if first_day == last_day {
                Cell::new(&first_day)
            } else {
                Cell::new(format!("{}..{}", first_day, last_day))
            }
        };
        row.push(date_cell);

        if has_any_status {
            let status_cell = match t.status.as_deref() {
                Some("completed") => Cell::new("completed").fg(Color::Green),
                Some("in_progress") => Cell::new("in_progress").fg(Color::Yellow),
                Some("blocked") => Cell::new("blocked").fg(Color::Red),
                Some(other) => Cell::new(other),
                None => Cell::new("-").fg(Color::DarkGrey),
            };
            row.push(status_cell);
        }

        // Session IDs: показываем до 3 коротких ID, остальные как "+N"
        let session_ids_str = {
            let max_show = 3;
            if t.session_ids.len() <= max_show {
                t.session_ids.join(", ")
            } else {
                let shown: Vec<&str> = t.session_ids.iter().take(max_show).map(|s| s.as_str()).collect();
                format!("{} +{}", shown.join(", "), t.session_ids.len() - max_show)
            }
        };

        row.extend(vec![
            Cell::new(&t.project_name),
            Cell::new(&session_ids_str),
            Cell::new(t.turn_count),
            Cell::new(t.human_turn_count),
            Cell::new(&agent_time),
        ]);

        if with_aw {
            let human_time = t
                .human_time_secs
                .map(|h| format_duration_secs(h))
                .unwrap_or_else(|| "N/A".to_string());
            row.push(Cell::new(&human_time));

            let dirty_time = t
                .dirty_human_time_secs
                .map(|d| format_duration_secs(d))
                .unwrap_or_else(|| "N/A".to_string());
            row.push(Cell::new(&dirty_time));
        }

        row.push(Cell::new(&cost));
        row.push(Cell::new(t.tool_calls.total));
        row.push(Cell::new(t.tool_calls.read));
        row.push(Cell::new(t.tool_calls.write));
        row.push(Cell::new(t.tool_calls.bash));
        row.push(Cell::new(t.tool_calls.mcp));
        row.push(Cell::new(t.tool_calls.devboy));
        table.add_row(row);
    }

    // Строка TOTAL
    let mut total_row = vec![
        Cell::new("TOTAL").fg(Color::Cyan),
        Cell::new("").fg(Color::Cyan), // Title
    ];
    total_row.push(Cell::new("").fg(Color::Cyan)); // Description
    total_row.push(Cell::new("").fg(Color::Cyan)); // Date
    if has_any_status {
        total_row.push(Cell::new("").fg(Color::Cyan));
    }
    total_row.extend(vec![
        Cell::new("").fg(Color::Cyan),
        Cell::new(format!("{} sessions", total_sessions)).fg(Color::Cyan),
        Cell::new(total_turns).fg(Color::Cyan),
        Cell::new(total_human_turns).fg(Color::Cyan),
        Cell::new(format_duration_secs(total_agent_time)).fg(Color::Cyan),
    ]);
    if with_aw {
        let human_total = if has_any_human_time {
            format_duration_secs(total_human_time)
        } else {
            "N/A".to_string()
        };
        total_row.push(Cell::new(human_total).fg(Color::Cyan));

        let dirty_total = if has_any_dirty_time {
            format_duration_secs(total_dirty_time)
        } else {
            "N/A".to_string()
        };
        total_row.push(Cell::new(dirty_total).fg(Color::Cyan));
    }
    total_row.push(Cell::new(tokens::format_cost(total_cost)).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.total).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.read).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.write).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.bash).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.mcp).fg(Color::Cyan));
    total_row.push(Cell::new(total_tc.devboy).fg(Color::Cyan));
    table.add_row(total_row);

    println!("{table}");
}

/// Детальная таблица сессии
pub fn session_detail_table(session: &ClaudeSession) {
    println!(
        "Session: {} ({})",
        &session.session_id.to_string()[..8],
        session.slug.as_deref().unwrap_or("unknown")
    );
    println!("Project: {}", session.project_name);
    println!(
        "Time: {} → {}",
        session.start_time.format("%Y-%m-%d %H:%M:%S"),
        session.end_time.format("%H:%M:%S")
    );
    println!("Duration: {}", session.duration_display());
    if let Some(ref branch) = session.git_branch {
        println!("Branch: {}", branch);
    }
    println!();

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "#",
            "Time",
            "Wait",
            "Model",
            "Tools",
            "Tokens (in/out)",
            "Cost",
        ]);

    for (i, turn) in session.turns.iter().enumerate() {
        let time = turn.user_timestamp.format("%H:%M:%S").to_string();
        let wait = turn
            .wait_duration()
            .map(|d| format!("{}s", d.num_seconds()))
            .unwrap_or_else(|| "N/A".to_string());
        let model = turn
            .model
            .as_deref()
            .map(|m| {
                if m.contains("opus") {
                    "opus"
                } else if m.contains("haiku") {
                    "haiku"
                } else {
                    "sonnet"
                }
            })
            .unwrap_or("?");
        let tools = if turn.tool_calls.is_empty() {
            "-".to_string()
        } else {
            turn.tool_calls.join(", ")
        };
        let tok = turn
            .usage
            .as_ref()
            .map(|u| {
                format!(
                    "{}/{}",
                    tokens::format_tokens(u.input_tokens),
                    tokens::format_tokens(u.output_tokens)
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let cost = turn
            .usage
            .as_ref()
            .map(|u| {
                tokens::format_cost(crate::claude::tokens::calculate_cost(
                    u,
                    turn.model.as_deref().unwrap_or("sonnet"),
                ))
            })
            .unwrap_or_else(|| "-".to_string());

        table.add_row(vec![
            Cell::new(i + 1),
            Cell::new(&time),
            Cell::new(&wait),
            Cell::new(model),
            Cell::new(&tools),
            Cell::new(&tok),
            Cell::new(&cost),
        ]);
    }

    println!("{table}");
}

/// Расширенная детальная таблица сессии с per-turn focus и chunk summaries
pub fn session_detail_enhanced(
    session: &ClaudeSession,
    turn_focus: Option<&[TurnFocusInfo]>,
    chunk_summaries: Option<&[(usize, String, Option<String>)]>,
) {
    println!(
        "Session: {} ({})",
        &session.session_id.to_string()[..8],
        session.slug.as_deref().unwrap_or("unknown")
    );
    println!("Project: {}", session.project_name);
    println!(
        "Time: {} → {}",
        session.start_time.format("%Y-%m-%d %H:%M:%S"),
        session.end_time.format("%H:%M:%S")
    );
    println!("Duration: {}", session.duration_display());
    if let Some(ref branch) = session.git_branch {
        println!("Branch: {}", branch);
    }
    println!();

    let has_focus = turn_focus.is_some();

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);

    // Заголовки
    let mut headers: Vec<&str> = vec!["#", "Time", "Wait", "User Message", "Tools", "Model", "Cost"];
    if has_focus {
        headers.push("Focus");
    }
    table.set_header(headers);

    // Chunk summaries индексированные по диапазонам turns (каждые 30 turns)
    let chunk_size = 30usize;

    for (i, turn) in session.turns.iter().enumerate() {
        // Вставляем chunk summary перед группами по 30 turns (кроме первой)
        if i > 0 && i % chunk_size == 0 {
            if let Some(ref chunks) = chunk_summaries {
                let chunk_idx = (i / chunk_size) - 1;
                if let Some((_, summary, _)) = chunks.get(chunk_idx) {
                    let truncated = if summary.len() > 120 {
                        format!("{}...", &summary[..117])
                    } else {
                        summary.clone()
                    };
                    let sep = format!("--- Chunk {} summary: {} ---", chunk_idx + 1, truncated);
                    let col_count = if has_focus { 8 } else { 7 };
                    let mut sep_row = vec![Cell::new("").fg(Color::DarkGrey)];
                    sep_row.push(Cell::new(&sep).fg(Color::DarkGrey));
                    for _ in 2..col_count {
                        sep_row.push(Cell::new("").fg(Color::DarkGrey));
                    }
                    table.add_row(sep_row);
                }
            }
        }

        let time = turn.user_timestamp.format("%H:%M:%S").to_string();
        let wait = turn
            .wait_duration()
            .map(|d| format!("{}s", d.num_seconds()))
            .unwrap_or_else(|| "N/A".to_string());
        let model = turn
            .model
            .as_deref()
            .map(|m| {
                if m.contains("opus") {
                    "opus"
                } else if m.contains("haiku") {
                    "haiku"
                } else {
                    "sonnet"
                }
            })
            .unwrap_or("?");

        // User message (truncated)
        let user_msg = turn
            .user_message_preview
            .as_deref()
            .unwrap_or("-");
        let user_msg_short = if user_msg.len() > 50 {
            format!("{}...", &user_msg[..47])
        } else {
            user_msg.to_string()
        };

        // Tools — compact: Read(3), Edit(2), Bash(1)
        let tools = if turn.tool_calls.is_empty() {
            "-".to_string()
        } else {
            let mut tool_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
            for tc in &turn.tool_calls {
                *tool_counts.entry(tc.as_str()).or_default() += 1;
            }
            let mut sorted: Vec<(&&str, &usize)> = tool_counts.iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(a.1));
            sorted
                .iter()
                .map(|(name, count)| {
                    if **count > 1 {
                        format!("{}({})", name, count)
                    } else {
                        name.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let cost = turn
            .usage
            .as_ref()
            .map(|u| {
                tokens::format_cost(crate::claude::tokens::calculate_cost(
                    u,
                    turn.model.as_deref().unwrap_or("sonnet"),
                ))
            })
            .unwrap_or_else(|| "-".to_string());

        let mut row = vec![
            Cell::new(i + 1),
            Cell::new(&time),
            Cell::new(&wait),
            Cell::new(&user_msg_short),
            Cell::new(&tools),
            Cell::new(model),
            Cell::new(&cost),
        ];

        if has_focus {
            let focus_str = if let Some(ref focus) = turn_focus {
                if let Some(fi) = focus.get(i) {
                    if fi.was_afk {
                        "AFK".to_string()
                    } else if fi.was_watching_terminal {
                        fi.primary_app
                            .as_deref()
                            .map(|a| format!("{} (watching)", a))
                            .unwrap_or_else(|| "watching".to_string())
                    } else {
                        fi.primary_app.as_deref().unwrap_or("N/A").to_string()
                    }
                } else {
                    "N/A".to_string()
                }
            } else {
                "N/A".to_string()
            };

            let focus_cell = if let Some(ref focus) = turn_focus {
                if let Some(fi) = focus.get(i) {
                    if fi.was_afk {
                        Cell::new(&focus_str).fg(Color::Red)
                    } else if fi.was_watching_terminal {
                        Cell::new(&focus_str).fg(Color::Green)
                    } else {
                        Cell::new(&focus_str).fg(Color::Yellow)
                    }
                } else {
                    Cell::new(&focus_str)
                }
            } else {
                Cell::new(&focus_str)
            };
            row.push(focus_cell);
        }

        table.add_row(row);
    }

    // Последний chunk summary (если turns > chunk_size)
    if let Some(ref chunks) = chunk_summaries {
        let last_chunk_idx = session.turns.len() / chunk_size;
        if session.turns.len() % chunk_size != 0 || last_chunk_idx > 0 {
            // Показываем последний chunk summary если он не был показан
            let displayed_chunks = if session.turns.len() > chunk_size {
                (session.turns.len() / chunk_size) - 1
            } else {
                0
            };
            for (idx, summary, _) in chunks.iter() {
                if *idx > displayed_chunks {
                    let truncated = if summary.len() > 120 {
                        format!("{}...", &summary[..117])
                    } else {
                        summary.clone()
                    };
                    let sep = format!("--- Chunk {} summary: {} ---", idx + 1, truncated);
                    let col_count = if has_focus { 8 } else { 7 };
                    let mut sep_row = vec![Cell::new("").fg(Color::DarkGrey)];
                    sep_row.push(Cell::new(&sep).fg(Color::DarkGrey));
                    for _ in 2..col_count {
                        sep_row.push(Cell::new("").fg(Color::DarkGrey));
                    }
                    table.add_row(sep_row);
                }
            }
        }
    }

    println!("{table}");
}

/// Таблица браузерных страниц + сводка по Claude сессии
pub fn browse_table(session: &ClaudeSession, browse_stats: &BrowseStats, terminal_stats: &TerminalFocusStats) {
    println!(
        "Browser pages during session {}:\n",
        &session.session_id.to_string()[..8]
    );
    println!(
        "  Project: {}  |  Duration: {}  |  Turns: {}  |  Cost: {}",
        session.project_name,
        session.duration_display(),
        session.turns.len(),
        tokens::format_cost(session.total_usage.estimated_cost_usd),
    );
    if let Some(ref slug) = session.slug {
        println!("  Task: {}", slug);
    }
    println!();

    // Таблица страниц
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Page", "Category", "Duration", "Visits"]);

    for page in &browse_stats.pages {
        let cat_cell = match page.category.is_work_related() {
            true => Cell::new(page.category.label()).fg(Color::Green),
            false => Cell::new(page.category.label()).fg(Color::Red),
        };

        table.add_row(vec![
            Cell::new(&page.title),
            cat_cell,
            Cell::new(format_duration_secs(page.total_duration_secs)),
            Cell::new(page.visit_count),
        ]);
    }

    println!("{table}");

    // Сводка work-related vs distracted
    let work_pct = browse_stats.work_related_pct;
    let distracted_pct = 100.0 - work_pct;

    let work_color = if work_pct >= 70.0 {
        Color::Green
    } else if work_pct >= 40.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    println!();
    let mut summary_table = Table::new();
    summary_table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Metric", "Value"]);

    summary_table.add_row(vec![
        Cell::new("Work-related"),
        Cell::new(format!("{:.0}%", work_pct)).fg(work_color),
    ]);
    summary_table.add_row(vec![
        Cell::new("Distracted"),
        Cell::new(format!("{:.0}%", distracted_pct)).fg(Color::Red),
    ]);

    println!("{summary_table}");

    // Категории
    if !browse_stats.categories.is_empty() {
        println!("\nTime by category:");
        let mut cat_table = Table::new();
        cat_table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["Category", "Time", "Work?"]);

        for (cat, time) in &browse_stats.categories {
            let work_label = if cat.is_work_related() { "Yes" } else { "No" };
            cat_table.add_row(vec![
                Cell::new(cat.label()),
                Cell::new(format_duration_secs(*time)),
                Cell::new(work_label),
            ]);
        }

        println!("{cat_table}");
    }

    // Статистика фокуса терминала
    let total_session_time = terminal_stats.total_processing_secs + terminal_stats.total_thinking_secs;
    let accounted_time = terminal_stats.human_focused_secs
        + terminal_stats.agent_autonomous_secs
        + terminal_stats.other_app_secs
        + terminal_stats.afk_secs;

    if total_session_time > 0.0 {
        println!("\nTerminal focus breakdown:");
        let mut tf_table = Table::new();
        tf_table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(vec!["Metric", "Time", "%"]);

        // Используем accounted_time для процентов (нормализация при перекрытиях AW событий)
        let pct_base = if accounted_time > 0.0 { accounted_time } else { total_session_time };

        let rows: Vec<(&str, f64, Color)> = vec![
            ("Human focused (this terminal)", terminal_stats.human_focused_secs, Color::Green),
            ("Agent autonomous (processing)", terminal_stats.agent_autonomous_secs, Color::Cyan),
            ("Other apps", terminal_stats.other_app_secs, Color::Yellow),
            ("AFK", terminal_stats.afk_secs, Color::Red),
        ];

        for (label, secs, color) in &rows {
            let pct = secs / pct_base * 100.0;
            tf_table.add_row(vec![
                Cell::new(label),
                Cell::new(format_duration_secs(*secs)).fg(*color),
                Cell::new(format!("{:.0}%", pct)).fg(*color),
            ]);
        }

        println!("{tf_table}");

        // Детализация по фазам
        println!(
            "\n  Processing (Claude working): {}  |  Thinking (user reading/typing): {}",
            format_duration_secs(terminal_stats.total_processing_secs),
            format_duration_secs(terminal_stats.total_thinking_secs),
        );
    }
}

fn format_duration_secs(secs: f64) -> String {
    let s = secs as i64;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}s", s)
    }
}
