use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use indicatif::{ProgressBar, ProgressStyle};

use crate::account::detection;
use crate::activity::db;
use crate::activity::transform;
use crate::biome::engine::{self as biome_engine, Biome, BiomeFilter};
use crate::blocks::engine::{self as blocks, Block, BlockFilter};
use crate::claude::mcp_patterns;
use crate::claude::parser;
use crate::claude::session::{self, AggregatedUsage, ClaudeSession};
use crate::cli::{Agent, GroupBy, OutputFormat, StatuslineFormat, TaskSortBy};
use crate::config::Config;
use crate::correlation::engine;
use crate::correlation::tasks;
use crate::index::{indexer, schema};
use crate::limits::engine as limits_engine;
use crate::limits::weekly::{self, WeeklyWindow};
use crate::output::{json, table, timeline};
use crate::tmux_activity::{idle as tmux_idle, poller as tmux_poller, store as tmux_store};

/// Загрузить и построить сессии с прогресс-баром
fn load_sessions(config: &Config) -> Result<Vec<ClaudeSession>> {
    let files = parser::discover_jsonl_files(&config.claude_projects_dir)?;

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} files ({eta})",
        )
        .unwrap()
        .progress_chars("=>-"),
    );

    let mut parsed = Vec::new();
    for file_info in &files {
        match parser::parse_jsonl_file(&file_info.path) {
            Ok(events) if !events.is_empty() => {
                parsed.push((file_info.clone(), events));
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Ошибка: {}: {}", file_info.path.display(), e);
            }
        }
        pb.inc(1);
    }
    pb.finish_and_clear();

    let sessions = session::build_sessions(parsed);
    Ok(sessions)
}

/// Фильтрация сессий по проекту и дате
fn filter_sessions<'a>(
    sessions: &'a [ClaudeSession],
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Vec<&'a ClaudeSession> {
    let from_dt = from.and_then(parse_date);
    let to_dt = to.and_then(parse_date_end);

    sessions
        .iter()
        .filter(|s| !s.is_subagent)
        .filter(|s| {
            if let Some(p) = project {
                s.project_name.contains(p)
            } else {
                true
            }
        })
        .filter(|s| {
            if let Some(dt) = from_dt {
                s.start_time >= dt
            } else {
                true
            }
        })
        .filter(|s| {
            if let Some(dt) = to_dt {
                s.start_time <= dt
            } else {
                true
            }
        })
        .collect()
}

fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
}

fn parse_date_end(s: &str) -> Option<DateTime<Utc>> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(23, 59, 59))
        .map(|dt| dt.and_utc())
}

// ==================== Команды ====================

/// Команда: список проектов
pub fn projects(config: &Config, format: &OutputFormat) -> Result<()> {
    let sessions = load_sessions(config)?;
    let filtered: Vec<&ClaudeSession> = sessions.iter().filter(|s| !s.is_subagent).collect();

    // Группируем по проекту
    let mut project_map: HashMap<String, (usize, AggregatedUsage)> = HashMap::new();
    for session in &filtered {
        let entry = project_map
            .entry(session.project_name.clone())
            .or_insert_with(|| (0, AggregatedUsage::default()));
        entry.0 += 1;
        entry.1.merge(&session.total_usage);
    }

    let mut projects: Vec<(String, usize, AggregatedUsage)> = project_map
        .into_iter()
        .map(|(name, (count, usage))| (name, count, usage))
        .collect();
    projects.sort_by(|a, b| {
        b.2.estimated_cost_usd
            .partial_cmp(&a.2.estimated_cost_usd)
            .unwrap()
    });

    println!("Found {} projects\n", projects.len());

    match format {
        OutputFormat::Table => table::projects_table(&projects),
        OutputFormat::Json => json::projects_json(&projects),
        OutputFormat::Csv => print_csv_projects(&projects),
    }

    Ok(())
}

/// Команда: список сессий
pub fn sessions(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    format: &OutputFormat,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let mut filtered = filter_sessions(&all_sessions, project, from, to);

    // Сортируем по дате (новые сверху) и ограничиваем
    filtered.sort_by_key(|b| std::cmp::Reverse(b.start_time));
    filtered.truncate(limit);

    println!("Showing {} sessions\n", filtered.len());

    match format {
        OutputFormat::Table => table::sessions_table(&filtered),
        OutputFormat::Json => json::sessions_json(&filtered),
        OutputFormat::Csv => print_csv_sessions(&filtered),
    }

    Ok(())
}

/// Команда: сводка
pub fn summary(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let filtered = filter_sessions(&all_sessions, project, from, to);

    let total_sessions = filtered.len();
    let total_turns: usize = filtered.iter().map(|s| s.turns.len()).sum();
    let total_duration_secs: i64 = filtered.iter().map(|s| s.duration().num_seconds()).sum();

    let mut total_usage = AggregatedUsage::default();
    for s in &filtered {
        total_usage.merge(&s.total_usage);
    }

    match format {
        OutputFormat::Table => table::summary_table(
            total_sessions,
            total_turns,
            &total_usage,
            total_duration_secs,
        ),
        OutputFormat::Json => json::summary_json(
            total_sessions,
            total_turns,
            &total_usage,
            total_duration_secs,
        ),
        OutputFormat::Csv => {
            println!("sessions,turns,duration_secs,requests,input_tokens,output_tokens,cost_usd");
            println!(
                "{},{},{},{},{},{},{:.4}",
                total_sessions,
                total_turns,
                total_duration_secs,
                total_usage.request_count,
                total_usage.input_tokens,
                total_usage.output_tokens,
                total_usage.estimated_cost_usd,
            );
        }
    }

    Ok(())
}

/// Команда: детали сессии
pub fn session(
    config: &Config,
    session_id: &str,
    correlate: bool,
    with_llm: bool,
    _format: &OutputFormat,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;

    // Ищем сессию по подстроке ID
    let found = all_sessions
        .iter()
        .find(|s| s.session_id.to_string().starts_with(session_id));

    let session = match found {
        Some(s) => s,
        None => {
            anyhow::bail!("Сессия с ID '{}' не найдена", session_id);
        }
    };

    // Собираем per-turn focus если есть AW
    let turn_focus = if correlate && config.has_activitywatch() {
        let raw_window = db::load_window_events(
            &config.activitywatch_db_path,
            Some(session.start_time),
            Some(session.end_time),
        )?;
        let raw_afk = db::load_afk_events(
            &config.activitywatch_db_path,
            Some(session.start_time),
            Some(session.end_time),
        )?;

        if raw_window.is_empty() {
            None
        } else {
            let window_events = transform::flood_window(raw_window, transform::DEFAULT_PULSETIME);
            let afk_events = transform::flood_afk(raw_afk, transform::DEFAULT_PULSETIME);
            let session_clone = clone_session_for_correlation(session);
            Some(engine::collect_per_turn_focus(
                &session_clone,
                &window_events,
                &afk_events,
            ))
        }
    } else {
        None
    };

    // Загружаем chunk summaries если --with-llm
    let chunk_summaries = if with_llm {
        match crate::classification::ClassificationCache::open() {
            Ok(cache) => {
                // Определяем task_id из git branch или slug
                let task_id = session
                    .git_branch
                    .as_deref()
                    .and_then(tasks::extract_task_id)
                    .or_else(|| session.slug.as_ref().map(|s| format!("~{}", s)))
                    .unwrap_or_else(|| format!("~{}", &session.session_id.to_string()[..8]));

                let summaries = cache.get_all_chunk_summaries(&task_id);
                if summaries.is_empty() {
                    None
                } else {
                    Some(summaries)
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };

    table::session_detail_enhanced(session, turn_focus.as_deref(), chunk_summaries.as_deref());

    // Дополнительная корреляция — timeline если есть AW и нет enhanced mode
    if correlate && config.has_activitywatch() && turn_focus.is_none() {
        println!(
            "\nActivityWatch database found but no window events for this session's time range."
        );
    } else if correlate && !config.has_activitywatch() {
        println!(
            "\nActivityWatch database not found at {}",
            config.activitywatch_db_path.display()
        );
    }

    Ok(())
}

/// Команда: анализ фокуса
pub fn focus(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    if !config.has_activitywatch() {
        anyhow::bail!(
            "ActivityWatch database not found at {}",
            config.activitywatch_db_path.display()
        );
    }

    let all_sessions = load_sessions(config)?;
    let filtered = filter_sessions(&all_sessions, project, from, to);

    if filtered.is_empty() {
        println!("No sessions found matching the filters.");
        return Ok(());
    }

    // Определяем общий диапазон времени
    let from_dt = filtered.iter().map(|s| s.start_time).min().unwrap();
    let to_dt = filtered.iter().map(|s| s.end_time).max().unwrap();

    // Загружаем ActivityWatch данные один раз и flood
    let raw_window =
        db::load_window_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
    let raw_afk = db::load_afk_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
    let window_events = transform::flood_window(raw_window, transform::DEFAULT_PULSETIME);
    let afk_events = transform::flood_afk(raw_afk, transform::DEFAULT_PULSETIME);

    // Коррелируем каждую сессию
    let mut correlated_sessions = Vec::new();
    for session in filtered {
        let session_clone = clone_session_for_correlation(session);
        let correlated = engine::correlate_session(session_clone, &window_events, &afk_events);
        // Пропускаем сессии без данных корреляции
        if correlated.focus_stats.total_processing_time_secs > 0.0 {
            correlated_sessions.push(correlated);
        }
    }

    println!(
        "Focus analysis for {} sessions (with ActivityWatch data)\n",
        correlated_sessions.len()
    );

    match format {
        OutputFormat::Table => table::focus_table(&correlated_sessions),
        OutputFormat::Json => json::focus_json(&correlated_sessions),
        OutputFormat::Csv => {
            println!("session_id,project,processing_secs,thinking_secs,focus_pct");
            for cs in &correlated_sessions {
                println!(
                    "{},{},{:.0},{:.0},{:.0}",
                    &cs.session.session_id.to_string()[..8],
                    cs.session.project_name,
                    cs.focus_stats.total_processing_time_secs,
                    cs.focus_stats.total_thinking_time_secs,
                    cs.focus_stats.focus_percentage,
                );
            }
        }
    }

    Ok(())
}

/// Команда: timeline
///
/// Принимает task ID (DEV-570), session UUID или подстроку.
/// 1. Ищет по UUID substring
/// 2. Если не нашёл — ищет по task ID через find_sessions_by_task_id (cache-only)
pub fn timeline(config: &Config, id: &str) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let non_subagent: Vec<&ClaudeSession> =
        all_sessions.iter().filter(|s| !s.is_subagent).collect();

    // 1. Ищем по UUID substring (точное совпадение начала)
    let uuid_matches: Vec<&ClaudeSession> = non_subagent
        .iter()
        .filter(|s| s.session_id.to_string().starts_with(id))
        .copied()
        .collect();

    let (task_title, matched_sessions) = if !uuid_matches.is_empty() {
        // Найдена одна или несколько сессий по UUID
        let title = if uuid_matches.len() == 1 {
            format!(
                "Session {} | {}",
                &uuid_matches[0].session_id.to_string()[..8],
                uuid_matches[0].project_name,
            )
        } else {
            format!("{} sessions matching '{}'", uuid_matches.len(), id)
        };
        (title, uuid_matches)
    } else {
        // 2. Ищем по task ID через cached classifier
        let classifier = crate::classification::Classifier::new().ok();

        match tasks::find_sessions_by_task_id(id, &non_subagent, classifier.as_ref()) {
            Some((title, session_uuids)) => {
                // Находим сессии по UUID
                let sessions: Vec<&ClaudeSession> = non_subagent
                    .iter()
                    .filter(|s| session_uuids.contains(&s.session_id.to_string()))
                    .copied()
                    .collect();

                if sessions.is_empty() {
                    anyhow::bail!("Task '{}' найден, но сессии не загружены", id);
                }

                let header = format!("Task: {} | {} | {} sessions", id, title, sessions.len(),);
                (header, sessions)
            }
            None => {
                anyhow::bail!(
                    "Не найдено: '{}'. Укажите task ID (DEV-570), session UUID или подстроку.",
                    id
                );
            }
        }
    };

    // Сортируем сессии хронологически
    let mut sorted_sessions = matched_sessions;
    sorted_sessions.sort_by_key(|s| s.start_time);

    // Загружаем AW данные для всего диапазона
    let from_dt = sorted_sessions.iter().map(|s| s.start_time).min().unwrap();
    let to_dt = sorted_sessions.iter().map(|s| s.end_time).max().unwrap();

    let (window_events, afk_events) = if config.has_activitywatch() {
        let raw_w =
            db::load_window_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
        let raw_a = db::load_afk_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
        let w = transform::flood_window(raw_w, transform::DEFAULT_PULSETIME);
        let a = transform::flood_afk(raw_a, transform::DEFAULT_PULSETIME);
        (w, a)
    } else {
        (Vec::new(), Vec::new())
    };

    // Строим SessionTimelineData для каждой сессии
    let total = sorted_sessions.len();
    let mut timeline_data: Vec<timeline::SessionTimelineData> = Vec::new();
    let mut total_cost = 0.0;

    for (i, session) in sorted_sessions.iter().enumerate() {
        total_cost += session.total_usage.estimated_cost_usd;

        // Per-turn focus и terminal stats
        let (turn_focus, terminal_stats) = if !window_events.is_empty() {
            let session_clone = clone_session_for_correlation(session);
            let focus = engine::collect_per_turn_focus(&session_clone, &window_events, &afk_events);
            let stats = engine::collect_terminal_focus_stats(session, &window_events, &afk_events);
            (Some(focus), Some(stats))
        } else {
            (None, None)
        };

        // Gap info от предыдущей сессии
        let gap_info = if i > 0 {
            let prev_end = sorted_sessions[i - 1].end_time;
            let gap = timeline::session_chain_gap(prev_end, session.start_time);
            if gap.is_empty() {
                None
            } else {
                Some(gap)
            }
        } else {
            None
        };

        timeline_data.push(timeline::SessionTimelineData {
            session,
            turn_focus,
            terminal_stats,
            index: i + 1,
            total,
            gap_info,
        });
    }

    timeline::print_detailed_timeline(&task_title, &timeline_data, total_cost);

    Ok(())
}

/// Команда: анализ браузерных страниц
pub fn browse(config: &Config, session_id: &str, format: &OutputFormat) -> Result<()> {
    if !config.has_activitywatch() {
        anyhow::bail!(
            "ActivityWatch database not found at {}",
            config.activitywatch_db_path.display()
        );
    }

    let all_sessions = load_sessions(config)?;

    // Ищем сессию по подстроке ID
    let found = all_sessions
        .iter()
        .find(|s| s.session_id.to_string().starts_with(session_id));

    let session = match found {
        Some(s) => s,
        None => {
            anyhow::bail!("Сессия с ID '{}' не найдена", session_id);
        }
    };

    // Загружаем window events и AFK events за время сессии
    let raw_window = db::load_window_events(
        &config.activitywatch_db_path,
        Some(session.start_time),
        Some(session.end_time),
    )?;
    let raw_afk = db::load_afk_events(
        &config.activitywatch_db_path,
        Some(session.start_time),
        Some(session.end_time),
    )?;

    if raw_window.is_empty() {
        println!("No ActivityWatch data found for this session's time range.");
        return Ok(());
    }

    // Flood + filter_period_intersect pipeline
    let (active_window, flooded_window, flooded_afk) = transform::preprocess_active_window_events(
        raw_window,
        raw_afk,
        transform::DEFAULT_PULSETIME,
    );

    // Browse stats: только активное время (пересечение с not-afk)
    let browse_stats =
        engine::collect_browse_stats(&active_window, session.start_time, session.end_time);

    // Terminal focus stats: flooded данные (обрабатывает AFK самостоятельно)
    let session_clone = clone_session_for_correlation(session);
    let terminal_stats =
        engine::collect_terminal_focus_stats(&session_clone, &flooded_window, &flooded_afk);

    match format {
        OutputFormat::Table => table::browse_table(session, &browse_stats, &terminal_stats),
        OutputFormat::Json => json::browse_json(session, &browse_stats, &terminal_stats),
        OutputFormat::Csv => {
            println!("title,category,is_work_related,duration_secs,visits");
            for page in &browse_stats.pages {
                println!(
                    "\"{}\",{},{},{:.0},{}",
                    page.title.replace('"', "\"\""),
                    page.category.label(),
                    page.category.is_work_related(),
                    page.total_duration_secs,
                    page.visit_count,
                );
            }
        }
    }

    Ok(())
}

/// Команда: группировка сессий по задачам
#[allow(clippy::too_many_arguments)]
pub fn tasks(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    with_aw: bool,
    classifier: Option<&crate::classification::Classifier>,
    sort: &TaskSortBy,
    format: &OutputFormat,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let filtered = filter_sessions(&all_sessions, project, from, to);

    if filtered.is_empty() {
        println!("No sessions found matching the filters.");
        return Ok(());
    }

    // Опционально загружаем AW данные
    let (window_events, afk_events) = if with_aw && config.has_activitywatch() {
        let from_dt = filtered.iter().map(|s| s.start_time).min().unwrap();
        let to_dt = filtered.iter().map(|s| s.end_time).max().unwrap();

        let w = db::load_window_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
        let a = db::load_afk_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
        let flooded_w = transform::flood_window(w, transform::DEFAULT_PULSETIME);
        let flooded_a = transform::flood_afk(a, transform::DEFAULT_PULSETIME);
        (Some(flooded_w), Some(flooded_a))
    } else {
        if with_aw && !config.has_activitywatch() {
            eprintln!(
                "Warning: ActivityWatch database not found at {}",
                config.activitywatch_db_path.display()
            );
        }
        (None, None)
    };

    let mut task_stats = tasks::build_task_stats(
        &filtered,
        window_events.as_deref(),
        afk_events.as_deref(),
        classifier,
    );

    // Сортировка
    match sort {
        TaskSortBy::Cost => {
            task_stats.sort_by(|a, b| b.cost_usd.partial_cmp(&a.cost_usd).unwrap());
        }
        TaskSortBy::Time => {
            task_stats.sort_by(|a, b| b.agent_time_secs.partial_cmp(&a.agent_time_secs).unwrap());
        }
        TaskSortBy::Sessions => {
            task_stats.sort_by_key(|b| std::cmp::Reverse(b.session_count));
        }
        TaskSortBy::Recent => {
            task_stats.sort_by_key(|b| std::cmp::Reverse(b.last_seen));
        }
    }

    println!(
        "Tasks from {} sessions ({} tasks found)\n",
        filtered.len(),
        task_stats.len(),
    );

    match format {
        OutputFormat::Table => table::tasks_table(&task_stats, with_aw),
        OutputFormat::Json => json::tasks_json(&task_stats),
        OutputFormat::Csv => {
            println!("display_id,task_id,title,description,project,group_source,status,sessions,turns,human_turns,agent_time_secs,human_time_secs,dirty_human_time_secs,cost_usd,tool_calls_total,tool_calls_read,tool_calls_write,tool_calls_bash,tool_calls_mcp,tool_calls_devboy,first_seen,last_seen");
            for t in &task_stats {
                println!(
                    "{},{},{},{},{},{},{},{},{},{},{:.0},{},{},{:.4},{},{},{},{},{},{},{},{}",
                    t.display_id,
                    t.task_id,
                    t.title.as_deref().unwrap_or(""),
                    t.description.as_deref().unwrap_or(""),
                    t.project_name,
                    t.group_source.label(),
                    t.status.as_deref().unwrap_or(""),
                    t.session_count,
                    t.turn_count,
                    t.human_turn_count,
                    t.agent_time_secs,
                    t.human_time_secs
                        .map(|h| format!("{:.0}", h))
                        .unwrap_or_else(|| "".to_string()),
                    t.dirty_human_time_secs
                        .map(|d| format!("{:.0}", d))
                        .unwrap_or_else(|| "".to_string()),
                    t.cost_usd,
                    t.tool_calls.total,
                    t.tool_calls.read,
                    t.tool_calls.write,
                    t.tool_calls.bash,
                    t.tool_calls.mcp,
                    t.tool_calls.devboy,
                    t.first_seen.to_rfc3339(),
                    t.last_seen.to_rfc3339(),
                );
            }
        }
    }

    // Выводим статистику LLM вызовов (classification + summarization)
    if let Some(clf) = classifier {
        let usage = clf.get_usage_stats();
        if usage.request_count > 0 {
            println!(
                "\nLLM usage: {} requests, {} input tokens, {} output tokens",
                usage.request_count,
                crate::claude::tokens::format_tokens(usage.input_tokens),
                crate::claude::tokens::format_tokens(usage.output_tokens),
            );
        }
    }

    Ok(())
}

/// Команда: стоимость
pub fn cost(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    group_by: &GroupBy,
    format: &OutputFormat,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let filtered = filter_sessions(&all_sessions, project, from, to);

    // Группируем по периоду
    let mut groups: HashMap<String, AggregatedUsage> = HashMap::new();

    for session in &filtered {
        let key = match group_by {
            GroupBy::Day => session.start_time.format("%Y-%m-%d").to_string(),
            GroupBy::Week => {
                let iso_week = session.start_time.iso_week();
                format!("{}-W{:02}", iso_week.year(), iso_week.week())
            }
            GroupBy::Month => session.start_time.format("%Y-%m").to_string(),
            GroupBy::Session => format!(
                "{} ({})",
                &session.session_id.to_string()[..8],
                session.project_name,
            ),
        };

        groups.entry(key).or_default().merge(&session.total_usage);
    }

    let mut rows: Vec<(String, AggregatedUsage)> = groups.into_iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    println!(
        "Cost breakdown ({})\n",
        match group_by {
            GroupBy::Day => "by day",
            GroupBy::Week => "by week",
            GroupBy::Month => "by month",
            GroupBy::Session => "by session",
        }
    );

    match format {
        OutputFormat::Table => table::cost_table(&rows),
        OutputFormat::Json => json::cost_json(&rows),
        OutputFormat::Csv => {
            println!("period,requests,input_tokens,output_tokens,cache_write,cache_read,cost_usd");
            for (period, usage) in &rows {
                println!(
                    "{},{},{},{},{},{},{:.4}",
                    period,
                    usage.request_count,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_tokens,
                    usage.cache_read_tokens,
                    usage.estimated_cost_usd,
                );
            }
        }
    }

    Ok(())
}

/// Команда: очистить кеш суммаризации для пересуммаризации
pub fn reclassify(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<()> {
    let all_sessions = load_sessions(config)?;
    let filtered = filter_sessions(&all_sessions, project, from, to);

    if filtered.is_empty() {
        println!("No sessions found matching the filters.");
        return Ok(());
    }

    // Собираем уникальные task IDs
    let mut task_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for session in &filtered {
        for turn in &session.turns {
            if let Some(id) = turn.git_branch.as_deref().and_then(tasks::extract_task_id) {
                task_ids.insert(id);
            } else {
                // Fallback: session slug
                let key = session
                    .slug
                    .as_deref()
                    .map(|s| format!("~{}", s))
                    .unwrap_or_else(|| format!("~{}", &session.session_id.to_string()[..8]));
                task_ids.insert(key);
            }
        }
    }

    let task_ids_vec: Vec<String> = task_ids.into_iter().collect();
    let cache = crate::classification::ClassificationCache::open()?;
    let deleted = cache.clear_summaries_for_tasks(&task_ids_vec)?;

    println!(
        "Cleared {} cached summaries for {} tasks from {} sessions.",
        deleted,
        task_ids_vec.len(),
        filtered.len(),
    );
    println!("Run `tasks --with-llm` to re-summarize.");

    Ok(())
}

/// Команда: установить ручной заголовок задачи
pub fn retitle(task_id: &str, title: &str) -> Result<()> {
    let cache = crate::classification::ClassificationCache::open()?;
    cache.set_manual_title(task_id, title)?;
    println!("Title for '{}' set to: {}", task_id, title);
    Ok(())
}

/// Встроенный SKILL.md
const SKILL_CONTENT: &str = include_str!("../skills/SKILL.md");

/// Извлечь body из SKILL.md (всё после frontmatter '---...---')
fn skill_body() -> &'static str {
    // Ищем второй '---' (конец frontmatter)
    let content = SKILL_CONTENT.trim_start_matches("---");
    if let Some(pos) = content.find("---") {
        content[pos + 3..].trim_start_matches('\n')
    } else {
        SKILL_CONTENT
    }
}

/// Автоопределение агентов по маркерным директориям в текущей рабочей папке
fn detect_agents() -> Vec<Agent> {
    let mut agents = Vec::new();

    if PathBuf::from(".claude").is_dir() {
        agents.push(Agent::Claude);
    }
    if PathBuf::from(".cursor").is_dir() {
        agents.push(Agent::Cursor);
    }
    if PathBuf::from(".windsurf").is_dir() {
        agents.push(Agent::Windsurf);
    }
    if PathBuf::from(".clinerules").exists() {
        agents.push(Agent::Cline);
    }
    if PathBuf::from(".github").is_dir() {
        agents.push(Agent::Copilot);
    }

    // Если ничего не нашли — Claude Code по умолчанию
    if agents.is_empty() {
        agents.push(Agent::Claude);
    }

    agents
}

/// Путь для skill файла агента
fn agent_skill_path(agent: &Agent, global: bool) -> Result<PathBuf> {
    match agent {
        Agent::Claude => {
            if global {
                let home = dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
                Ok(home
                    .join(".claude")
                    .join("skills")
                    .join("devboy-tools-agent-usage")
                    .join("SKILL.md"))
            } else {
                Ok(PathBuf::from(".claude")
                    .join("skills")
                    .join("devboy-tools-agent-usage")
                    .join("SKILL.md"))
            }
        }
        Agent::Cursor => Ok(PathBuf::from(".cursor")
            .join("rules")
            .join("devboy-tools-agent-usage.mdc")),
        Agent::Windsurf => Ok(PathBuf::from(".windsurf")
            .join("rules")
            .join("devboy-tools-agent-usage.md")),
        Agent::Cline => Ok(PathBuf::from(".clinerules").join("devboy-tools-agent-usage.md")),
        Agent::Copilot => Ok(PathBuf::from(".github")
            .join("instructions")
            .join("devboy-tools-agent-usage.instructions.md")),
    }
}

/// Сгенерировать контент skill файла для агента
fn agent_skill_content(agent: &Agent) -> String {
    let body = skill_body();
    let description =
        "Analyze AI agent (Claude Code) usage — costs, tasks, time tracking, focus analysis";

    match agent {
        Agent::Claude => SKILL_CONTENT.to_string(),
        Agent::Cursor => {
            format!(
                "---\ndescription: {}\nalwaysApply: false\n---\n\n{}",
                description, body
            )
        }
        Agent::Windsurf => body.to_string(),
        Agent::Cline => {
            format!("---\ndescription: {}\n---\n\n{}", description, body)
        }
        Agent::Copilot => body.to_string(),
    }
}

/// Человекочитаемое имя агента
fn agent_label(agent: &Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Cursor => "cursor",
        Agent::Windsurf => "windsurf",
        Agent::Cline => "cline",
        Agent::Copilot => "copilot",
    }
}

/// Команда: компактный statusline для tmux.
///
/// Объединяет 5h block burn + weekly % + plan в одну width-stable строку.
/// При отсутствии данных (пустой индекс, нет активного блока) graceful
/// degradation — placeholder'ы, чтобы tmux не "прыгал".
pub fn statusline_cmd(account: Option<&str>, format: &StatuslineFormat) -> Result<()> {
    let conn = match schema::open_index() {
        Ok(c) => c,
        Err(_) => {
            // Индекс ещё не создан — печатаем placeholder без падения
            print_placeholder(format);
            return Ok(());
        }
    };
    let now_ms = Utc::now().timestamp_millis();

    let account_id: String = match account {
        Some(a) => a.to_string(),
        None => match detection::detect_current() {
            Some(info) => info.id,
            None => "(none)".to_string(),
        },
    };

    // 5h блок
    let block_filter = BlockFilter {
        account_id: Some(&account_id),
        ..Default::default()
    };
    let active = blocks::find_active(&conn, &block_filter, now_ms)
        .ok()
        .flatten();

    // Weekly usage (fallback на calibration если OAuth недоступен)
    let anchor = weekly::anchor_ms();
    let win = weekly::current_window(anchor);
    let weekly = limits_engine::usage_for(&conn, &account_id, win.clone()).ok();

    // OAuth-based real % (priority). Cache 120s — за это время в БД snapshot живёт.
    let oauth_acct = if account_id != "(none)" {
        Some(account_id.as_str())
    } else {
        None
    };
    let oauth = crate::usage_api::cache::fetch_cached(&conn, 120, oauth_acct).ok();

    // Plan
    let plan: String = conn
        .query_row(
            "SELECT plan FROM accounts WHERE id = ?",
            rusqlite::params![account_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or_else(|| "?".into());

    match format {
        StatuslineFormat::Json => {
            let v = serde_json::json!({
                "now_ms": now_ms,
                "account_id": account_id,
                "plan": plan,
                "active_block": active,
                "weekly_calibrated": weekly,
                "oauth_usage": oauth.as_ref().map(|c| serde_json::json!({
                    "five_hour_pct": c.usage.five_hour.utilization,
                    "seven_day_pct": c.usage.seven_day.utilization,
                    "seven_day_sonnet_pct": c.usage.seven_day_sonnet.as_ref().map(|s| s.utilization),
                    "source": format!("{:?}", c.source),
                    "ts_ms": c.ts_ms,
                })),
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        StatuslineFormat::Raw => {
            println!(
                "{}",
                render_oneline(&account_id, &plan, &active, &weekly, oauth.as_ref(), now_ms)
            );
        }
        StatuslineFormat::Tmux => {
            println!(
                "{}",
                render_tmux(&active, &weekly, oauth.as_ref(), &plan, now_ms)
            );
        }
    }
    Ok(())
}

fn print_placeholder(format: &StatuslineFormat) {
    match format {
        StatuslineFormat::Json => println!("{{}}"),
        StatuslineFormat::Raw => println!("(no index)"),
        StatuslineFormat::Tmux => println!("⏸ no-idx"),
    }
}

/// Width-stable строка для tmux. Формат:
///   "EMOJI  6k/m  $81  ⏰4h55m  5h:18% W:44%  Max20"
///
/// 5h:N% и W:N% — реальные числа из OAuth /api/oauth/usage (если доступен).
/// Если OAuth недоступен — fallback на calibration-based weekly с маркером *.
fn render_tmux(
    active: &Option<Block>,
    weekly: &Option<limits_engine::WeeklyUsage>,
    oauth: Option<&crate::usage_api::cache::CachedUsage>,
    plan: &str,
    now_ms: i64,
) -> String {
    let (emoji, burn_str, cost_str, time_left) = match active {
        Some(b) => {
            let burn = b.burn_rate_tpm.unwrap_or(0.0);
            let emoji = burn_emoji(burn);
            let burn_str = format_burn(burn);
            let cost_str = format!("${:>6.0}", b.cost_usd);
            let remaining_ms = (b.end_ms - now_ms).max(0);
            let h = remaining_ms / 3_600_000;
            let m = (remaining_ms % 3_600_000) / 60_000;
            let time = format!("⏰{}h{:02}m", h, m);
            (emoji, burn_str, cost_str, time)
        }
        None => (
            "⚪",
            "  ---/m".to_string(),
            "$  ---".to_string(),
            "⏰--h--m".to_string(),
        ),
    };

    // Two-section usage: 5h% + W% из OAuth endpoint (real).
    // Fallback на calibration weekly с маркером '*' если OAuth недоступен.
    let (five_str, weekly_str) = match oauth {
        Some(c) => {
            let fh = c.usage.five_hour.utilization;
            let sd = c.usage.seven_day.utilization;
            // Source marker: ! если stale (старый cache, fetch упал недавно).
            let marker = match c.source {
                crate::usage_api::cache::UsageSource::Stale => "!",
                _ => " ",
            };
            (
                format!("5h:{:>4.1}%", fh),
                format!("W:{:>4.1}%{}", sd, marker),
            )
        }
        None => {
            // Fallback на calibration-based weekly (с маркером *).
            let fallback = match weekly {
                Some(u) => match u.percent {
                    Some(p) => {
                        let marker = match u.ceiling_source.as_deref() {
                            Some("default-community") => "*",
                            _ => " ",
                        };
                        format!("W:{:>4.1}%{}", p, marker)
                    }
                    None => "W: --- ".to_string(),
                },
                None => "W: --- ".to_string(),
            };
            ("5h: ---".to_string(), fallback)
        }
    };

    format!(
        "{} {} {} {} {} {} {}",
        emoji, burn_str, cost_str, time_left, five_str, weekly_str, plan
    )
}

/// Burn rate emoji (визуальный индикатор нагрузки).
fn burn_emoji(tpm: f64) -> &'static str {
    if tpm < 1000.0 {
        "🟢"
    } else if tpm < 5000.0 {
        "🟡"
    } else if tpm < 15000.0 {
        "🟠"
    } else {
        "🔴"
    }
}

/// Format burn: "  9k/m", " 99k/m", "999k/m" — 6 символов всегда.
fn format_burn(tpm: f64) -> String {
    if tpm < 1000.0 {
        format!("{:>4.0}/m", tpm)
    } else if tpm < 1_000_000.0 {
        format!("{:>4.0}k/m", tpm / 1000.0)
    } else {
        format!("{:>4.1}M/m", tpm / 1_000_000.0)
    }
}

fn render_oneline(
    account_id: &str,
    plan: &str,
    active: &Option<Block>,
    weekly: &Option<limits_engine::WeeklyUsage>,
    oauth: Option<&crate::usage_api::cache::CachedUsage>,
    now_ms: i64,
) -> String {
    let tmux = render_tmux(active, weekly, oauth, plan, now_ms);
    format!("[{}] {}", &account_id[..account_id.len().min(8)], tmux)
}

/// Распарсить размер: "220M" → 220_000_000, "100k" → 100_000, "12345" → 12345.
pub fn parse_token_count(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("пустое значение");
    }
    let (num_part, mult): (&str, u64) = match s.chars().last() {
        Some(c) if c.eq_ignore_ascii_case(&'k') => (&s[..s.len() - 1], 1_000),
        Some('M') => (&s[..s.len() - 1], 1_000_000),
        Some('m') => (&s[..s.len() - 1], 1_000_000),
        Some(c) if c.eq_ignore_ascii_case(&'g') => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    let n: f64 = num_part
        .parse()
        .map_err(|_| anyhow::anyhow!("не могу распарсить число: {}", s))?;
    Ok((n * mult as f64).round() as u64)
}

/// Команда: reconcile — сопоставление local vs endpoint.
pub fn reconcile_cmd(
    account: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;

    let account_id: Option<String> = match account {
        Some(a) => Some(a.to_string()),
        None => detection::detect_current().map(|i| i.id),
    };

    let from_ms = from.and_then(parse_date_to_ms).unwrap_or(0);
    let to_ms = to.and_then(parse_date_to_ms).unwrap_or(i64::MAX);

    let report =
        crate::usage_api::reconcile::compute(&conn, from_ms, to_ms, account_id.as_deref())?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputFormat::Csv => {
            println!("from,to,duration_secs,delta_5h_pct,delta_7d_pct,local_tokens,tokens_per_pct_7d,drift");
            for i in &report.intervals {
                println!(
                    "{},{},{},{:+.2},{:+.2},{},{},{}",
                    iso_from_ms(i.from_ts_ms),
                    iso_from_ms(i.to_ts_ms),
                    i.duration_secs,
                    i.delta_5h_pct,
                    i.delta_7d_pct,
                    i.local_tokens,
                    i.tokens_per_pct_7d
                        .map(|x| format!("{:.0}", x))
                        .unwrap_or_default(),
                    i.drift,
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            println!("account: {}", account_id.as_deref().unwrap_or("(all)"));
            println!(
                "Σ Δ7d: {:.1}%  Σ local tokens: {}  samples: {}  drift share: {:.1}%",
                report.total_delta_7d_pct,
                report.total_local_tokens,
                report.samples_used,
                report.drift_share_pct
            );
            if let Some(m) = report.mean_tokens_per_pct {
                println!("Implied conversion: {:.0} tokens ≈ 1% weekly Δ", m);
            }
            println!();

            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec![
                "from (UTC)",
                "gap",
                "Δ7d %",
                "local tokens",
                "tok/%",
                "note",
            ]);

            for i in &report.intervals {
                let gap_str = match i.duration_secs {
                    s if s < 60 => format!("{}s", s),
                    s if s < 3600 => format!("{}m", s / 60),
                    s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
                };
                let delta_cell = if i.delta_7d_pct > 0.0 {
                    Cell::new(format!("+{:.2}", i.delta_7d_pct)).fg(Color::Yellow)
                } else if i.delta_7d_pct < 0.0 {
                    Cell::new(format!("{:.2}", i.delta_7d_pct)).fg(Color::Green)
                } else {
                    Cell::new("0.00")
                };
                let note = if i.drift {
                    Cell::new("⚠ drift").fg(Color::Red)
                } else {
                    Cell::new("")
                };
                table.add_row(vec![
                    Cell::new(iso_from_ms(i.from_ts_ms)),
                    Cell::new(gap_str),
                    delta_cell,
                    Cell::new(i.local_tokens.to_string()),
                    Cell::new(
                        i.tokens_per_pct_7d
                            .map(|x| format!("{:.0}", x))
                            .unwrap_or_else(|| "—".into()),
                    ),
                    note,
                ]);
            }
            println!("{table}");
            if report.intervals.is_empty() {
                println!("(нет пар snapshots — запустите `usage --refresh` несколько раз)");
            }
        }
    }
    Ok(())
}

/// Команда: история snapshots с delta-колонками.
fn usage_history_cmd(
    conn: &rusqlite::Connection,
    account: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    format: &OutputFormat,
) -> Result<()> {
    let from_ms = from.and_then(parse_date_to_ms).unwrap_or(0);
    let to_ms = to.and_then(parse_date_to_ms).unwrap_or(i64::MAX);
    let rows = crate::usage_api::history::list(conn, from_ms, to_ms, account, limit)?;
    let with_d = crate::usage_api::history::with_deltas(rows);

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&with_d)?);
        }
        OutputFormat::Csv => {
            println!("ts,five_hour_pct,seven_day_pct,delta_5h,delta_7d,gap_secs");
            for d in &with_d {
                println!(
                    "{},{:.1},{:.1},{},{},{}",
                    iso_from_ms(d.snapshot.ts_ms),
                    d.snapshot.five_hour_pct,
                    d.snapshot.seven_day_pct,
                    d.delta_5h.map(|x| format!("{:+.1}", x)).unwrap_or_default(),
                    d.delta_7d.map(|x| format!("{:+.1}", x)).unwrap_or_default(),
                    d.gap_secs.map(|s| s.to_string()).unwrap_or_default(),
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec!["ts (UTC)", "5h %", "Δ5h", "7d %", "Δ7d", "gap"]);
            for d in &with_d {
                let delta_5h_cell = match d.delta_5h {
                    None => Cell::new("—"),
                    Some(x) if x > 0.0 => Cell::new(format!("+{:.1}", x)).fg(Color::Yellow),
                    Some(x) if x < 0.0 => Cell::new(format!("{:.1}", x)).fg(Color::Green),
                    Some(_) => Cell::new("0.0"),
                };
                let delta_7d_cell = match d.delta_7d {
                    None => Cell::new("—"),
                    Some(x) if x > 0.0 => Cell::new(format!("+{:.1}", x)).fg(Color::Yellow),
                    Some(x) if x < 0.0 => Cell::new(format!("{:.1}", x)).fg(Color::Green),
                    Some(_) => Cell::new("0.0"),
                };
                let gap_str = match d.gap_secs {
                    None => "—".into(),
                    Some(s) if s < 60 => format!("{}s", s),
                    Some(s) if s < 3600 => format!("{}m", s / 60),
                    Some(s) => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
                };
                table.add_row(vec![
                    Cell::new(iso_from_ms(d.snapshot.ts_ms)),
                    Cell::new(format!("{:.1}%", d.snapshot.five_hour_pct)),
                    delta_5h_cell,
                    Cell::new(format!("{:.1}%", d.snapshot.seven_day_pct)),
                    delta_7d_cell,
                    Cell::new(gap_str),
                ]);
            }
            println!("{table}");
            if with_d.is_empty() {
                println!("(нет snapshots — запустите `agent-usage usage` для первого fetch)");
            }
        }
    }
    Ok(())
}

/// Команда: usage — реальные % из OAuth endpoint.
#[allow(clippy::too_many_arguments)]
pub fn usage_cmd(
    refresh: bool,
    ttl: i64,
    account: Option<&str>,
    history: bool,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;

    let account_id: Option<String> = match account {
        Some(a) => Some(a.to_string()),
        None => detection::detect_current().map(|i| i.id),
    };

    if history {
        return usage_history_cmd(&conn, account_id.as_deref(), from, to, limit, format);
    }

    let effective_ttl = if refresh { 0 } else { ttl };
    let cached =
        crate::usage_api::cache::fetch_cached(&conn, effective_ttl, account_id.as_deref())?;

    let u = &cached.usage;
    match format {
        OutputFormat::Json => {
            let v = serde_json::json!({
                "source": format!("{:?}", cached.source),
                "ts_ms": cached.ts_ms,
                "account_id": account_id,
                "usage": u,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        OutputFormat::Csv => {
            println!("metric,utilization_pct,resets_at");
            println!(
                "five_hour,{:.1},{}",
                u.five_hour.utilization,
                u.five_hour.resets_at.as_deref().unwrap_or("")
            );
            println!(
                "seven_day,{:.1},{}",
                u.seven_day.utilization,
                u.seven_day.resets_at.as_deref().unwrap_or("")
            );
            if let Some(s) = &u.seven_day_sonnet {
                println!(
                    "seven_day_sonnet,{:.1},{}",
                    s.utilization,
                    s.resets_at.as_deref().unwrap_or("")
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec!["metric", "%", "resets at (UTC)"]);

            let color_for = |p: f64| -> Color {
                if p >= 90.0 {
                    Color::Red
                } else if p >= 70.0 {
                    Color::Yellow
                } else {
                    Color::Green
                }
            };

            let mut add = |name: &str, p: f64, resets: &Option<String>| {
                table.add_row(vec![
                    Cell::new(name),
                    Cell::new(format!("{:.1}%", p)).fg(color_for(p)),
                    Cell::new(resets.as_deref().unwrap_or("—")),
                ]);
            };

            add("5h window", u.five_hour.utilization, &u.five_hour.resets_at);
            add("7d window", u.seven_day.utilization, &u.seven_day.resets_at);
            if let Some(s) = &u.seven_day_sonnet {
                add("7d Sonnet", s.utilization, &s.resets_at);
            }
            if let Some(o) = &u.seven_day_opus {
                add("7d Opus", o.utilization, &o.resets_at);
            }

            println!(
                "source: {:?}  ts: {}",
                cached.source,
                iso_from_ms(cached.ts_ms)
            );
            if let Some(a) = &account_id {
                println!("account: {}", a);
            }
            println!("{table}");

            if let Some(e) = &u.extra_usage {
                if e.is_enabled {
                    let used_pct = if e.monthly_limit > 0.0 {
                        e.used_credits / e.monthly_limit * 100.0
                    } else {
                        0.0
                    };
                    println!();
                    println!(
                        "Extra (overage budget): {} {:.2} / {:.0} = {:.1}%",
                        e.currency, e.used_credits, e.monthly_limit, used_pct,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Команда: ceiling — show или set manual override.
pub fn ceiling_cmd(
    account: Option<&str>,
    set: Option<&str>,
    notes: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;

    let account_id: String = match account {
        Some(a) => a.to_string(),
        None => match detection::detect_current() {
            Some(info) => info.id,
            None => anyhow::bail!("не удалось определить аккаунт. Используйте --account."),
        },
    };

    // SET режим
    if let Some(set_str) = set {
        let value = parse_token_count(set_str)?;
        conn.execute(
            "INSERT INTO plan_overrides
             (account_id, weekly_ceiling_tokens, source, set_at, notes)
             VALUES (?, ?, 'manual', datetime('now'), ?)
             ON CONFLICT(account_id) DO UPDATE SET
                weekly_ceiling_tokens = excluded.weekly_ceiling_tokens,
                source = excluded.source,
                set_at = excluded.set_at,
                notes = excluded.notes",
            rusqlite::params![account_id, value as i64, notes],
        )?;
        eprintln!(
            "[ceiling] account {} → manual override: {} tokens/week",
            account_id, value
        );
        return Ok(());
    }

    // SHOW режим — узнать current plan и resolve ceiling.
    let plan_str: Option<String> = conn
        .query_row(
            "SELECT plan FROM accounts WHERE id = ?",
            rusqlite::params![account_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    let plan = plan_str
        .as_deref()
        .map(crate::account::plan::Plan::parse)
        .unwrap_or(crate::account::plan::Plan::Unknown);
    let (ceiling, source) = limits_engine::resolve_ceiling(&conn, &account_id, plan)?;

    // Также подтянем set_at и notes если есть override row
    let override_meta: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT set_at, notes FROM plan_overrides WHERE account_id = ?",
            rusqlite::params![account_id],
            |r| Ok((r.get(0)?, r.get(1).ok())),
        )
        .ok();

    match format {
        OutputFormat::Json => {
            let v = serde_json::json!({
                "account_id": account_id,
                "plan": plan,
                "ceiling": ceiling,
                "source": source,
                "set_at": override_meta.as_ref().map(|(s, _)| s),
                "notes": override_meta.as_ref().and_then(|(_, n)| n.clone()),
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        _ => {
            println!("account:   {}", account_id);
            println!("plan:      {}", plan);
            match (ceiling, source) {
                (Some(c), Some(src)) => {
                    println!("ceiling:   {} tokens/week", format_token_count(c));
                    println!("source:    {}", src);
                    if src == "default-community" {
                        println!();
                        println!("⚠  Это community-estimate, не официальная цифра Anthropic.");
                        println!("   Узнайте свой реальный ceiling через `claude` → /status,");
                        println!("   затем: agent-usage ceiling --set <число>");
                    }
                    if let Some((set_at, notes)) = override_meta {
                        println!("set_at:    {}", set_at);
                        if let Some(n) = notes {
                            println!("notes:     {}", n);
                        }
                    }
                }
                _ => {
                    println!("ceiling:   неизвестен (Plan::Unknown, нет override)");
                    println!();
                    println!("Используйте: agent-usage ceiling --set <число>");
                }
            }
        }
    }
    Ok(())
}

fn format_token_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}G ({})", n as f64 / 1e9, n)
    } else if n >= 1_000_000 {
        format!("{:.0}M ({})", n as f64 / 1e6, n)
    } else if n >= 1_000 {
        format!("{:.0}K ({})", n as f64 / 1e3, n)
    } else {
        n.to_string()
    }
}

/// Команда: agent-usage activity report — аналитика по tmux_activity.
pub fn activity_report(
    from: Option<&str>,
    to: Option<&str>,
    top: usize,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;

    let from_ms = from.and_then(parse_date_to_ms).unwrap_or(0);
    let to_ms = to.and_then(parse_date_to_ms).unwrap_or(i64::MAX);

    // Total stats — distinct snapshots, range
    let (snap_count, first_ts, last_ts): (i64, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(DISTINCT ts_ms), MIN(ts_ms), MAX(ts_ms)
             FROM tmux_activity WHERE ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![from_ms, to_ms],
            |r| Ok((r.get(0)?, r.get(1).ok(), r.get(2).ok())),
        )
        .unwrap_or((0, None, None));

    if snap_count == 0 {
        match format {
            OutputFormat::Json => println!("{{\"snapshots\": 0}}"),
            _ => println!("(нет данных — запустите `agent-usage activity watch`)"),
        }
        return Ok(());
    }

    // Per-command активность (только pane_active=1, то есть видимая для пользователя).
    let mut cmd_stmt = conn.prepare(
        "SELECT command, COUNT(*) AS n
         FROM tmux_activity
         WHERE ts_ms >= ? AND ts_ms < ? AND pane_active = 1
         GROUP BY command ORDER BY n DESC LIMIT ?",
    )?;
    let commands: Vec<(String, i64)> = cmd_stmt
        .query_map(rusqlite::params![from_ms, to_ms, top as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(cmd_stmt);

    // Per-session активность
    let mut sess_stmt = conn.prepare(
        "SELECT session, COUNT(*) AS n
         FROM tmux_activity
         WHERE ts_ms >= ? AND ts_ms < ? AND pane_active = 1
         GROUP BY session ORDER BY n DESC LIMIT ?",
    )?;
    let sessions: Vec<(String, i64)> = sess_stmt
        .query_map(rusqlite::params![from_ms, to_ms, top as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .filter_map(Result::ok)
        .collect();
    drop(sess_stmt);

    // Idle stats: snapshots где idle_ms > 5 min
    let idle_threshold_ms = 5 * 60 * 1000_i64;
    let (idle_snaps, idle_known): (i64, i64) = conn
        .query_row(
            "SELECT
                COUNT(DISTINCT CASE WHEN idle_ms > ? THEN ts_ms END),
                COUNT(DISTINCT CASE WHEN idle_ms IS NOT NULL THEN ts_ms END)
             FROM tmux_activity WHERE ts_ms >= ? AND ts_ms < ?",
            rusqlite::params![idle_threshold_ms, from_ms, to_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    let total_active_count: i64 = commands.iter().map(|(_, n)| *n).sum();

    match format {
        OutputFormat::Json => {
            let v = serde_json::json!({
                "snapshots": snap_count,
                "first_ts_ms": first_ts,
                "last_ts_ms": last_ts,
                "top_commands": commands.iter().map(|(c, n)| serde_json::json!({
                    "command": c, "count": n
                })).collect::<Vec<_>>(),
                "top_sessions": sessions.iter().map(|(s, n)| serde_json::json!({
                    "session": s, "count": n
                })).collect::<Vec<_>>(),
                "idle_snapshots": idle_snaps,
                "idle_data_available": idle_known,
                "idle_threshold_ms": idle_threshold_ms,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        OutputFormat::Csv => {
            println!("kind,name,count");
            for (c, n) in &commands {
                println!("command,{},{}", c, n);
            }
            for (s, n) in &sessions {
                println!("session,{},{}", s, n);
            }
            println!("__snapshots,total,{}", snap_count);
            println!("__idle_above_threshold,count,{}", idle_snaps);
        }
        OutputFormat::Table => {
            println!(
                "Range: {} → {}  ({} snapshots)",
                first_ts.map(iso_from_ms).unwrap_or_else(|| "?".into()),
                last_ts.map(iso_from_ms).unwrap_or_else(|| "?".into()),
                snap_count,
            );
            println!();
            println!("Top commands (по активным pane'ам):");
            let max_cnt = commands.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
            for (c, n) in &commands {
                let bar_len = ((*n as f64 / max_cnt as f64) * 40.0).round() as usize;
                let pct = if total_active_count > 0 {
                    (*n as f64 / total_active_count as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "  {:<14} {:>6} ({:>5.1}%) {}",
                    c,
                    n,
                    pct,
                    "█".repeat(bar_len)
                );
            }
            println!();
            println!("Top sessions:");
            let max_s = sessions.iter().map(|(_, n)| *n).max().unwrap_or(1).max(1);
            for (s, n) in &sessions {
                let bar_len = ((*n as f64 / max_s as f64) * 40.0).round() as usize;
                println!("  {:<14} {:>6}        {}", s, n, "█".repeat(bar_len));
            }
            println!();
            if idle_known > 0 {
                let idle_pct = (idle_snaps as f64 / idle_known as f64) * 100.0;
                println!(
                    "Idle ({}+ мин): {} / {} snapshots = {:.1}%",
                    idle_threshold_ms / 60_000,
                    idle_snaps,
                    idle_known,
                    idle_pct,
                );
            } else {
                println!("Idle: данных нет (idle_ms не определён ни на одном snapshot)");
            }
        }
    }
    Ok(())
}

/// Команда: long-running daemon, snapshot каждые interval секунд.
/// Завершается по SIGTERM/SIGINT (Ctrl+C).
pub fn activity_watch(interval_secs: u64) -> Result<()> {
    if interval_secs == 0 {
        anyhow::bail!("--interval должен быть >= 1");
    }
    let conn = schema::open_index()?;
    eprintln!(
        "[activity watch] starting, interval={}s, db={}",
        interval_secs,
        schema::index_db_path()?.display()
    );

    let mut errors_streak = 0u32;
    let mut snapshot_count = 0u64;

    loop {
        let now = Utc::now().timestamp_millis();
        let panes = tmux_poller::poll().unwrap_or_default();
        let idle_ms = tmux_idle::current_idle_ms();

        if panes.is_empty() {
            // tmux server не запущен — будем ждать пока поднимется.
            // Лог только редко, чтобы не шуметь.
            if errors_streak.is_multiple_of(30) {
                eprintln!("[activity watch] tmux server не отвечает (waiting)");
            }
            errors_streak = errors_streak.saturating_add(1);
        } else {
            match tmux_store::insert_snapshot(&conn, now, &panes, idle_ms) {
                Ok(n) => {
                    snapshot_count += 1;
                    errors_streak = 0;
                    // Логируем каждые 60 итераций (по умолчанию = раз в 10 минут на interval=10s)
                    if snapshot_count.is_multiple_of(60) {
                        eprintln!(
                            "[activity watch] {} snapshots, panes={} idle_ms={:?}",
                            snapshot_count, n, idle_ms
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Warning: insert failed: {}", e);
                    errors_streak = errors_streak.saturating_add(1);
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(interval_secs));
    }
}

/// Команда: одна snapshot tmux activity. Под --dry-run только печатает.
pub fn activity_collect(dry_run: bool) -> Result<()> {
    let panes = tmux_poller::poll()?;
    let idle_ms = tmux_idle::current_idle_ms();
    let now_ms = Utc::now().timestamp_millis();

    if panes.is_empty() {
        eprintln!("[activity] tmux server не запущен или нет pane'ов");
        return Ok(());
    }

    if dry_run {
        println!("ts_ms = {}", now_ms);
        println!("idle_ms = {:?}", idle_ms);
        println!("panes ({}):", panes.len());
        for p in &panes {
            println!(
                "  {}/{}.{} active={} cmd={:<10} cwd={}",
                p.session, p.window_idx, p.pane_idx, p.pane_active, p.command, p.cwd,
            );
        }
        return Ok(());
    }

    let conn = schema::open_index()?;
    let inserted = tmux_store::insert_snapshot(&conn, now_ms, &panes, idle_ms)?;
    eprintln!(
        "[activity] inserted={} idle_ms={:?} db={}",
        inserted,
        idle_ms,
        schema::index_db_path()?.display(),
    );
    Ok(())
}

/// Команда: biome aquarium (🐋🦈🐬🐟🦐🦠).
pub fn biome_cmd(
    account: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;

    let from_ms = from.and_then(parse_date_to_ms);
    let to_ms = to.and_then(parse_date_to_ms);
    let filter = BiomeFilter {
        account_id: account,
        from_ms,
        to_ms,
    };

    let summary = biome_engine::summary(&conn, &filter)?;

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        OutputFormat::Csv => {
            println!("biome,sessions");
            for b in Biome::all() {
                let n = summary.counts.get(b.name()).copied().unwrap_or(0);
                println!("{},{}", b.name(), n);
            }
            println!("__total_sessions,{}", summary.total_sessions);
            println!("__total_turns,{}", summary.total_turns);
            println!("__total_cost,{:.4}", summary.total_cost_usd);
        }
        OutputFormat::Table => {
            // Aquarium: per-biome bar (relative scale)
            let max_n = Biome::all()
                .iter()
                .map(|b| summary.counts.get(b.name()).copied().unwrap_or(0))
                .max()
                .unwrap_or(0)
                .max(1);

            println!(
                "Sessions: {}  Turns: {}  Cost: ${:.2}",
                summary.total_sessions, summary.total_turns, summary.total_cost_usd,
            );
            println!();

            for b in Biome::all() {
                let n = summary.counts.get(b.name()).copied().unwrap_or(0);
                let bar_len = if max_n == 0 {
                    0
                } else {
                    ((n as f64 / max_n as f64) * 40.0).round() as usize
                };
                let bar = "█".repeat(bar_len);
                println!("{} {:<9} {:>4} {}", b.emoji(), b.name(), n, bar,);
            }
        }
    }
    Ok(())
}

fn parse_date_to_ms(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp_millis())
}

/// Команда: показать аккаунты или историю переключений.
pub fn accounts_cmd(switches: bool, format: &OutputFormat) -> Result<()> {
    let conn = schema::open_index()?;
    if switches {
        accounts_switches(&conn, format)
    } else {
        accounts_list(&conn, format)
    }
}

fn accounts_list(conn: &rusqlite::Connection, format: &OutputFormat) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.plan, a.first_seen_ms, a.last_seen_ms, a.notes,
                COALESCE(t.cnt, 0), COALESCE(t.cost, 0.0)
         FROM accounts a
         LEFT JOIN (
             SELECT account_id, COUNT(*) AS cnt, SUM(cost_usd) AS cost
             FROM turns
             WHERE account_id IS NOT NULL
             GROUP BY account_id
         ) t ON t.account_id = a.id
         ORDER BY a.last_seen_ms DESC",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        i64,
        f64,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1).ok(),
                r.get(2).ok(),
                r.get(3).ok(),
                r.get(4).ok(),
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    match format {
        OutputFormat::Json => {
            let json_rows: Vec<_> = rows
                .iter()
                .map(|(id, plan, first, last, notes, turns, cost)| {
                    serde_json::json!({
                        "id": id,
                        "plan": plan,
                        "first_seen_ms": first,
                        "last_seen_ms": last,
                        "notes": notes,
                        "turns": turns,
                        "cost_usd": cost,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_rows)?);
        }
        OutputFormat::Csv => {
            println!("id,plan,first_seen,last_seen,turns,cost_usd,notes");
            for (id, plan, first, last, notes, turns, cost) in &rows {
                println!(
                    "{},{},{},{},{},{:.4},{}",
                    id,
                    plan.as_deref().unwrap_or(""),
                    first.map(iso_from_ms).unwrap_or_default(),
                    last.map(iso_from_ms).unwrap_or_default(),
                    turns,
                    cost,
                    notes.as_deref().unwrap_or("").replace(',', ";"),
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec![
                "id",
                "plan",
                "first_seen",
                "last_seen",
                "turns",
                "cost",
                "notes",
            ]);
            for (id, plan, first, last, notes, turns, cost) in &rows {
                table.add_row(vec![
                    Cell::new(id),
                    Cell::new(plan.as_deref().unwrap_or("")),
                    Cell::new(first.map(iso_from_ms).unwrap_or_default()),
                    Cell::new(last.map(iso_from_ms).unwrap_or_default()),
                    Cell::new(turns.to_string()),
                    Cell::new(format!("${:.2}", cost)),
                    Cell::new(notes.as_deref().unwrap_or("")),
                ]);
            }
            let null_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM turns WHERE account_id IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            println!("{table}");
            if null_count > 0 {
                println!(
                    "({} turns без account_id — это исторические, до first detection)",
                    null_count,
                );
            }
        }
    }
    Ok(())
}

fn accounts_switches(conn: &rusqlite::Connection, format: &OutputFormat) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT ts_ms, previous_account, current_account, confidence, detected_at
         FROM account_switches ORDER BY ts_ms ASC",
    )?;
    let rows: Vec<(i64, Option<String>, String, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1).ok(), r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    match format {
        OutputFormat::Json => {
            let json_rows: Vec<_> = rows
                .iter()
                .map(|(ts, prev, cur, conf, det)| {
                    serde_json::json!({
                        "ts_ms": ts,
                        "previous_account": prev,
                        "current_account": cur,
                        "confidence": conf,
                        "detected_at": det,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_rows)?);
        }
        OutputFormat::Csv => {
            println!("ts,previous_account,current_account,confidence,detected_at");
            for (ts, prev, cur, conf, det) in &rows {
                println!(
                    "{},{},{},{},{}",
                    iso_from_ms(*ts),
                    prev.as_deref().unwrap_or(""),
                    cur,
                    conf,
                    det
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec!["when", "from", "to", "confidence"]);
            for (ts, prev, cur, conf, _) in &rows {
                let conf_cell = match conf.as_str() {
                    "high" => Cell::new(conf).fg(Color::Green),
                    "medium" => Cell::new(conf).fg(Color::Yellow),
                    _ => Cell::new(conf).fg(Color::DarkGrey),
                };
                table.add_row(vec![
                    Cell::new(iso_from_ms(*ts)),
                    Cell::new(prev.as_deref().unwrap_or("(none)")),
                    Cell::new(cur),
                    conf_cell,
                ]);
            }
            println!("{table}");
            if rows.is_empty() {
                println!("(переключений ещё не было)");
            }
        }
    }
    Ok(())
}

/// Команда: weekly limits (% usage от ceiling плана).
pub fn limits_cmd(
    account: Option<&str>,
    week: &str,
    limit: usize,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;
    let anchor = weekly::anchor_ms();

    // Решаем какой account показывать: явный, или из detection (текущий).
    let account_id: String = match account {
        Some(a) => a.to_string(),
        None => match detection::detect_current() {
            Some(info) => info.id,
            None => anyhow::bail!(
                "не удалось определить аккаунт. Используйте --account ID или установите CLAUDE_ACCOUNT."
            ),
        },
    };

    // Какие окна показать.
    let windows: Vec<WeeklyWindow> = match week {
        "current" => vec![weekly::current_window(anchor)],
        "all" => {
            let current = weekly::current_window(anchor);
            // last `limit` окон включая текущее. Если "pre" — заменяем на W0.
            if current.id == "pre" {
                vec![current]
            } else {
                let cur_n: i64 = current.id.trim_start_matches('W').parse().unwrap_or(0);
                let from = (cur_n + 1).saturating_sub(limit as i64).max(0);
                (from..=cur_n)
                    .map(|n| WeeklyWindow::nth(n, anchor))
                    .collect()
            }
        }
        s => {
            // Парсим как "W<N>" или просто "<N>"
            let n: i64 = s
                .trim_start_matches('W')
                .parse()
                .map_err(|_| anyhow::anyhow!("неверный --week: {}", s))?;
            vec![WeeklyWindow::nth(n, anchor)]
        }
    };

    let usages: Vec<_> = windows
        .into_iter()
        .map(|w| limits_engine::usage_for(&conn, &account_id, w))
        .collect::<Result<Vec<_>>>()?;

    match format {
        OutputFormat::Json => {
            let v = serde_json::json!({
                "account_id": account_id,
                "anchor_ms": anchor,
                "usages": usages,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        OutputFormat::Csv => {
            println!(
                "window,start,end,plan,used_tokens,cache_tokens,turns,cost_usd,ceiling,percent"
            );
            for u in &usages {
                println!(
                    "{},{},{},{},{},{},{},{:.4},{},{:.2}",
                    u.window.id,
                    iso_from_ms(u.window.start_ms),
                    iso_from_ms(u.window.end_ms),
                    u.plan,
                    u.used_tokens,
                    u.cache_tokens,
                    u.turns,
                    u.cost_usd,
                    u.ceiling.map(|c| c.to_string()).unwrap_or_default(),
                    u.percent.unwrap_or(0.0),
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec![
                "window", "start", "plan", "used", "ceiling", "%", "cache", "turns", "cost",
            ]);
            for u in &usages {
                let pct_cell = match u.percent {
                    Some(p) if p >= 90.0 => Cell::new(format!("{:.1}%", p)).fg(Color::Red),
                    Some(p) if p >= 70.0 => Cell::new(format!("{:.1}%", p)).fg(Color::Yellow),
                    Some(p) => Cell::new(format!("{:.1}%", p)).fg(Color::Green),
                    None => Cell::new("n/a").fg(Color::DarkGrey),
                };
                let ceiling = u
                    .ceiling
                    .map(|c| format!("{:>11}", c))
                    .unwrap_or_else(|| "n/a".into());
                table.add_row(vec![
                    Cell::new(&u.window.id),
                    Cell::new(u.window.start_date_utc()),
                    Cell::new(format!("{}", u.plan)),
                    Cell::new(format!("{:>11}", u.used_tokens)),
                    Cell::new(ceiling),
                    pct_cell,
                    Cell::new(format!("{}", u.cache_tokens)),
                    Cell::new(u.turns.to_string()),
                    Cell::new(format!("${:.2}", u.cost_usd)),
                ]);
            }
            println!("account: {}", account_id);
            println!(
                "anchor:  {} ({})",
                iso_from_ms(anchor),
                weekly::DEFAULT_ANCHOR_ISO
            );
            println!("{table}");
        }
    }
    Ok(())
}

/// Команда: показать 5-часовые rate-limit блоки.
pub fn blocks_cmd(
    active_only: bool,
    account: Option<&str>,
    limit: usize,
    format: &OutputFormat,
) -> Result<()> {
    let conn = schema::open_index()?;
    let filter = BlockFilter {
        account_id: account,
        ..Default::default()
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut all = blocks::build_blocks_at(&conn, &filter, now_ms)?;

    let to_show: Vec<Block> = if active_only {
        all.into_iter().filter(|b| b.is_active).collect()
    } else {
        if all.len() > limit {
            all.drain(..all.len() - limit);
        }
        all
    };

    match format {
        OutputFormat::Json => {
            let v = serde_json::json!({
                "now_ms": now_ms,
                "blocks": to_show,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        OutputFormat::Csv => {
            println!("start_iso,end_iso,is_active,turns,input,output,cache_create,cache_read,cost_usd,burn_rate_tpm,account");
            for b in &to_show {
                println!(
                    "{},{},{},{},{},{},{},{},{:.4},{},{}",
                    iso_from_ms(b.start_ms),
                    iso_from_ms(b.end_ms),
                    b.is_active,
                    b.turns,
                    b.tokens_input,
                    b.tokens_output,
                    b.tokens_cache_create,
                    b.tokens_cache_read,
                    b.cost_usd,
                    b.burn_rate_tpm
                        .map(|x| format!("{:.1}", x))
                        .unwrap_or_default(),
                    b.account_id.as_deref().unwrap_or(""),
                );
            }
        }
        OutputFormat::Table => {
            use comfy_table::{presets::UTF8_FULL, Cell, Color, Table};
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(vec![
                "start",
                "end",
                "state",
                "turns",
                "in+out",
                "cache",
                "burn(t/min)",
                "cost",
            ]);
            for b in &to_show {
                let state_cell = if b.is_active {
                    Cell::new("ACTIVE").fg(Color::Green)
                } else {
                    Cell::new("closed").fg(Color::DarkGrey)
                };
                let burn = b
                    .burn_rate_tpm
                    .map(|x| format!("{:.0}", x))
                    .unwrap_or_else(|| "—".into());
                table.add_row(vec![
                    Cell::new(iso_from_ms(b.start_ms)),
                    Cell::new(iso_from_ms(b.end_ms)),
                    state_cell,
                    Cell::new(b.turns.to_string()),
                    Cell::new(format!("{}", b.tokens_input + b.tokens_output)),
                    Cell::new(format!("{}", b.tokens_cache_create + b.tokens_cache_read)),
                    Cell::new(burn),
                    Cell::new(format!("${:.2}", b.cost_usd)),
                ]);
            }
            println!("{table}");
            if to_show.is_empty() {
                println!("(нет блоков — запустите `index` или проверьте фильтр)");
            }
        }
    }
    Ok(())
}

fn iso_from_ms(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Команда: обновить SQLite-индекс по JSONL логам.
///
/// При `full=true` очищаем `parsed_files` и `turns` для данного host — следующая
/// итерация будет холодным проходом. Иначе использует watermark.
/// Команда `proxy` — транспортная аналитика cc-proxy (эпик proxy-correlation).
pub fn proxy_cmd(from: Option<&str>, to: Option<&str>) -> Result<()> {
    use colored::Colorize;
    let conn = schema::open_index()?;
    let from_ms = from.and_then(parse_date_to_ms).unwrap_or(0);
    let to_ms = to.and_then(parse_date_to_ms).unwrap_or(i64::MAX);

    let rep = crate::proxy::correlation::transport_report(&conn, from_ms, to_ms)?;

    if rep.n_obs == 0 {
        println!(
            "{}",
            "Нет cc-proxy наблюдений в окне. Запусти трафик через cc-proxied.sh + `agent-usage index`."
                .yellow()
        );
        return Ok(());
    }

    println!("{}", "═══ cc-proxy транспортная аналитика ═══".bold());
    if let Some(c) = &rep.coverage {
        let warn = if c.pct() < 50.0 {
            " ⚠ статы смещены к проксированным сессиям"
        } else {
            ""
        };
        println!(
            "coverage: {}/{} турнов с джойном ({:.0}%){}",
            c.matched,
            c.total_turns,
            c.pct(),
            warn.dimmed()
        );
    }
    println!("observations: {}", rep.n_obs);

    println!("\n{}", "▸ latency / очередь".bold());
    println!(
        "  wait_ms (наша очередь):   p50={}  p95={}  max={}  Σ={:.1}s",
        rep.wait_p50,
        rep.wait_p95,
        rep.wait_max,
        rep.sum_wait_ms as f64 / 1000.0
    );
    println!(
        "  dur_ms  (round-trip):     p50={}  p95={}  Σ={:.1}s",
        rep.dur_p50,
        rep.dur_p95,
        rep.sum_dur_ms as f64 / 1000.0
    );

    println!("\n{}", "▸ конкурентность / overload".bold());
    println!("  peak inflight_at_start:   {}", rep.peak_inflight);
    println!(
        "  auth(401/403)={}  upstream(429/5xx)={}  hidden-overload(200+sse)={}  orphan-retries={}",
        rep.auth_errors, rep.upstream_errors, rep.hidden_overload, rep.orphan_errors
    );

    println!(
        "\n{}",
        "▸ re-cache (флагман: KV-кэш потерян из-за задержки)".bold()
    );
    if rep.recache_events == 0 {
        println!(
            "  {}",
            "0 событий — hold не убивает кэш в этом окне ✓".green()
        );
    } else {
        println!(
            "  events={}  оценка потерь=${:.2}  из них вина очереди=${:.2}",
            rep.recache_events, rep.recache_cost_usd, rep.recache_queue_attributable_usd
        );
        println!(
            "  {}",
            "(оценка: TTL=300s assumption, Δ=1.15x база входа; re-cache = creation после gap+wait>TTL при тёплом prev)".dimmed()
        );
    }

    Ok(())
}

pub fn index_cmd(
    config: &Config,
    full: bool,
    quiet: bool,
    host: &str,
    path: Option<&std::path::Path>,
) -> Result<()> {
    let started = std::time::Instant::now();

    let mut conn = schema::open_index()?;

    if full {
        if !quiet {
            eprintln!("[index] --full: очистка для host='{}'", host);
        }
        conn.execute("DELETE FROM turns WHERE host = ?", rusqlite::params![host])?;
        conn.execute(
            "DELETE FROM parsed_files WHERE host = ?",
            rusqlite::params![host],
        )?;
    }

    let projects_dir: &std::path::Path = match path {
        Some(p) => p,
        None => &config.claude_projects_dir,
    };

    if !quiet {
        eprintln!(
            "[index] host='{}' директория: {}",
            host,
            projects_dir.display()
        );
    }

    let stats = indexer::index_all_for_host(&mut conn, projects_dir, host)?;

    // cc-proxy транспортные наблюдения (эпик proxy-correlation). Отсутствие лога
    // (mount не примонтирован) — не ошибка, просто пропускаем.
    let proxy_log = crate::proxy::default_proxy_log();
    let proxy_stats = match crate::proxy::ingest_proxy_log(&mut conn, &proxy_log, host) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: ingest cc-proxy лога не удался ({})", e);
            crate::proxy::ProxyIngestStats::default()
        }
    };

    let elapsed = started.elapsed();

    if quiet {
        // Одна строка для cron / cc-stat.sh
        println!(
            "{} elapsed={:.2}s db={}",
            stats.summary(),
            elapsed.as_secs_f64(),
            schema::index_db_path()?.display(),
        );
    } else {
        eprintln!("[index] готово за {:.2}s", elapsed.as_secs_f64());
        eprintln!("[index] {}", stats.summary());
        eprintln!("[index] {}", proxy_stats.summary());

        // Краткая аналитика — какой объём набрался
        let total_turns: i64 = conn
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
            .unwrap_or(0);
        let total_cost: f64 = conn
            .query_row("SELECT COALESCE(SUM(cost_usd), 0) FROM turns", [], |r| {
                r.get(0)
            })
            .unwrap_or(0.0);
        let total_tokens_in: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(tokens_input), 0) FROM turns",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let total_tokens_out: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(tokens_output), 0) FROM turns",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        eprintln!(
            "[index] в индексе: turns={} input={} output={} cost=${:.2}",
            total_turns, total_tokens_in, total_tokens_out, total_cost,
        );
        eprintln!("[index] db: {}", schema::index_db_path()?.display());
    }

    Ok(())
}

/// Команда: установить skill для AI-агентов
pub fn install_skills(global: bool, force: bool, agents: Option<Vec<Agent>>) -> Result<()> {
    let target_agents = match agents {
        Some(a) if !a.is_empty() => a,
        _ => detect_agents(),
    };

    // --global имеет смысл только для Claude Code
    if global {
        let has_non_claude = target_agents.iter().any(|a| !matches!(a, Agent::Claude));
        if has_non_claude {
            eprintln!("Warning: --global is only supported for Claude Code. Other agents will be installed locally.");
        }
    }

    let mut installed = 0;
    for agent in &target_agents {
        let is_global = global && matches!(agent, Agent::Claude);
        let skill_path = agent_skill_path(agent, is_global)?;

        if skill_path.exists() && !force {
            eprintln!(
                "Skipped {} (already exists: {}). Use --force to overwrite.",
                agent_label(agent),
                skill_path.display()
            );
            continue;
        }

        if let Some(parent) = skill_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = agent_skill_content(agent);
        std::fs::write(&skill_path, content)?;

        println!(
            "Installed skill for {}: {}",
            agent_label(agent),
            skill_path.display()
        );
        installed += 1;
    }

    if installed == 0 {
        println!("No skills installed. Use --force to overwrite existing files.");
    } else if installed > 1 {
        println!("\nInstalled skills for {} agents.", installed);
    }

    Ok(())
}

// ==================== Вспомогательные функции ====================

/// Клонировать сессию для передачи в correlation engine
/// (Нужно потому что correlation::correlate_session принимает ownership)
fn clone_session_for_correlation(session: &ClaudeSession) -> ClaudeSession {
    ClaudeSession {
        session_id: session.session_id,
        project_name: session.project_name.clone(),
        project_path: session.project_path.clone(),
        start_time: session.start_time,
        end_time: session.end_time,
        git_branch: session.git_branch.clone(),
        version: session.version.clone(),
        slug: session.slug.clone(),
        turns: session
            .turns
            .iter()
            .map(|t| session::Turn {
                user_timestamp: t.user_timestamp,
                assistant_timestamp: t.assistant_timestamp,
                turn_duration_ms: t.turn_duration_ms,
                tool_calls: t.tool_calls.clone(),
                tool_call_details: t.tool_call_details.clone(),
                mcp_calls: t.mcp_calls.clone(),
                tool_results: t.tool_results.clone(),
                usage: t.usage.clone(),
                model: t.model.clone(),
                git_branch: t.git_branch.clone(),
                user_message_preview: t.user_message_preview.clone(),
                context_tokens: t.context_tokens,
            })
            .collect(),
        total_usage: session.total_usage.clone(),
        is_subagent: session.is_subagent,
        compactions: session.compactions.clone(),
    }
}

fn print_csv_projects(projects: &[(String, usize, AggregatedUsage)]) {
    println!("project,sessions,input_tokens,output_tokens,cost_usd");
    for (name, sessions, usage) in projects {
        println!(
            "{},{},{},{},{:.4}",
            name, sessions, usage.input_tokens, usage.output_tokens, usage.estimated_cost_usd
        );
    }
}

fn print_csv_sessions(sessions: &[&ClaudeSession]) {
    println!(
        "session_id,project,start_time,duration_secs,turns,input_tokens,output_tokens,cost_usd"
    );
    for s in sessions {
        println!(
            "{},{},{},{},{},{},{},{:.4}",
            &s.session_id.to_string()[..8],
            s.project_name,
            s.start_time.to_rfc3339(),
            s.duration().num_seconds(),
            s.turns.len(),
            s.total_usage.input_tokens,
            s.total_usage.output_tokens,
            s.total_usage.estimated_cost_usd,
        );
    }
}

/// Команда: анализ поведенческих паттернов MCP pipeline инструментов
pub fn mcp_patterns(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    verbose: bool,
    format: &OutputFormat,
) -> Result<()> {
    let sessions = load_sessions(config)?;
    let filtered = filter_sessions(&sessions, project, from, to);

    let report = mcp_patterns::build_behavior_report(&filtered);

    println!(
        "Проанализировано {} сессий, {} с pipeline вызовами, {} инвокаций\n",
        report.total_sessions_analyzed,
        report.sessions_with_pipeline_calls,
        report.total_invocations,
    );

    if report.total_invocations == 0 {
        println!(
            "Pipeline инструменты (get_issues, get_merge_requests и т.д.) не найдены в логах."
        );
        println!("Убедитесь, что devboy MCP сервер используется в сессиях.");
        return Ok(());
    }

    match format {
        OutputFormat::Table => print_mcp_patterns_table(&report.tool_stats),
        OutputFormat::Json => print_mcp_patterns_json(&report),
        OutputFormat::Csv => print_mcp_patterns_csv(&report.tool_stats),
    }

    if verbose {
        println!("\n--- Детали инвокаций ---");
        let invocations = mcp_patterns::extract_pipeline_invocations(&filtered);
        for inv in &invocations {
            println!(
                "[{}] {} | {} чанков | p₁={} | вызовов={}",
                &inv.session_id.to_string()[..8],
                inv.tool_name,
                inv.total_chunks(),
                if inv.needed_pagination() { "0" } else { "1" },
                inv.calls.len(),
            );
            for call in &inv.calls {
                let chunk_str = call
                    .chunk
                    .map_or("base".to_string(), |c| format!("chunk={}", c));
                let key_str = call.item_key.as_deref().unwrap_or("");
                println!("    {} {}", chunk_str, key_str);
            }
        }
    }

    Ok(())
}

fn print_mcp_patterns_table(stats: &[mcp_patterns::ToolBehaviorStats]) {
    use comfy_table::{presets, Cell, Color, Table};
    let mut table = Table::new();
    table.load_preset(presets::UTF8_BORDERS_ONLY);
    table.set_header(vec![
        "Инструмент",
        "Инвокаций",
        "p₁ (first-chunk)",
        "E[chunks]",
        "max chunk",
        "Сессий",
        "Проектов",
    ]);

    for s in stats {
        let p1_color = if s.p1 >= 0.7 {
            Color::Green
        } else if s.p1 >= 0.5 {
            Color::Yellow
        } else {
            Color::Red
        };
        table.add_row(vec![
            Cell::new(&s.tool_name),
            Cell::new(s.total_invocations),
            Cell::new(format!("{:.1}%", s.p1_percent())).fg(p1_color),
            Cell::new(format!("{:.2}", s.e_chunks)),
            Cell::new(s.max_chunk_seen),
            Cell::new(s.sessions_using),
            Cell::new(s.projects_using),
        ]);
    }
    println!("{table}");
    println!("\np₁ — вероятность что первого чанка достаточно для ответа агента");
    println!("E[chunks] — среднее кол-во запрошенных чанков на инвокацию");
}

fn print_mcp_patterns_json(report: &mcp_patterns::BehaviorReport) {
    use serde_json::json;
    let obj = json!({
        "total_sessions_analyzed": report.total_sessions_analyzed,
        "sessions_with_pipeline_calls": report.sessions_with_pipeline_calls,
        "total_invocations": report.total_invocations,
        "tools": report.tool_stats.iter().map(|s| json!({
            "tool_name": s.tool_name,
            "total_invocations": s.total_invocations,
            "first_chunk_sufficient": s.first_chunk_sufficient,
            "p1": s.p1,
            "e_chunks": s.e_chunks,
            "max_chunk_seen": s.max_chunk_seen,
            "sessions_using": s.sessions_using,
            "projects_using": s.projects_using,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&obj).unwrap());
}

fn print_mcp_patterns_csv(stats: &[mcp_patterns::ToolBehaviorStats]) {
    println!("tool_name,total_invocations,first_chunk_sufficient,p1,e_chunks,max_chunk_seen,sessions_using,projects_using");
    for s in stats {
        println!(
            "{},{},{},{:.4},{:.4},{},{},{}",
            s.tool_name,
            s.total_invocations,
            s.first_chunk_sufficient,
            s.p1,
            s.e_chunks,
            s.max_chunk_seen,
            s.sessions_using,
            s.projects_using,
        );
    }
}

// ==================== context-enrichment ====================

/// Enrichment инструменты специфичные для каждого pipeline tool
fn enrichment_tools_for(primary_tool: &str) -> &'static [&'static str] {
    match primary_tool {
        "get_issues" | "search_issues" => &[
            "get_issue",
            "get_issue_comments",
            "get_issue_relations",
            "get_epics",
        ],
        "get_merge_requests" | "search_merge_requests" => {
            &["get_merge_request_discussions", "get_merge_request_diffs"]
        }
        "get_merge_request_diffs" => &["get_merge_request_discussions", "get_issue_comments"],
        "get_merge_request_discussions" => &["get_merge_request_diffs", "get_issue_comments"],
        "get_meeting_notes" | "search_meeting_notes" => &[
            "get_meeting_transcript",
            "search_meeting_notes",
            "get_chat_messages",
        ],
        _ => &[
            "get_issue",
            "get_issue_comments",
            "get_issue_relations",
            "get_epics",
            "get_merge_request_discussions",
            "get_merge_request_diffs",
            "get_meeting_transcript",
            "search_meeting_notes",
        ],
    }
}

/// Команда: анализ гипотезы обогащения контекста
pub fn context_enrichment(
    config: &Config,
    tool_filter: &str,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    let sessions = load_sessions(config)?;
    let filtered = filter_sessions(&sessions, project, from, to);

    let mut points: Vec<EnrichmentPoint> = Vec::new();

    for session in &filtered {
        for turn in &session.turns {
            for result in &turn.tool_results {
                if result.is_error || result.tool_name != tool_filter {
                    continue;
                }
                let Some(items_shown) = result.items_shown else {
                    continue;
                };
                if items_shown == 0 {
                    continue;
                }
                let chars_per_item = result.content_chars as f64 / items_shown as f64;

                // Считаем enrichment вызовы в том же turn'е
                let followup_names: Vec<String> = result
                    .same_turn_followups
                    .iter()
                    .map(|(name, _)| {
                        if name.starts_with("mcp__") {
                            mcp_short_tool_name(name).to_string()
                        } else {
                            name.clone()
                        }
                    })
                    .collect();

                let enrichment_tools = enrichment_tools_for(tool_filter);
                let enrichment_count = followup_names
                    .iter()
                    .filter(|n| enrichment_tools.contains(&n.as_str()))
                    .count();

                points.push(EnrichmentPoint {
                    chars_per_item,
                    content_chars: result.content_chars,
                    items_shown,
                    enrichment_count,
                    total_followups: followup_names.len(),
                    followup_names,
                });
            }
        }
    }

    if points.is_empty() {
        println!(
            "Нет данных для инструмента '{}' с известным количеством айтемов.",
            tool_filter
        );
        println!(
            "Убедитесь, что ответы содержат TOON-заголовки (#number title) или [chunks] маркер."
        );
        return Ok(());
    }

    println!(
        "Инструмент: {}  |  записей с known item count: {}\n",
        tool_filter,
        points.len()
    );

    // Группируем по бакетам chars_per_item
    // Бакеты: tiny (<200), small (200-500), medium (500-1500), large (1500-4000), huge (>4000)
    let buckets: &[(&str, f64, f64)] = &[
        ("tiny  <200", 0.0, 200.0),
        ("small 200-500", 200.0, 500.0),
        ("med   500-1.5k", 500.0, 1500.0),
        ("large 1.5k-4k", 1500.0, 4000.0),
        ("huge  >4k", 4000.0, f64::MAX),
    ];

    let enrichment_tools = enrichment_tools_for(tool_filter);
    match format {
        OutputFormat::Table => {
            print_enrichment_table(&points, buckets, tool_filter, enrichment_tools);
            print_enrichment_correlation(&points, enrichment_tools);
        }
        OutputFormat::Json => print_enrichment_json(&points, buckets),
        OutputFormat::Csv => print_enrichment_csv(&points),
    }

    Ok(())
}

fn print_enrichment_table(
    points: &[EnrichmentPoint],
    buckets: &[(&str, f64, f64)],
    tool_filter: &str,
    _enrichment_tools: &[&str],
) {
    use comfy_table::{presets, Cell, Color, Table};

    let mut table = Table::new();
    table.load_preset(presets::UTF8_BORDERS_ONLY);
    table.set_header(vec![
        "chars/item bucket",
        "N",
        "mean ch/item",
        "E[enrichment]",
        "E[followups]",
        "% has enrichment",
    ]);

    for &(label, lo, hi) in buckets {
        let bp: Vec<_> = points
            .iter()
            .filter(|p| p.chars_per_item >= lo && p.chars_per_item < hi)
            .collect();
        if bp.is_empty() {
            continue;
        }
        let n = bp.len();
        let mean_cpi = bp.iter().map(|p| p.chars_per_item).sum::<f64>() / n as f64;
        let mean_enr = bp.iter().map(|p| p.enrichment_count as f64).sum::<f64>() / n as f64;
        let mean_fup = bp.iter().map(|p| p.total_followups as f64).sum::<f64>() / n as f64;
        let pct_enr =
            bp.iter().filter(|p| p.enrichment_count > 0).count() as f64 / n as f64 * 100.0;

        // Цвет: чем больше enrichment при малом контексте — тем желтее
        let enr_color = if lo < 500.0 && mean_enr > 1.5 {
            Color::Yellow
        } else if lo >= 1500.0 && mean_enr < 0.5 {
            Color::Green
        } else {
            Color::Reset
        };

        table.add_row(vec![
            Cell::new(label),
            Cell::new(n),
            Cell::new(format!("{:.0}", mean_cpi)),
            Cell::new(format!("{:.2}", mean_enr)).fg(enr_color),
            Cell::new(format!("{:.2}", mean_fup)),
            Cell::new(format!("{:.0}%", pct_enr)),
        ]);
    }
    println!("{table}");
    println!(
        "\nE[enrichment] — среднее число enrichment tool calls ({}) в том же turn'е",
        tool_filter
    );
    println!("Гипотеза: чем меньше chars/item → тем больше E[enrichment]");
    println!();
}

fn print_enrichment_correlation(points: &[EnrichmentPoint], enrichment_tools: &[&str]) {
    // Pearson correlation между chars_per_item и enrichment_count
    let n = points.len() as f64;
    if n < 3.0 {
        return;
    }
    let mean_x = points.iter().map(|p| p.chars_per_item).sum::<f64>() / n;
    let mean_y = points
        .iter()
        .map(|p| p.enrichment_count as f64)
        .sum::<f64>()
        / n;

    let cov: f64 = points
        .iter()
        .map(|p| (p.chars_per_item - mean_x) * (p.enrichment_count as f64 - mean_y))
        .sum::<f64>()
        / n;
    let std_x = (points
        .iter()
        .map(|p| (p.chars_per_item - mean_x).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();
    let std_y = (points
        .iter()
        .map(|p| (p.enrichment_count as f64 - mean_y).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();

    if std_x > 0.0 && std_y > 0.0 {
        let r = cov / (std_x * std_y);
        let interpretation = if r < -0.3 {
            "✓ отрицательная корреляция — гипотеза подтверждается"
        } else if r > 0.3 {
            "✗ положительная корреляция — гипотеза опровергается"
        } else {
            "~ корреляция слабая"
        };
        println!(
            "Pearson r(chars_per_item, enrichment_count) = {:.3}  {}",
            r, interpretation
        );
    }

    // Топ enrichment инструментов по всем точкам
    let mut tool_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for p in points {
        for name in &p.followup_names {
            if enrichment_tools.contains(&name.as_str()) {
                *tool_counts.entry(name.clone()).or_default() += 1;
            }
        }
    }
    let mut sorted: Vec<_> = tool_counts.into_iter().collect();
    sorted.sort_by_key(|(_, c)| std::cmp::Reverse(*c));

    println!("\nТоп enrichment инструментов:");
    for (tool, count) in sorted.iter().take(8) {
        let pct = count * 100 / points.len().max(1);
        println!("  {:40} {:4} ({:2}%)", tool, count, pct);
    }
}

fn print_enrichment_json(points: &[EnrichmentPoint], buckets: &[(&str, f64, f64)]) {
    use serde_json::json;
    let bucket_data: Vec<_> = buckets
        .iter()
        .map(|&(label, lo, hi)| {
            let bp: Vec<_> = points
                .iter()
                .filter(|p| p.chars_per_item >= lo && p.chars_per_item < hi)
                .collect();
            let n = bp.len();
            let mean_enr = if n > 0 {
                bp.iter().map(|p| p.enrichment_count as f64).sum::<f64>() / n as f64
            } else {
                0.0
            };
            json!({ "bucket": label, "count": n, "mean_enrichment": mean_enr })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&bucket_data).unwrap());
}

fn print_enrichment_csv(points: &[EnrichmentPoint]) {
    println!("chars_per_item,items_shown,content_chars,enrichment_count,total_followups");
    for p in points {
        println!(
            "{:.1},{},{},{},{}",
            p.chars_per_item, p.items_shown, p.content_chars, p.enrichment_count, p.total_followups
        );
    }
}

// Вспомогательная структура для обхода ограничений замыканий
struct EnrichmentPoint {
    chars_per_item: f64,
    content_chars: usize,
    items_shown: usize,
    enrichment_count: usize,
    total_followups: usize,
    followup_names: Vec<String>,
}

// ==================== tool-behavior ====================

/// Команда: анализ поведения агента после больших MCP ответов
pub fn tool_behavior(
    config: &Config,
    tool_filter: Option<&str>,
    large_threshold: usize,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    use std::collections::HashMap;

    let sessions = load_sessions(config)?;
    let filtered = filter_sessions(&sessions, project, from, to);

    // Собираем данные по каждому инструменту
    // ключ: tool_name, значение: (большие, малые) — агрегированная статистика
    let mut large_followup_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut small_followup_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut large_count: HashMap<String, usize> = HashMap::new();
    let mut small_count: HashMap<String, usize> = HashMap::new();

    // Статистика "следующего turn'а" — что делал агент после большого ответа
    let mut large_next_turn_tools: HashMap<String, HashMap<String, usize>> = HashMap::new();

    for session in &filtered {
        let turns = &session.turns;
        for (turn_idx, turn) in turns.iter().enumerate() {
            let next_turn = turns.get(turn_idx + 1);

            for result in &turn.tool_results {
                if result.is_error {
                    continue;
                }
                // Применяем фильтр по инструменту
                if let Some(f) = tool_filter {
                    if !result.tool_name.contains(f) {
                        continue;
                    }
                }

                let is_large = result.content_chars >= large_threshold;

                if is_large {
                    *large_count.entry(result.tool_name.clone()).or_default() += 1;

                    // Follow-ups в том же turn'е
                    for (name, _) in &result.same_turn_followups {
                        let short = if name.starts_with("mcp__") {
                            mcp_short_tool_name(name)
                        } else {
                            name.as_str()
                        };
                        *large_followup_counts
                            .entry(result.tool_name.clone())
                            .or_default()
                            .entry(short.to_string())
                            .or_default() += 1;
                    }
                    if result.same_turn_followups.is_empty() {
                        *large_followup_counts
                            .entry(result.tool_name.clone())
                            .or_default()
                            .entry("[no followup]".to_string())
                            .or_default() += 1;
                    }

                    // Следующий turn
                    if let Some(next) = next_turn {
                        let next_tools: std::collections::HashSet<String> = next
                            .tool_call_details
                            .iter()
                            .map(|(name, _)| {
                                if name.starts_with("mcp__") {
                                    mcp_short_tool_name(name).to_string()
                                } else {
                                    name.clone()
                                }
                            })
                            .collect();
                        for tool in next_tools {
                            *large_next_turn_tools
                                .entry(result.tool_name.clone())
                                .or_default()
                                .entry(tool)
                                .or_default() += 1;
                        }
                        if next.tool_call_details.is_empty() {
                            *large_next_turn_tools
                                .entry(result.tool_name.clone())
                                .or_default()
                                .entry("[text response]".to_string())
                                .or_default() += 1;
                        }
                    }
                } else {
                    *small_count.entry(result.tool_name.clone()).or_default() += 1;

                    for (name, _) in &result.same_turn_followups {
                        let short = if name.starts_with("mcp__") {
                            mcp_short_tool_name(name)
                        } else {
                            name.as_str()
                        };
                        *small_followup_counts
                            .entry(result.tool_name.clone())
                            .or_default()
                            .entry(short.to_string())
                            .or_default() += 1;
                    }
                    if result.same_turn_followups.is_empty() {
                        *small_followup_counts
                            .entry(result.tool_name.clone())
                            .or_default()
                            .entry("[no followup]".to_string())
                            .or_default() += 1;
                    }
                }
            }
        }
    }

    // Собираем все инструменты для которых есть данные
    let mut all_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
    all_tools.extend(large_count.keys().cloned());
    all_tools.extend(small_count.keys().cloned());
    let mut all_tools: Vec<String> = all_tools.into_iter().collect();
    all_tools.sort_by_key(|t| {
        let l = large_count.get(t).copied().unwrap_or(0);
        let s = small_count.get(t).copied().unwrap_or(0);
        std::cmp::Reverse(l + s)
    });

    println!(
        "Порог 'большого' ответа: {} символов (≈{} tokens)\n",
        large_threshold,
        large_threshold / 35 * 10
    );

    match format {
        OutputFormat::Table => print_tool_behavior_table(
            &all_tools,
            &large_count,
            &small_count,
            &large_followup_counts,
            &small_followup_counts,
            &large_next_turn_tools,
            large_threshold,
        ),
        OutputFormat::Json => print_tool_behavior_json(
            &all_tools,
            &large_count,
            &small_count,
            &large_followup_counts,
            &small_followup_counts,
            &large_next_turn_tools,
        ),
        OutputFormat::Csv => print_tool_behavior_csv(
            &all_tools,
            &large_count,
            &small_count,
            &large_followup_counts,
        ),
    }

    Ok(())
}

fn mcp_short_tool_name(full_name: &str) -> &str {
    full_name.rsplit("__").next().unwrap_or(full_name)
}

#[allow(clippy::too_many_arguments)]
fn print_tool_behavior_table(
    tools: &[String],
    large_count: &HashMap<String, usize>,
    small_count: &HashMap<String, usize>,
    large_followups: &HashMap<String, HashMap<String, usize>>,
    small_followups: &HashMap<String, HashMap<String, usize>>,
    large_next_turn: &HashMap<String, HashMap<String, usize>>,
    threshold: usize,
) {
    use comfy_table::{presets, Cell, Color, Table};

    for tool_name in tools {
        let lc = large_count.get(tool_name).copied().unwrap_or(0);
        let sc = small_count.get(tool_name).copied().unwrap_or(0);
        let total = lc + sc;
        if total == 0 {
            continue;
        }

        println!(
            "━━━ {} ━━━  total: {}  large (>{} ch): {}  small: {}",
            tool_name, total, threshold, lc, sc,
        );

        // Таблица follow-ups в том же turn'е
        if lc > 0 || sc > 0 {
            let mut table = Table::new();
            table.load_preset(presets::UTF8_BORDERS_ONLY);
            table.set_header(vec![
                "Follow-up (same turn)",
                &format!("Large (n={})", lc),
                "Large%",
                &format!("Small (n={})", sc),
                "Small%",
            ]);

            // Все уникальные follow-up tools
            let mut all_followup: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            if let Some(m) = large_followups.get(tool_name) {
                all_followup.extend(m.keys().cloned());
            }
            if let Some(m) = small_followups.get(tool_name) {
                all_followup.extend(m.keys().cloned());
            }
            let mut all_followup: Vec<String> = all_followup.into_iter().collect();
            all_followup.sort_by_key(|k| {
                let l = large_followups
                    .get(tool_name)
                    .and_then(|m| m.get(k))
                    .copied()
                    .unwrap_or(0);
                let s = small_followups
                    .get(tool_name)
                    .and_then(|m| m.get(k))
                    .copied()
                    .unwrap_or(0);
                std::cmp::Reverse(l + s)
            });

            for followup in all_followup.iter().take(10) {
                let l_cnt = large_followups
                    .get(tool_name)
                    .and_then(|m| m.get(followup))
                    .copied()
                    .unwrap_or(0);
                let s_cnt = small_followups
                    .get(tool_name)
                    .and_then(|m| m.get(followup))
                    .copied()
                    .unwrap_or(0);
                let l_pct = (l_cnt * 100).checked_div(lc).unwrap_or(0);
                let s_pct = (s_cnt * 100).checked_div(sc).unwrap_or(0);

                let diff_color = if l_pct > s_pct + 10 {
                    Color::Yellow // чаще при большом ответе
                } else if s_pct > l_pct + 10 {
                    Color::Cyan // чаще при маленьком ответе
                } else {
                    Color::Reset
                };

                table.add_row(vec![
                    Cell::new(followup),
                    Cell::new(l_cnt),
                    Cell::new(format!("{}%", l_pct)).fg(diff_color),
                    Cell::new(s_cnt),
                    Cell::new(format!("{}%", s_pct)),
                ]);
            }
            println!("{table}");
        }

        // Следующий turn после большого ответа
        if let Some(next_map) = large_next_turn.get(tool_name) {
            if !next_map.is_empty() {
                println!("  Следующий turn после большого ответа (top-5):");
                let mut next_sorted: Vec<(&String, &usize)> = next_map.iter().collect();
                next_sorted.sort_by_key(|(_, &v)| std::cmp::Reverse(v));
                for (tool, count) in next_sorted.iter().take(5) {
                    let pct = *count * 100 / lc.max(1);
                    println!("    {:40} {:3} ({:2}%)", tool, count, pct);
                }
            }
        }
        println!();
    }
}

fn print_tool_behavior_json(
    tools: &[String],
    large_count: &HashMap<String, usize>,
    small_count: &HashMap<String, usize>,
    large_followups: &HashMap<String, HashMap<String, usize>>,
    small_followups: &HashMap<String, HashMap<String, usize>>,
    large_next_turn: &HashMap<String, HashMap<String, usize>>,
) {
    use serde_json::json;
    let arr: Vec<_> = tools
        .iter()
        .map(|t| {
            json!({
                "tool": t,
                "large_count": large_count.get(t).copied().unwrap_or(0),
                "small_count": small_count.get(t).copied().unwrap_or(0),
                "large_same_turn_followups": large_followups.get(t),
                "small_same_turn_followups": small_followups.get(t),
                "large_next_turn_tools": large_next_turn.get(t),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap());
}

fn print_tool_behavior_csv(
    tools: &[String],
    large_count: &HashMap<String, usize>,
    small_count: &HashMap<String, usize>,
    large_followups: &HashMap<String, HashMap<String, usize>>,
) {
    println!("tool_name,size_bucket,followup_tool,count,pct");
    for tool in tools {
        let lc = large_count.get(tool).copied().unwrap_or(0);
        let sc = small_count.get(tool).copied().unwrap_or(0);
        if let Some(map) = large_followups.get(tool) {
            for (followup, cnt) in map {
                let pct = (cnt * 100).checked_div(lc).unwrap_or(0);
                println!("{},large,{},{},{}", tool, followup, cnt, pct);
            }
        }
        if sc > 0 {
            println!("{},small,[data],{},100", tool, sc);
        }
    }
}

// ==================== tool-response-stats ====================

/// Команда: статистика размеров ответов MCP pipeline инструментов
pub fn tool_response_stats(
    config: &Config,
    project: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    format: &OutputFormat,
) -> Result<()> {
    use std::collections::HashMap;

    let sessions = load_sessions(config)?;
    let filtered = filter_sessions(&sessions, project, from, to);

    // Собираем все tool_results из всех turn'ов
    // tool_name → список размеров в символах
    let mut by_tool: HashMap<String, Vec<usize>> = HashMap::new();
    let mut total_results = 0usize;
    let mut error_count = 0usize;

    for session in &filtered {
        for turn in &session.turns {
            for result in &turn.tool_results {
                if result.is_error {
                    error_count += 1;
                    continue;
                }
                by_tool
                    .entry(result.tool_name.clone())
                    .or_default()
                    .push(result.content_chars);
                total_results += 1;
            }
        }
    }

    println!(
        "Проанализировано {} сессий, {} ответов MCP инструментов ({} ошибок)\n",
        filtered.len(),
        total_results,
        error_count,
    );

    if total_results == 0 {
        println!("MCP ответы не найдены в логах.");
        println!("Убедитесь, что devboy MCP сервер использовался в сессиях.");
        return Ok(());
    }

    // Вычисляем статистику по каждому инструменту
    let mut stats: Vec<ToolResponseToolStats> = by_tool
        .into_iter()
        .map(|(tool_name, mut sizes)| {
            sizes.sort_unstable();
            let count = sizes.len();
            let total: usize = sizes.iter().sum();
            let mean = total as f64 / count as f64;
            let median = sizes[count / 2];
            let p75 = sizes[count * 75 / 100];
            let p90 = sizes[count * 90 / 100];
            let p99 = sizes[count * 99 / 100];
            let max = *sizes.last().unwrap_or(&0);

            // Бюджеты в символах (chars ≈ tokens × 3.5 для TOON)
            // 8000 tokens ≈ 28000 chars; 4000 tokens ≈ 14000 chars
            let exceeds_28k = sizes.iter().filter(|&&s| s > 28_000).count();
            let exceeds_14k = sizes.iter().filter(|&&s| s > 14_000).count();
            let exceeds_7k = sizes.iter().filter(|&&s| s > 7_000).count();

            ToolResponseToolStats {
                tool_name,
                count,
                mean_chars: mean as usize,
                median_chars: median,
                p75_chars: p75,
                p90_chars: p90,
                p99_chars: p99,
                max_chars: max,
                pct_exceeds_28k: exceeds_28k as f64 / count as f64 * 100.0,
                pct_exceeds_14k: exceeds_14k as f64 / count as f64 * 100.0,
                pct_exceeds_7k: exceeds_7k as f64 / count as f64 * 100.0,
            }
        })
        .collect();

    stats.sort_by_key(|b| std::cmp::Reverse(b.count));

    match format {
        OutputFormat::Table => print_tool_response_stats_table(&stats),
        OutputFormat::Json => print_tool_response_stats_json(&stats),
        OutputFormat::Csv => print_tool_response_stats_csv(&stats),
    }

    Ok(())
}

struct ToolResponseToolStats {
    tool_name: String,
    count: usize,
    mean_chars: usize,
    median_chars: usize,
    p75_chars: usize,
    p90_chars: usize,
    p99_chars: usize,
    max_chars: usize,
    /// % ответов больше 28k символов (≈8k tokens)
    pct_exceeds_28k: f64,
    /// % ответов больше 14k символов (≈4k tokens)
    pct_exceeds_14k: f64,
    /// % ответов больше 7k символов (≈2k tokens)
    pct_exceeds_7k: f64,
}

fn print_tool_response_stats_table(stats: &[ToolResponseToolStats]) {
    use comfy_table::{presets, Cell, Color, Table};

    // Таблица 1: размеры
    let mut table = Table::new();
    table.load_preset(presets::UTF8_BORDERS_ONLY);
    table.set_header(vec![
        "Инструмент",
        "Вызовов",
        "Median",
        "P75",
        "P90",
        "P99",
        "Max",
        "~tokens(P90)",
    ]);
    for s in stats {
        table.add_row(vec![
            Cell::new(&s.tool_name),
            Cell::new(s.count),
            Cell::new(format_chars(s.median_chars)),
            Cell::new(format_chars(s.p75_chars)),
            Cell::new(format_chars(s.p90_chars)),
            Cell::new(format_chars(s.p99_chars)),
            Cell::new(format_chars(s.max_chars)),
            Cell::new(s.p90_chars / 35 * 10), // chars / 3.5
        ]);
    }
    println!("{table}");

    // Таблица 2: % превышающих бюджеты
    println!();
    let mut table2 = Table::new();
    table2.load_preset(presets::UTF8_BORDERS_ONLY);
    table2.set_header(vec![
        "Инструмент",
        "Вызовов",
        ">2k tok (>7k ch)",
        ">4k tok (>14k ch)",
        ">8k tok (>28k ch)",
    ]);
    for s in stats {
        let color_28k = if s.pct_exceeds_28k > 50.0 {
            Color::Red
        } else if s.pct_exceeds_28k > 20.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        table2.add_row(vec![
            Cell::new(&s.tool_name),
            Cell::new(s.count),
            Cell::new(format!("{:.0}%", s.pct_exceeds_7k)),
            Cell::new(format!("{:.0}%", s.pct_exceeds_14k)),
            Cell::new(format!("{:.0}%", s.pct_exceeds_28k)).fg(color_28k),
        ]);
    }
    println!("{table2}");
    println!("\n>N tok — доля ответов превышающих бюджет N тысяч токенов (оценка: chars / 3.5)");
}

fn format_chars(chars: usize) -> String {
    if chars >= 1_000_000 {
        format!("{:.1}M", chars as f64 / 1_000_000.0)
    } else if chars >= 1_000 {
        format!("{:.1}k", chars as f64 / 1_000.0)
    } else {
        format!("{}", chars)
    }
}

fn print_tool_response_stats_json(stats: &[ToolResponseToolStats]) {
    use serde_json::json;
    let arr: Vec<_> = stats
        .iter()
        .map(|s| {
            json!({
                "tool_name": s.tool_name,
                "count": s.count,
                "median_chars": s.median_chars,
                "p75_chars": s.p75_chars,
                "p90_chars": s.p90_chars,
                "p99_chars": s.p99_chars,
                "max_chars": s.max_chars,
                "approx_p90_tokens": s.p90_chars / 35 * 10,
                "pct_exceeds_7k_chars": s.pct_exceeds_7k,
                "pct_exceeds_14k_chars": s.pct_exceeds_14k,
                "pct_exceeds_28k_chars": s.pct_exceeds_28k,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap());
}

fn print_tool_response_stats_csv(stats: &[ToolResponseToolStats]) {
    println!("tool_name,count,mean_chars,median_chars,p75_chars,p90_chars,p99_chars,max_chars,approx_p90_tokens,pct_exceeds_28k");
    for s in stats {
        println!(
            "{},{},{},{},{},{},{},{},{},{:.1}",
            s.tool_name,
            s.count,
            s.mean_chars,
            s.median_chars,
            s.p75_chars,
            s.p90_chars,
            s.p99_chars,
            s.max_chars,
            s.p90_chars / 35 * 10,
            s.pct_exceeds_28k,
        );
    }
}

#[cfg(test)]
mod calib_tests {
    use super::*;

    #[test]
    fn parse_plain_number() {
        assert_eq!(parse_token_count("12345").unwrap(), 12345);
        assert_eq!(parse_token_count("0").unwrap(), 0);
    }

    #[test]
    fn parse_k_suffix() {
        assert_eq!(parse_token_count("100k").unwrap(), 100_000);
        assert_eq!(parse_token_count("100K").unwrap(), 100_000);
        assert_eq!(parse_token_count("1.5k").unwrap(), 1500);
    }

    #[test]
    fn parse_m_suffix() {
        assert_eq!(parse_token_count("44M").unwrap(), 44_000_000);
        assert_eq!(parse_token_count("220M").unwrap(), 220_000_000);
        assert_eq!(parse_token_count("1.5M").unwrap(), 1_500_000);
    }

    #[test]
    fn parse_g_suffix() {
        assert_eq!(parse_token_count("1g").unwrap(), 1_000_000_000);
        assert_eq!(parse_token_count("2.5G").unwrap(), 2_500_000_000);
    }

    #[test]
    fn parse_invalid_returns_err() {
        assert!(parse_token_count("").is_err());
        assert!(parse_token_count("not a number").is_err());
        assert!(parse_token_count("12X").is_err()); // unknown suffix → fails number parse
    }

    #[test]
    fn format_uses_appropriate_suffix() {
        assert_eq!(format_token_count(500), "500");
        assert_eq!(format_token_count(1500), "2K (1500)");
        assert_eq!(format_token_count(220_000_000), "220M (220000000)");
    }
}
