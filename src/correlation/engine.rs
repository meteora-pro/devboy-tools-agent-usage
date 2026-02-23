use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::activity::classifier::{classify_app, classify_browser_title, clean_browser_title};
use crate::activity::models::{AfkStatus, AppCategory, AwAfkEvent, AwWindowEvent};
use crate::claude::session::ClaudeSession;

use super::models::*;

/// Коррелировать Claude сессию с ActivityWatch данными
pub fn correlate_session(
    session: ClaudeSession,
    window_events: &[AwWindowEvent],
    afk_events: &[AwAfkEvent],
) -> CorrelatedSession {
    let mut focus_periods = Vec::new();
    let mut stats = FocusStats::default();
    let mut app_time: HashMap<String, f64> = HashMap::new();

    for (i, turn) in session.turns.iter().enumerate() {
        let assistant_ts = match turn.assistant_timestamp {
            Some(ts) => ts,
            None => continue,
        };

        // Период 1: Claude обрабатывает запрос (user → assistant)
        let processing_start = turn.user_timestamp;
        let processing_end = assistant_ts;
        let processing_secs =
            (processing_end - processing_start).num_milliseconds() as f64 / 1000.0;

        if processing_secs > 0.0 {
            let activities =
                find_activities_in_range(window_events, processing_start, processing_end);
            let was_afk = is_afk_in_range(afk_events, processing_start, processing_end);

            // Считаем фокус/отвлечения
            for activity in &activities {
                if activity.category.is_focused() {
                    stats.focused_during_processing_secs += activity.duration_secs;
                } else {
                    stats.distracted_during_processing_secs += activity.duration_secs;
                }
                *app_time.entry(activity.app.clone()).or_default() += activity.duration_secs;
            }

            if was_afk {
                stats.afk_during_processing_secs += processing_secs;
            }

            stats.total_processing_time_secs += processing_secs;

            focus_periods.push(FocusPeriod {
                start: processing_start,
                end: processing_end,
                claude_state: ClaudeState::Processing,
                activities,
                was_afk,
            });
        }

        // Период 2: Пользователь думает (assistant → следующий user)
        if let Some(next_turn) = session.turns.get(i + 1) {
            let thinking_start = assistant_ts;
            let thinking_end = next_turn.user_timestamp;
            let thinking_secs = (thinking_end - thinking_start).num_milliseconds() as f64 / 1000.0;

            if thinking_secs > 0.0 && thinking_secs < 3600.0 {
                // Игнорируем промежутки > 1 часа (скорее всего разные сессии работы)
                let activities =
                    find_activities_in_range(window_events, thinking_start, thinking_end);
                let was_afk = is_afk_in_range(afk_events, thinking_start, thinking_end);

                stats.total_thinking_time_secs += thinking_secs;

                focus_periods.push(FocusPeriod {
                    start: thinking_start,
                    end: thinking_end,
                    claude_state: ClaudeState::UserThinking,
                    activities,
                    was_afk,
                });
            }
        }
    }

    // Считаем процент фокуса
    let total_active =
        stats.focused_during_processing_secs + stats.distracted_during_processing_secs;
    if total_active > 0.0 {
        stats.focus_percentage = stats.focused_during_processing_secs / total_active * 100.0;
    }

    // Топ приложений
    let mut top_apps: Vec<(String, f64)> = app_time.into_iter().collect();
    top_apps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top_apps.truncate(10);
    stats.top_apps = top_apps;

    CorrelatedSession {
        session,
        focus_periods,
        focus_stats: stats,
    }
}

/// Найти активности пользователя в заданном временном диапазоне
fn find_activities_in_range(
    events: &[AwWindowEvent],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Vec<UserActivity> {
    // Бинарный поиск: находим первый event, который может пересекаться
    let start_idx = events.partition_point(|e| e.end_time() <= start);

    let mut activities = Vec::new();

    for event in &events[start_idx..] {
        if event.timestamp >= end {
            break;
        }

        // Вычисляем пересечение временных интервалов
        let overlap_start = event.timestamp.max(start);
        let overlap_end = event.end_time().min(end);
        let overlap_secs = (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0;

        if overlap_secs > 0.0 {
            activities.push(UserActivity {
                app: event.app.clone(),
                title: event.title.clone(),
                category: classify_app(&event.app),
                duration_secs: overlap_secs,
            });
        }
    }

    activities
}

/// Собрать статистику браузерных страниц из всех Browser-активностей сессии
pub fn collect_browse_stats(
    window_events: &[AwWindowEvent],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> BrowseStats {
    // Находим все Browser-события в диапазоне сессии
    let start_idx = window_events.partition_point(|e| e.end_time() <= start);

    // Группируем по очищенному заголовку: (duration, visits)
    let mut page_map: HashMap<String, (f64, usize, String)> = HashMap::new();

    for event in &window_events[start_idx..] {
        if event.timestamp >= end {
            break;
        }

        // Только Browser-приложения
        if classify_app(&event.app) != AppCategory::Browser {
            continue;
        }

        // Считаем пересечение с диапазоном сессии
        let overlap_start = event.timestamp.max(start);
        let overlap_end = event.end_time().min(end);
        let overlap_secs = (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0;

        if overlap_secs > 0.0 {
            let cleaned = clean_browser_title(&event.title);
            let entry = page_map
                .entry(cleaned.clone())
                .or_insert_with(|| (0.0, 0, event.title.clone()));
            entry.0 += overlap_secs;
            entry.1 += 1;
        }
    }

    // Конвертируем в BrowsePage и сортируем по времени
    let mut pages: Vec<BrowsePage> = page_map
        .into_iter()
        .map(|(clean_title, (duration, visits, _raw_title))| {
            let category = classify_browser_title(&clean_title);
            BrowsePage {
                title: clean_title,
                category,
                total_duration_secs: duration,
                visit_count: visits,
            }
        })
        .collect();

    pages.sort_by(|a, b| {
        b.total_duration_secs
            .partial_cmp(&a.total_duration_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Агрегируем время по категориям
    let mut cat_time: HashMap<String, (crate::activity::models::BrowserCategory, f64)> =
        HashMap::new();
    for page in &pages {
        let key = page.category.label().to_string();
        let entry = cat_time
            .entry(key)
            .or_insert_with(|| (page.category.clone(), 0.0));
        entry.1 += page.total_duration_secs;
    }
    let mut categories: Vec<(crate::activity::models::BrowserCategory, f64)> =
        cat_time.into_values().collect();
    categories.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Считаем процент рабочих страниц
    let total_time: f64 = pages.iter().map(|p| p.total_duration_secs).sum();
    let work_time: f64 = pages
        .iter()
        .filter(|p| p.category.is_work_related())
        .map(|p| p.total_duration_secs)
        .sum();
    let work_related_pct = if total_time > 0.0 {
        work_time / total_time * 100.0
    } else {
        0.0
    };

    BrowseStats {
        pages,
        categories,
        work_related_pct,
    }
}

/// Проверить, был ли пользователь AFK в заданном диапазоне
fn is_afk_in_range(events: &[AwAfkEvent], start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    let start_idx = events.partition_point(|e| e.end_time() <= start);

    for event in &events[start_idx..] {
        if event.timestamp >= end {
            break;
        }

        // Проверяем пересечение
        let overlap_start = event.timestamp.max(start);
        let overlap_end = event.end_time().min(end);
        if overlap_end > overlap_start && event.status == AfkStatus::Afk {
            return true;
        }
    }

    false
}

/// Посчитать количество AFK-секунд в заданном диапазоне (гранулярно)
fn afk_seconds_in_range(events: &[AwAfkEvent], start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    let start_idx = events.partition_point(|e| e.end_time() <= start);
    let mut afk_secs = 0.0;

    for event in &events[start_idx..] {
        if event.timestamp >= end {
            break;
        }
        if event.status == AfkStatus::Afk {
            let overlap_start = event.timestamp.max(start);
            let overlap_end = event.end_time().min(end);
            let overlap = (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0;
            if overlap > 0.0 {
                afk_secs += overlap;
            }
        }
    }

    afk_secs
}

/// Проверить, является ли окно терминалом с данным basename и Claude
fn is_matching_terminal(app: &str, title: &str, basename: &str) -> bool {
    let app_lower = app.to_lowercase();
    let is_terminal = matches!(
        app_lower.as_str(),
        "terminal" | "iterm2" | "iterm" | "alacritty" | "kitty" | "wezterm" | "warp" | "hyper"
    );
    if !is_terminal {
        return false;
    }

    // Формат заголовка Terminal.app: "{basename} — {spinner} {title} — {process} ◂ claude ... — {cols}×{rows}"
    // Проверяем: начинается с basename + " —" и содержит "claude"
    let lower_title = title.to_lowercase();
    let basename_lower = basename.to_lowercase();

    lower_title.starts_with(&format!("{} \u{2014}", basename_lower))
        && lower_title.contains("claude")
}

/// Собрать статистику фокуса терминала: агент vs человек
///
/// Для каждого периода сессии (Processing/UserThinking) определяем:
/// - human_focused: пользователь смотрит на ЭТОТ терминал и не AFK
/// - agent_autonomous: Claude обрабатывает, а пользователь НЕ смотрит на этот терминал
/// - afk: пользователь AFK
/// - other_app: пользователь в другом приложении (не AFK)
pub fn collect_terminal_focus_stats(
    session: &ClaudeSession,
    window_events: &[AwWindowEvent],
    afk_events: &[AwAfkEvent],
) -> TerminalFocusStats {
    let basename = std::path::Path::new(&session.project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&session.project_name);

    let mut stats = TerminalFocusStats {
        human_focused_secs: 0.0,
        agent_autonomous_secs: 0.0,
        afk_secs: 0.0,
        other_app_secs: 0.0,
        total_processing_secs: 0.0,
        total_thinking_secs: 0.0,
        dirty_human_secs: 0.0,
    };

    for (i, turn) in session.turns.iter().enumerate() {
        let assistant_ts = match turn.assistant_timestamp {
            Some(ts) => ts,
            None => continue,
        };

        // Период 1: Processing (user → assistant)
        let processing_start = turn.user_timestamp;
        let processing_end = assistant_ts;
        let processing_secs =
            (processing_end - processing_start).num_milliseconds() as f64 / 1000.0;

        if processing_secs > 0.0 {
            stats.total_processing_secs += processing_secs;
            accumulate_focus_for_range(
                window_events,
                afk_events,
                processing_start,
                processing_end,
                basename,
                true, // is_processing
                &mut stats,
            );
        }

        // Период 2: UserThinking (assistant → следующий user)
        if let Some(next_turn) = session.turns.get(i + 1) {
            let thinking_start = assistant_ts;
            let thinking_end = next_turn.user_timestamp;
            let thinking_secs = (thinking_end - thinking_start).num_milliseconds() as f64 / 1000.0;

            if thinking_secs > 0.0 && thinking_secs < 3600.0 {
                stats.total_thinking_secs += thinking_secs;
                accumulate_focus_for_range(
                    window_events,
                    afk_events,
                    thinking_start,
                    thinking_end,
                    basename,
                    false, // is_processing
                    &mut stats,
                );
            }
        }
    }

    stats
}

/// Для заданного временного диапазона накопить метрики фокуса
fn accumulate_focus_for_range(
    window_events: &[AwWindowEvent],
    afk_events: &[AwAfkEvent],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    basename: &str,
    is_processing: bool,
    stats: &mut TerminalFocusStats,
) {
    let start_idx = window_events.partition_point(|e| e.end_time() <= start);

    for event in &window_events[start_idx..] {
        if event.timestamp >= end {
            break;
        }

        let overlap_start = event.timestamp.max(start);
        let overlap_end = event.end_time().min(end);
        let overlap_secs = (overlap_end - overlap_start).num_milliseconds() as f64 / 1000.0;

        if overlap_secs <= 0.0 {
            continue;
        }

        let afk_in_overlap = afk_seconds_in_range(afk_events, overlap_start, overlap_end);
        let not_afk_in_overlap = (overlap_secs - afk_in_overlap).max(0.0);

        let is_this_terminal = is_matching_terminal(&event.app, &event.title, basename);

        // Dirty human time: пользователь не AFK пока агент processing (любое приложение)
        if is_processing {
            stats.dirty_human_secs += not_afk_in_overlap;
        }

        // AFK время всегда добавляем в afk_secs
        stats.afk_secs += afk_in_overlap;

        if is_this_terminal {
            // Пользователь смотрит на этот терминал и не AFK
            stats.human_focused_secs += not_afk_in_overlap;
        } else if is_processing {
            // Claude работает, а пользователь смотрит на другое приложение
            stats.agent_autonomous_secs += not_afk_in_overlap;
        } else {
            // UserThinking, пользователь в другом приложении
            stats.other_app_secs += not_afk_in_overlap;
        }
    }
}

/// Собрать информацию о фокусе для каждого turn сессии
///
/// Для каждого turn определяем: какое приложение было в фокусе максимум времени,
/// был ли пользователь AFK, смотрел ли на терминал.
pub fn collect_per_turn_focus(
    session: &ClaudeSession,
    window_events: &[AwWindowEvent],
    afk_events: &[AwAfkEvent],
) -> Vec<TurnFocusInfo> {
    let basename = std::path::Path::new(&session.project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&session.project_name);

    let mut result = Vec::with_capacity(session.turns.len());

    for turn in &session.turns {
        let assistant_ts = match turn.assistant_timestamp {
            Some(ts) => ts,
            None => {
                result.push(TurnFocusInfo {
                    primary_app: None,
                    primary_title: None,
                    was_afk: false,
                    was_watching_terminal: false,
                    processing_secs: 0.0,
                    not_afk_secs: 0.0,
                    watching_terminal_secs: 0.0,
                });
                continue;
            }
        };

        let processing_start = turn.user_timestamp;
        let processing_end = assistant_ts;
        let processing_secs =
            (processing_end - processing_start).num_milliseconds() as f64 / 1000.0;

        if processing_secs <= 0.0 {
            result.push(TurnFocusInfo {
                primary_app: None,
                primary_title: None,
                was_afk: false,
                was_watching_terminal: false,
                processing_secs: 0.0,
                not_afk_secs: 0.0,
                watching_terminal_secs: 0.0,
            });
            continue;
        }

        // Находим приложения в диапазоне processing
        let activities = find_activities_in_range(window_events, processing_start, processing_end);

        // Считаем AFK
        let afk_secs = afk_seconds_in_range(afk_events, processing_start, processing_end);
        let not_afk_secs = (processing_secs - afk_secs).max(0.0);
        let was_afk = afk_secs > processing_secs * 0.5; // AFK > 50% времени

        // Находим приложение с максимальным временем и время на ЭТОМ терминале
        let mut app_time: HashMap<String, (f64, String)> = HashMap::new();
        let mut was_watching = false;
        let mut watching_terminal_secs: f64 = 0.0;

        for activity in &activities {
            let entry = app_time
                .entry(activity.app.clone())
                .or_insert_with(|| (0.0, activity.title.clone()));
            entry.0 += activity.duration_secs;

            if is_matching_terminal(&activity.app, &activity.title, basename) {
                was_watching = true;
                // Пропорционально вычитаем AFK из времени на терминале
                let afk_ratio = if processing_secs > 0.0 {
                    afk_secs / processing_secs
                } else {
                    0.0
                };
                watching_terminal_secs += activity.duration_secs * (1.0 - afk_ratio);
            }
        }

        let primary = app_time.into_iter().max_by(|a, b| {
            a.1 .0
                .partial_cmp(&b.1 .0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        result.push(TurnFocusInfo {
            primary_app: primary.as_ref().map(|(app, _)| app.clone()),
            primary_title: primary.map(|(_, (_, title))| title),
            was_afk,
            was_watching_terminal: was_watching,
            processing_secs,
            not_afk_secs,
            watching_terminal_secs,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_matching_terminal_basic() {
        assert!(is_matching_terminal(
            "Terminal",
            "dev-boy-env-1 — ⠐ Langfuse integration — caffeinate ◂ claude --dangerously-skip-permissions — 80×24",
            "dev-boy-env-1",
        ));
    }

    #[test]
    fn test_is_matching_terminal_case_insensitive() {
        assert!(is_matching_terminal(
            "Terminal",
            "Dev-Boy-Env-2 — ⠐ Some task — caffeinate ◂ claude — 120×40",
            "dev-boy-env-2",
        ));
    }

    #[test]
    fn test_is_matching_terminal_wrong_basename() {
        assert!(!is_matching_terminal(
            "Terminal",
            "dev-boy-env-1 — ⠐ Task — caffeinate ◂ claude — 80×24",
            "dev-boy-env-2",
        ));
    }

    #[test]
    fn test_is_matching_terminal_no_claude() {
        // Терминал с правильным basename, но без claude (обычная bash сессия)
        assert!(!is_matching_terminal(
            "Terminal",
            "dev-boy-env-1 — vim — zsh — 80×24",
            "dev-boy-env-1",
        ));
    }

    #[test]
    fn test_is_matching_terminal_wrong_app() {
        // Не терминальное приложение
        assert!(!is_matching_terminal(
            "Google Chrome",
            "dev-boy-env-1 — some page with claude — 80×24",
            "dev-boy-env-1",
        ));
    }

    #[test]
    fn test_is_matching_terminal_iterm() {
        assert!(is_matching_terminal(
            "iTerm2",
            "dev-boy-env-3 — ⠐ Migration — caffeinate ◂ claude — 100×30",
            "dev-boy-env-3",
        ));
    }
}
