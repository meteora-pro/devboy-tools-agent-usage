use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::activity::models::{AwAfkEvent, AwWindowEvent};
use crate::classification::{Classifier, TurnContext};
use crate::claude::session::ClaudeSession;
use crate::claude::tokens;

use super::engine::collect_terminal_focus_stats;
use super::models::{TaskGroupSource, TaskStats, ToolCallStats};

/// Извлечение task ID из git branch name
///
/// Поддерживает паттерны:
/// - "feat/DEV-569-langfuse-integration" -> "DEV-569"
/// - "fix/PROJ-123-some-fix" -> "PROJ-123"
/// - "feature/377-add-button" -> "377"
/// - "DEV-569" (просто task ID) -> "DEV-569"
pub fn extract_task_id(branch: &str) -> Option<String> {
    // Паттерн 1: Jira-style ID (ABC-123)
    let jira_re = Regex::new(r"([A-Z][A-Z0-9]+-\d+)").unwrap();
    if let Some(m) = jira_re.find(branch) {
        return Some(m.as_str().to_string());
    }

    // Паттерн 2: числовой ID после типа ветки (feat/377-..., fix/42-...)
    let num_re =
        Regex::new(r"(?:feat|fix|feature|bugfix|hotfix|chore|refactor|task)/(\d+)").unwrap();
    if let Some(caps) = num_re.captures(branch) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }

    None
}

/// Описание из суффикса branch name (после task ID)
///
/// "feat/DEV-569-langfuse-integration" -> "langfuse integration"
/// "fix/377-add-login-button" -> "add login button"
pub fn description_from_branch(branch: &str, task_id: &str) -> Option<String> {
    // Находим позицию task_id в branch
    if let Some(pos) = branch.find(task_id) {
        let after = &branch[pos + task_id.len()..];
        // Убираем ведущий разделитель (- или /)
        let trimmed = after.trim_start_matches(|c| c == '-' || c == '/' || c == '_');
        if trimmed.is_empty() {
            return None;
        }
        // Заменяем разделители на пробелы
        let desc = trimmed.replace(['-', '_'], " ");
        return Some(desc);
    }
    None
}

/// Определить ключ задачи для turn с трёхуровневым fallback:
/// 1. Git branch task ID (DEV-569) -> TaskGroupSource::Branch
/// 2. LLM classification (если доступен) -> TaskGroupSource::Llm
/// 3. Session slug + message preview -> TaskGroupSource::Session
fn task_key_for_turn(
    turn: &crate::claude::session::Turn,
    session: &ClaudeSession,
    classifications: &HashMap<(String, String), String>,
) -> (String, TaskGroupSource, Option<String>) {
    // 1. Попытка извлечь task ID из git branch
    if let Some(id) = turn.git_branch.as_deref().and_then(extract_task_id) {
        let branch_str = turn.git_branch.as_deref().unwrap_or("");
        let description = description_from_branch(branch_str, &id);
        return (id, TaskGroupSource::Branch, description);
    }

    // 2. Проверяем LLM classification (по session_id + timestamp)
    let turn_ts_key = turn.user_timestamp.to_rfc3339();
    let session_id_str = session.session_id.to_string();
    if let Some(label) = classifications.get(&(session_id_str, turn_ts_key)) {
        return (label.clone(), TaskGroupSource::Llm, turn.user_message_preview.clone());
    }

    // 3. Fallback: session slug или укороченный session_id
    let key = session
        .slug
        .as_deref()
        .map(|s| format!("~{}", s))
        .unwrap_or_else(|| format!("~{}", &session.session_id.to_string()[..8]));
    // Описание: message preview -> git branch -> None
    let description = turn.user_message_preview.clone()
        .or_else(|| turn.git_branch.clone());
    (key, TaskGroupSource::Session, description)
}

/// Processing time одного turn (assistant_ts - user_ts)
pub fn compute_turn_agent_time(
    user_ts: DateTime<Utc>,
    assistant_ts: Option<DateTime<Utc>>,
) -> f64 {
    match assistant_ts {
        Some(at) => {
            let ms = (at - user_ts).num_milliseconds() as f64 / 1000.0;
            ms.max(0.0)
        }
        None => 0.0,
    }
}

/// Вычислить scale factor для распределения subagent cost по turns
///
/// session.total_usage включает subagent cost, а turn-level cost — нет.
/// Scale factor = session.total_cost / sum(turn_costs)
fn compute_session_cost_scales(sessions: &[&ClaudeSession]) -> HashMap<String, f64> {
    let mut scales = HashMap::new();

    for session in sessions {
        let turn_sum: f64 = session.turns.iter()
            .map(|t| {
                t.usage
                    .as_ref()
                    .map(|u| tokens::calculate_cost(u, t.model.as_deref().unwrap_or("sonnet")))
                    .unwrap_or(0.0)
            })
            .sum();

        let session_total = session.total_usage.estimated_cost_usd;
        let scale = if turn_sum > 0.0 {
            session_total / turn_sum
        } else {
            1.0
        };

        scales.insert(session.session_id.to_string(), scale);
    }

    scales
}

/// Группировка sessions по задачам (turn-level branch)
pub fn build_task_stats(
    sessions: &[&ClaudeSession],
    window_events: Option<&[AwWindowEvent]>,
    afk_events: Option<&[AwAfkEvent]>,
    classifier: Option<&Classifier>,
) -> Vec<TaskStats> {
    // Phase 0: вычисляем scale factor для subagent cost
    let cost_scales = compute_session_cost_scales(sessions);

    // Phase 1: собираем turns без task ID для LLM classification
    let mut classify_requests = Vec::new();
    for session in sessions {
        for turn in &session.turns {
            if turn.git_branch.as_deref().and_then(extract_task_id).is_none() {
                if let Some(preview) = &turn.user_message_preview {
                    classify_requests.push(crate::classification::ClassifyRequest {
                        session_id: session.session_id.to_string(),
                        turn_timestamp: turn.user_timestamp,
                        message_preview: preview.clone(),
                        git_branch: turn.git_branch.clone(),
                        project_name: session.project_name.clone(),
                        session_slug: session.slug.clone(),
                    });
                }
            }
        }
    }

    // Phase 2: классифицируем через cache/LLM/fallback
    let mut classifications: HashMap<(String, String), String> = if let Some(clf) = classifier {
        clf.classify_turns(&classify_requests)
            .into_iter()
            .map(|((sid, ts), c)| ((sid, ts), c.label))
            .collect()
    } else {
        HashMap::new()
    };

    // Phase 2.5: Классификация orphan turns (нет branch task ID, нет preview)
    // Стратегия: наследуем классификацию от sibling turns в той же сессии,
    // или строим контекст всей сессии для LLM
    {
        // Собираем per-session labels из Phase 2
        let mut session_labels: HashMap<String, Vec<String>> = HashMap::new();
        for ((sid, _ts), label) in &classifications {
            session_labels.entry(sid.clone()).or_default().push(label.clone());
        }

        // Отслеживаем полностью orphan сессии (ни один turn не классифицирован)
        let mut fully_orphan_sessions: Vec<(&ClaudeSession, Vec<DateTime<Utc>>)> = Vec::new();

        for session in sessions {
            let sid = session.session_id.to_string();
            let mut orphan_timestamps: Vec<DateTime<Utc>> = Vec::new();

            for turn in &session.turns {
                // Turn с branch task ID — обработан в Phase 3 напрямую
                if turn.git_branch.as_deref().and_then(extract_task_id).is_some() {
                    continue;
                }
                // Turn с preview — был отправлен на классификацию в Phase 1-2
                if turn.user_message_preview.is_some() {
                    continue;
                }
                orphan_timestamps.push(turn.user_timestamp);
            }

            if orphan_timestamps.is_empty() {
                continue;
            }

            // Наследуем от sibling turns в этой сессии
            if let Some(labels) = session_labels.get(&sid) {
                let dominant = find_dominant_label(labels);
                for ts in &orphan_timestamps {
                    classifications.insert((sid.clone(), ts.to_rfc3339()), dominant.clone());
                }
            } else {
                // Нет ни одного классифицированного turn — нужна session-level классификация
                fully_orphan_sessions.push((session, orphan_timestamps));
            }
        }

        // Для полностью orphan сессий: строим контекст и классифицируем через LLM
        if let Some(clf) = classifier {
            if !fully_orphan_sessions.is_empty() {
                let session_requests: Vec<crate::classification::ClassifyRequest> =
                    fully_orphan_sessions.iter().map(|(session, _)| {
                        crate::classification::ClassifyRequest {
                            session_id: session.session_id.to_string(),
                            turn_timestamp: session.start_time,
                            message_preview: build_session_context(session),
                            git_branch: session.git_branch.clone(),
                            project_name: session.project_name.clone(),
                            session_slug: session.slug.clone(),
                        }
                    }).collect();

                let new_clf = clf.classify_turns(&session_requests);

                for (session, orphan_timestamps) in &fully_orphan_sessions {
                    let sid = session.session_id.to_string();
                    let session_ts_key = session.start_time.to_rfc3339();
                    if let Some(label) = new_clf.get(&(sid.clone(), session_ts_key)).map(|c| &c.label) {
                        for ts in orphan_timestamps {
                            classifications.insert((sid.clone(), ts.to_rfc3339()), label.clone());
                        }
                    }
                }
            }
        }
    }

    // Phase 3: группируем turns с учётом subagent cost scale
    let mut task_map: HashMap<String, TaskAccumulator> = HashMap::new();
    // Собираем turn контексты для суммаризации
    let mut task_turn_contexts: HashMap<String, Vec<TurnContext>> = HashMap::new();

    for session in sessions {
        let scale = cost_scales.get(&session.session_id.to_string())
            .copied()
            .unwrap_or(1.0);

        for turn in &session.turns {
            let (task_id, group_source, description) =
                task_key_for_turn(turn, session, &classifications);

            let agent_time = compute_turn_agent_time(
                turn.user_timestamp,
                turn.assistant_timestamp,
            );

            let raw_cost = turn
                .usage
                .as_ref()
                .map(|u| {
                    tokens::calculate_cost(u, turn.model.as_deref().unwrap_or("sonnet"))
                })
                .unwrap_or(0.0);

            // Применяем scale factor для учёта subagent cost
            let cost = raw_cost * scale;

            let ts = turn
                .assistant_timestamp
                .unwrap_or(turn.user_timestamp);

            let acc = task_map.entry(task_id.clone()).or_insert_with(|| {
                TaskAccumulator {
                    task_id: task_id.clone(),
                    description: description.clone(),
                    group_source: group_source.clone(),
                    project_names: HashSet::new(),
                    session_ids: HashSet::new(),
                    turn_count: 0,
                    human_turn_count: 0,
                    agent_time_secs: 0.0,
                    cost_usd: 0.0,
                    first_seen: ts,
                    last_seen: ts,
                    tool_calls: ToolCallStats::default(),
                }
            });

            // Обновляем описание если ещё нет
            if acc.description.is_none() && description.is_some() {
                acc.description = description;
            }

            acc.project_names.insert(session.project_name.clone());
            acc.session_ids.insert(session.session_id.to_string());
            acc.turn_count += 1;
            if turn.user_message_preview.is_some() {
                acc.human_turn_count += 1;
            }
            acc.agent_time_secs += agent_time;
            acc.cost_usd += cost;
            for tc in &turn.tool_calls {
                acc.tool_calls.add_tool(tc);
            }
            if ts < acc.first_seen {
                acc.first_seen = ts;
            }
            if ts > acc.last_seen {
                acc.last_seen = ts;
            }

            // Собираем turn context для суммаризации (все turns — hierarchical summarization обработает)
            let contexts = task_turn_contexts.entry(task_id).or_default();
            contexts.push(TurnContext {
                timestamp: turn.user_timestamp.format("%H:%M").to_string(),
                user_preview: turn.user_message_preview.clone(),
                tool_calls: turn.tool_calls.clone(),
                agent_time_secs: agent_time,
            });
        }
    }

    // Phase 4: LLM суммаризация диалогов по задачам (если classifier доступен)
    let summaries: HashMap<String, crate::classification::TaskSummary> =
        if let Some(clf) = classifier {
            let summary_requests: Vec<crate::classification::TaskSummaryRequest> = task_map
                .values()
                .map(|acc| {
                    let project_name = if acc.project_names.len() == 1 {
                        acc.project_names.iter().next().unwrap().clone()
                    } else {
                        "various".to_string()
                    };
                    let turns = task_turn_contexts
                        .remove(&acc.task_id)
                        .unwrap_or_default();

                    crate::classification::TaskSummaryRequest {
                        task_id: acc.task_id.clone(),
                        project_name,
                        turn_count: acc.turn_count,
                        last_turn_ts: acc.last_seen,
                        first_seen: acc.first_seen,
                        turns,
                    }
                })
                .collect();

            clf.summarize_tasks(&summary_requests)
        } else {
            HashMap::new()
        };

    // Собираем human_time и dirty_human_time если есть AW данные
    let (human_times, dirty_times): (HashMap<String, f64>, HashMap<String, f64>) =
        if let (Some(w_events), Some(a_events)) = (window_events, afk_events) {
            compute_human_times_per_task(sessions, w_events, a_events, &classifications)
        } else {
            (HashMap::new(), HashMap::new())
        };

    // Загружаем manual titles если classifier доступен
    let task_ids: Vec<String> = task_map.keys().cloned().collect();
    let manual_titles: HashMap<String, String> = if let Some(clf) = classifier {
        clf.get_manual_titles(&task_ids)
    } else {
        HashMap::new()
    };

    // Конвертируем в TaskStats
    task_map
        .into_values()
        .map(|acc| {
            let project_name = if acc.project_names.len() == 1 {
                acc.project_names.into_iter().next().unwrap()
            } else {
                "various".to_string()
            };

            // LLM summary заменяет branch description если доступен
            let (description, status, llm_title) = if let Some(s) = summaries.get(&acc.task_id) {
                (Some(s.summary.clone()), s.status.clone(), s.title.clone())
            } else {
                (acc.description, None, None)
            };

            // Короткие ID сессий (первые 8 символов), отсортированные по времени
            // Самый ранний session_id — первый (для display_id)
            let mut full_ids_sorted: Vec<&String> = acc.session_ids.iter().collect();
            full_ids_sorted.sort();
            let short_ids: Vec<String> = full_ids_sorted.iter()
                .map(|id| id[..8.min(id.len())].to_string())
                .collect();
            let first_session_id = short_ids.first().cloned().unwrap_or_default();

            // display_id: технический ID для команд и отображения
            // Branch: DEV-570, LLM/Session: первый session ID
            let display_id = match acc.group_source {
                TaskGroupSource::Branch => acc.task_id.clone(),
                _ => first_session_id.clone(),
            };

            // Приоритет title: manual > llm > fallback
            // Fallback зависит от source:
            //   Branch: "DEV-570 <llm_title или branch_desc>"
            //   LLM: task_id (LLM classification label) — это и есть название задачи
            //   Session: slug без "~"
            let manual_title = manual_titles.get(&acc.task_id).cloned();
            let title = if let Some(mt) = manual_title {
                // Manual title: для branch — добавляем ID
                match acc.group_source {
                    TaskGroupSource::Branch => Some(format!("{} {}", acc.task_id, mt)),
                    _ => Some(mt),
                }
            } else if let Some(lt) = llm_title {
                // LLM title: для branch — добавляем ID
                match acc.group_source {
                    TaskGroupSource::Branch => Some(format!("{} {}", acc.task_id, lt)),
                    _ => Some(lt),
                }
            } else {
                // Нет title: строим fallback
                match acc.group_source {
                    TaskGroupSource::Branch => {
                        // "DEV-570 langfuse integration" из branch description
                        if let Some(ref desc) = description {
                            let short = if desc.len() > 50 { &desc[..50] } else { desc };
                            Some(format!("{} {}", acc.task_id, short))
                        } else {
                            Some(acc.task_id.clone())
                        }
                    }
                    TaskGroupSource::Llm => {
                        // LLM label как title (это classification label), кроме "unclassified"
                        if acc.task_id == "unclassified" {
                            None
                        } else {
                            Some(acc.task_id.clone())
                        }
                    }
                    TaskGroupSource::Session => {
                        // Slug без "~" prefix
                        let slug = acc.task_id.trim_start_matches('~');
                        Some(slug.to_string())
                    }
                }
            };

            TaskStats {
                display_id,
                task_id: acc.task_id.clone(),
                description,
                project_name,
                session_count: acc.session_ids.len(),
                session_ids: short_ids,
                turn_count: acc.turn_count,
                human_turn_count: acc.human_turn_count,
                agent_time_secs: acc.agent_time_secs,
                human_time_secs: human_times.get(&acc.task_id).copied(),
                dirty_human_time_secs: dirty_times.get(&acc.task_id).copied(),
                cost_usd: acc.cost_usd,
                first_seen: acc.first_seen,
                last_seen: acc.last_seen,
                group_source: acc.group_source,
                status,
                title,
                tool_calls: acc.tool_calls,
            }
        })
        .collect()
}

/// Вычисляет human focused time и dirty human time на задачу через TerminalFocusStats
///
/// Для каждой сессии вычисляем terminal focus stats, затем пропорционально
/// распределяем human_focused_secs и dirty_human_secs по задачам на основе agent_time.
///
/// Возвращает (human_times, dirty_times)
fn compute_human_times_per_task(
    sessions: &[&ClaudeSession],
    window_events: &[AwWindowEvent],
    afk_events: &[AwAfkEvent],
    classifications: &HashMap<(String, String), String>,
) -> (HashMap<String, f64>, HashMap<String, f64>) {
    let mut human_result: HashMap<String, f64> = HashMap::new();
    let mut dirty_result: HashMap<String, f64> = HashMap::new();

    for session in sessions {
        let terminal_stats =
            collect_terminal_focus_stats(session, window_events, afk_events);

        let total_human = terminal_stats.human_focused_secs;
        let total_dirty = terminal_stats.dirty_human_secs;

        if total_human <= 0.0 && total_dirty <= 0.0 {
            continue;
        }

        // Считаем agent_time per task в этой сессии
        let mut task_agent_time: HashMap<String, f64> = HashMap::new();
        let mut total_agent_time = 0.0;

        for turn in &session.turns {
            let (task_id, _, _) = task_key_for_turn(turn, session, classifications);

            let agent_time = compute_turn_agent_time(
                turn.user_timestamp,
                turn.assistant_timestamp,
            );

            *task_agent_time.entry(task_id).or_default() += agent_time;
            total_agent_time += agent_time;
        }

        // Распределяем human_time и dirty_time пропорционально agent_time
        if total_agent_time > 0.0 {
            for (task_id, agent_time) in &task_agent_time {
                let proportion = agent_time / total_agent_time;
                if total_human > 0.0 {
                    *human_result.entry(task_id.clone()).or_default() += total_human * proportion;
                }
                if total_dirty > 0.0 {
                    *dirty_result.entry(task_id.clone()).or_default() += total_dirty * proportion;
                }
            }
        }
    }

    (human_result, dirty_result)
}

/// Найти сессии по task ID (display_id, task_id или подстрока)
///
/// Строит task → session mapping через branch + cached LLM классификации (без нового inference).
/// Возвращает (task_title, sorted_full_session_uuids) или None если не найдено.
pub fn find_sessions_by_task_id(
    id: &str,
    sessions: &[&ClaudeSession],
    classifier: Option<&Classifier>,
) -> Option<(String, Vec<String>)> {
    // Собираем LLM classifications из кеша (без нового inference)
    let classifications: HashMap<(String, String), String> = if let Some(clf) = classifier {
        // Собираем все turns без branch task ID для поиска cached classifications
        let mut requests = Vec::new();
        for session in sessions {
            for turn in &session.turns {
                if turn.git_branch.as_deref().and_then(extract_task_id).is_none() {
                    if let Some(preview) = &turn.user_message_preview {
                        requests.push(crate::classification::ClassifyRequest {
                            session_id: session.session_id.to_string(),
                            turn_timestamp: turn.user_timestamp,
                            message_preview: preview.clone(),
                            git_branch: turn.git_branch.clone(),
                            project_name: session.project_name.clone(),
                            session_slug: session.slug.clone(),
                        });
                    }
                }
            }
        }
        // classify_turns вернёт кешированные результаты; если LLM client отсутствует —
        // вернёт только то, что есть в кеше
        clf.classify_turns(&requests)
            .into_iter()
            .map(|((sid, ts), c)| ((sid, ts), c.label))
            .collect()
    } else {
        HashMap::new()
    };

    // Строим task → sessions mapping
    let mut task_sessions: HashMap<String, (HashSet<String>, Option<String>)> = HashMap::new();
    // Также строим display_id → task_id mapping
    let mut display_to_task: HashMap<String, String> = HashMap::new();

    for session in sessions {
        for turn in &session.turns {
            let (task_id, group_source, description) =
                task_key_for_turn(turn, session, &classifications);

            let entry = task_sessions.entry(task_id.clone()).or_insert_with(|| {
                (HashSet::new(), None)
            });
            entry.0.insert(session.session_id.to_string());
            if entry.1.is_none() {
                entry.1 = description;
            }

            // display_id для branch = task_id, для других = session short id
            let display_id = match group_source {
                TaskGroupSource::Branch => task_id.clone(),
                _ => session.session_id.to_string()[..8].to_string(),
            };
            display_to_task.insert(display_id, task_id);
        }
    }

    // Загружаем manual titles и task summaries из кеша
    let task_ids: Vec<String> = task_sessions.keys().cloned().collect();
    let manual_titles: HashMap<String, String> = if let Some(clf) = classifier {
        clf.get_manual_titles(&task_ids)
    } else {
        HashMap::new()
    };

    let summaries: HashMap<String, crate::classification::TaskSummary> =
        if let Some(clf) = classifier {
            // Проверяем кешированные summaries (без нового LLM inference)
            let summary_requests: Vec<crate::classification::TaskSummaryRequest> = Vec::new();
            clf.summarize_tasks(&summary_requests)
        } else {
            HashMap::new()
        };

    // Поиск: точное совпадение по task_id
    if let Some((session_ids, desc)) = task_sessions.get(id) {
        let title = build_task_title(id, desc.as_deref(), &manual_titles, &summaries);
        let mut ids: Vec<String> = session_ids.iter().cloned().collect();
        ids.sort();
        return Some((title, ids));
    }

    // Поиск: через display_id → task_id
    if let Some(task_id) = display_to_task.get(id) {
        if let Some((session_ids, desc)) = task_sessions.get(task_id) {
            let title = build_task_title(task_id, desc.as_deref(), &manual_titles, &summaries);
            let mut ids: Vec<String> = session_ids.iter().cloned().collect();
            ids.sort();
            return Some((title, ids));
        }
    }

    // Поиск: подстрока в task_id или display_id (case-insensitive)
    let id_lower = id.to_lowercase();
    for (task_id, (session_ids, desc)) in &task_sessions {
        if task_id.to_lowercase().contains(&id_lower) {
            let title = build_task_title(task_id, desc.as_deref(), &manual_titles, &summaries);
            let mut ids: Vec<String> = session_ids.iter().cloned().collect();
            ids.sort();
            return Some((title, ids));
        }
    }
    for (display_id, task_id) in &display_to_task {
        if display_id.to_lowercase().contains(&id_lower) {
            if let Some((session_ids, desc)) = task_sessions.get(task_id) {
                let title = build_task_title(task_id, desc.as_deref(), &manual_titles, &summaries);
                let mut ids: Vec<String> = session_ids.iter().cloned().collect();
                ids.sort();
                return Some((title, ids));
            }
        }
    }

    None
}

/// Построить заголовок задачи из доступных данных (manual > llm > description > task_id)
fn build_task_title(
    task_id: &str,
    description: Option<&str>,
    manual_titles: &HashMap<String, String>,
    summaries: &HashMap<String, crate::classification::TaskSummary>,
) -> String {
    if let Some(mt) = manual_titles.get(task_id) {
        return mt.clone();
    }
    if let Some(s) = summaries.get(task_id) {
        if let Some(ref t) = s.title {
            return t.clone();
        }
    }
    if let Some(desc) = description {
        return desc.to_string();
    }
    task_id.to_string()
}

struct TaskAccumulator {
    task_id: String,
    description: Option<String>,
    group_source: TaskGroupSource,
    project_names: HashSet<String>,
    session_ids: HashSet<String>,
    turn_count: usize,
    human_turn_count: usize,
    agent_time_secs: f64,
    cost_usd: f64,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    tool_calls: ToolCallStats,
}

/// Найти самый частый label в списке
fn find_dominant_label(labels: &[String]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for label in labels {
        *counts.entry(label.as_str()).or_default() += 1;
    }
    counts.into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(label, _)| label.to_string())
        .unwrap_or_default()
}

/// Построить контекст сессии для классификации orphan turns
/// (turns без user_message_preview — tool results, system reminders)
fn build_session_context(session: &ClaudeSession) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Session ID для трейсинга
    let sid = session.session_id.to_string();
    parts.push(format!("[session:{}]", &sid[..8.min(sid.len())]));

    // Project
    parts.push(format!("project:{}", session.project_name));

    // Slug
    if let Some(slug) = &session.slug {
        parts.push(format!("slug:{}", slug));
    }

    // Git branch
    if let Some(branch) = &session.git_branch {
        parts.push(format!("branch:{}", branch));
    }

    // Git branches из individual turns (могут отличаться от session-level)
    let turn_branches: HashSet<&str> = session.turns.iter()
        .filter_map(|t| t.git_branch.as_deref())
        .collect();
    if !turn_branches.is_empty() {
        let branches: Vec<&str> = turn_branches.into_iter().collect();
        parts.push(format!("turn_branches:{}", branches.join(",")));
    }

    // Все доступные message previews из других turns
    let previews: Vec<&str> = session.turns.iter()
        .filter_map(|t| t.user_message_preview.as_deref())
        .collect();
    if !previews.is_empty() {
        parts.push(format!("messages: {}", previews.join(" | ")));
    }

    // Tool calls summary (уникальные tools с количеством)
    let mut tool_counts: HashMap<&str, usize> = HashMap::new();
    for turn in &session.turns {
        for tc in &turn.tool_calls {
            *tool_counts.entry(tc.as_str()).or_default() += 1;
        }
    }
    if !tool_counts.is_empty() {
        let mut summary: Vec<String> = tool_counts.iter()
            .map(|(name, count)| format!("{}x{}", name, count))
            .collect();
        summary.sort();
        parts.push(format!("tools:{}", summary.join(",")));
    }

    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_task_id_jira_style() {
        assert_eq!(
            extract_task_id("feat/DEV-569-langfuse-integration"),
            Some("DEV-569".to_string())
        );
        assert_eq!(
            extract_task_id("fix/PROJ-123-some-fix"),
            Some("PROJ-123".to_string())
        );
        assert_eq!(
            extract_task_id("DEV-42"),
            Some("DEV-42".to_string())
        );
    }

    #[test]
    fn test_extract_task_id_numeric() {
        assert_eq!(
            extract_task_id("feature/377-add-button"),
            Some("377".to_string())
        );
        assert_eq!(
            extract_task_id("fix/42-hotfix"),
            Some("42".to_string())
        );
        assert_eq!(
            extract_task_id("feat/100"),
            Some("100".to_string())
        );
    }

    #[test]
    fn test_extract_task_id_none() {
        assert_eq!(extract_task_id("main"), None);
        assert_eq!(extract_task_id("develop"), None);
        assert_eq!(extract_task_id("my-branch"), None);
    }

    #[test]
    fn test_description_from_branch() {
        assert_eq!(
            description_from_branch("feat/DEV-569-langfuse-integration", "DEV-569"),
            Some("langfuse integration".to_string())
        );
        assert_eq!(
            description_from_branch("fix/377-add-login-button", "377"),
            Some("add login button".to_string())
        );
        assert_eq!(
            description_from_branch("feat/DEV-42", "DEV-42"),
            None
        );
    }

    #[test]
    fn test_compute_turn_agent_time() {
        use chrono::TimeZone;
        let user_ts = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap();
        let assistant_ts = Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 30).unwrap();

        assert!((compute_turn_agent_time(user_ts, Some(assistant_ts)) - 30.0).abs() < 0.01);
        assert_eq!(compute_turn_agent_time(user_ts, None), 0.0);
    }

    #[test]
    fn test_compute_session_cost_scales() {
        use uuid::Uuid;
        use crate::claude::session::{AggregatedUsage, Turn};
        use crate::claude::models::TokenUsage;

        // Сессия с total_usage = $10, но turn costs = $5 (subagent = $5)
        let session = ClaudeSession {
            session_id: Uuid::new_v4(),
            project_name: "test".to_string(),
            project_path: "/test".to_string(),
            start_time: Utc::now(),
            end_time: Utc::now(),
            git_branch: None,
            version: None,
            slug: None,
            turns: vec![Turn {
                user_timestamp: Utc::now(),
                assistant_timestamp: Some(Utc::now()),
                turn_duration_ms: None,
                tool_calls: vec![],
                tool_call_details: vec![],
                usage: Some(TokenUsage {
                    input_tokens: 100_000,
                    output_tokens: 1_000,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
                model: Some("sonnet".to_string()),
                git_branch: None,
                user_message_preview: None,
                context_tokens: None,
            }],
            total_usage: AggregatedUsage {
                input_tokens: 200_000,
                output_tokens: 2_000,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                estimated_cost_usd: 0.66, // $0.30 input + $0.03 output = $0.33 per turn, session = $0.66
                request_count: 2,
            },
            is_subagent: false,
            compactions: Vec::new(),
        };

        let sessions: Vec<&ClaudeSession> = vec![&session];
        let scales = compute_session_cost_scales(&sessions);
        let scale = scales.get(&session.session_id.to_string()).unwrap();

        // Scale should be ~2.0 (session cost / turn cost)
        assert!(*scale > 1.5 && *scale < 2.5, "scale = {}", scale);
    }
}
