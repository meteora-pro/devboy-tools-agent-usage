use std::collections::HashMap;

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use indicatif::{ProgressBar, ProgressStyle};

use crate::activity::db;
use crate::claude::parser;
use crate::claude::session::{self, AggregatedUsage, ClaudeSession};
use crate::cli::{GroupBy, OutputFormat, TaskSortBy};
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
        let window_events = db::load_window_events(
            &config.activitywatch_db_path,
            Some(session.start_time),
            Some(session.end_time),
        )?;
        let afk_events = db::load_afk_events(
            &config.activitywatch_db_path,
            Some(session.start_time),
            Some(session.end_time),
        )?;

        if window_events.is_empty() {
            None
        } else {
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

    // Загружаем ActivityWatch данные один раз
    let window_events =
        db::load_window_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
    let afk_events =
        db::load_afk_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;

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
        let w = db::load_window_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
        let a = db::load_afk_events(&config.activitywatch_db_path, Some(from_dt), Some(to_dt))?;
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
    let window_events = db::load_window_events(
        &config.activitywatch_db_path,
        Some(session.start_time),
        Some(session.end_time),
    )?;
    let afk_events = db::load_afk_events(
        &config.activitywatch_db_path,
        Some(session.start_time),
        Some(session.end_time),
    )?;

    if window_events.is_empty() {
        println!("No ActivityWatch data found for this session's time range.");
        return Ok(());
    }

    // Собираем browse stats
    let browse_stats =
        engine::collect_browse_stats(&window_events, session.start_time, session.end_time);

    // Собираем terminal focus stats
    let session_clone = clone_session_for_correlation(session);
    let terminal_stats =
        engine::collect_terminal_focus_stats(&session_clone, &window_events, &afk_events);

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
        (Some(w), Some(a))
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
