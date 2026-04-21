use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use indicatif::{ProgressBar, ProgressStyle};

use crate::activity::db;
use crate::activity::transform;
use crate::claude::mcp_patterns;
use crate::claude::parser;
use crate::claude::session::{self, AggregatedUsage, ClaudeSession};
use crate::cli::{Agent, GroupBy, OutputFormat, TaskSortBy};
use crate::config::Config;
use crate::correlation::engine;
use crate::correlation::tasks;
use crate::output::{json, table, timeline};

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
    filtered.sort_by(|a, b| b.start_time.cmp(&a.start_time));
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
            task_stats.sort_by(|a, b| b.session_count.cmp(&a.session_count));
        }
        TaskSortBy::Recent => {
            task_stats.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
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
        println!("Pipeline инструменты (get_issues, get_merge_requests и т.д.) не найдены в логах.");
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
                let chunk_str = call.chunk.map_or("base".to_string(), |c| format!("chunk={}", c));
                let key_str = call.item_key.as_deref().unwrap_or("");
                println!("    {} {}", chunk_str, key_str);
            }
        }
    }

    Ok(())
}

fn print_mcp_patterns_table(stats: &[mcp_patterns::ToolBehaviorStats]) {
    use comfy_table::{Cell, Color, Table, presets};
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
        "get_merge_requests" | "search_merge_requests" => &[
            "get_merge_request_discussions",
            "get_merge_request_diffs",
        ],
        "get_merge_request_diffs" => &[
            "get_merge_request_discussions",
            "get_issue_comments",
        ],
        "get_merge_request_discussions" => &[
            "get_merge_request_diffs",
            "get_issue_comments",
        ],
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
        println!("Нет данных для инструмента '{}' с известным количеством айтемов.", tool_filter);
        println!("Убедитесь, что ответы содержат TOON-заголовки (#number title) или [chunks] маркер.");
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
        ("tiny  <200",    0.0,    200.0),
        ("small 200-500", 200.0,  500.0),
        ("med   500-1.5k",500.0,  1500.0),
        ("large 1.5k-4k", 1500.0, 4000.0),
        ("huge  >4k",     4000.0, f64::MAX),
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
    use comfy_table::{Cell, Color, Table, presets};

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
        let pct_enr = bp.iter().filter(|p| p.enrichment_count > 0).count() as f64 / n as f64 * 100.0;

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
    println!("\nE[enrichment] — среднее число enrichment tool calls ({}) в том же turn'е", tool_filter);
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
    let mean_y = points.iter().map(|p| p.enrichment_count as f64).sum::<f64>() / n;

    let cov: f64 = points
        .iter()
        .map(|p| (p.chars_per_item - mean_x) * (p.enrichment_count as f64 - mean_y))
        .sum::<f64>() / n;
    let std_x = (points.iter().map(|p| (p.chars_per_item - mean_x).powi(2)).sum::<f64>() / n).sqrt();
    let std_y = (points.iter().map(|p| (p.enrichment_count as f64 - mean_y).powi(2)).sum::<f64>() / n).sqrt();

    if std_x > 0.0 && std_y > 0.0 {
        let r = cov / (std_x * std_y);
        let interpretation = if r < -0.3 {
            "✓ отрицательная корреляция — гипотеза подтверждается"
        } else if r > 0.3 {
            "✗ положительная корреляция — гипотеза опровергается"
        } else {
            "~ корреляция слабая"
        };
        println!("Pearson r(chars_per_item, enrichment_count) = {:.3}  {}", r, interpretation);
    }

    // Топ enrichment инструментов по всем точкам
    let mut tool_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
    #[allow(unused_variables)]
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
            } else { 0.0 };
            json!({ "bucket": label, "count": n, "mean_enrichment": mean_enr })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&bucket_data).unwrap());
}

fn print_enrichment_csv(points: &[EnrichmentPoint]) {
    println!("chars_per_item,items_shown,content_chars,enrichment_count,total_followups");
    for p in points {
        println!("{:.1},{},{},{},{}", p.chars_per_item, p.items_shown, p.content_chars, p.enrichment_count, p.total_followups);
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
    use comfy_table::{Cell, Color, Table, presets};

    for tool_name in tools {
        let lc = large_count.get(tool_name).copied().unwrap_or(0);
        let sc = small_count.get(tool_name).copied().unwrap_or(0);
        let total = lc + sc;
        if total == 0 {
            continue;
        }

        println!(
            "━━━ {} ━━━  total: {}  large (>{} ch): {}  small: {}",
            tool_name,
            total,
            threshold,
            lc,
            sc,
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
            let mut all_followup: std::collections::HashSet<String> = std::collections::HashSet::new();
            if let Some(m) = large_followups.get(tool_name) {
                all_followup.extend(m.keys().cloned());
            }
            if let Some(m) = small_followups.get(tool_name) {
                all_followup.extend(m.keys().cloned());
            }
            let mut all_followup: Vec<String> = all_followup.into_iter().collect();
            all_followup.sort_by_key(|k| {
                let l = large_followups.get(tool_name).and_then(|m| m.get(k)).copied().unwrap_or(0);
                let s = small_followups.get(tool_name).and_then(|m| m.get(k)).copied().unwrap_or(0);
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
                let l_pct = if lc > 0 { l_cnt * 100 / lc } else { 0 };
                let s_pct = if sc > 0 { s_cnt * 100 / sc } else { 0 };

                let diff_color = if l_pct > s_pct + 10 {
                    Color::Yellow // чаще при большом ответе
                } else if s_pct > l_pct + 10 {
                    Color::Cyan  // чаще при маленьком ответе
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
                let pct = if lc > 0 { cnt * 100 / lc } else { 0 };
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

    stats.sort_by(|a, b| b.count.cmp(&a.count));

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
    use comfy_table::{Cell, Color, Table, presets};

    // Таблица 1: размеры
    let mut table = Table::new();
    table.load_preset(presets::UTF8_BORDERS_ONLY);
    table.set_header(vec![
        "Инструмент", "Вызовов", "Median", "P75", "P90", "P99", "Max", "~tokens(P90)",
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
