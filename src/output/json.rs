use serde_json::json;

use crate::claude::session::{AggregatedUsage, ClaudeSession};
use crate::correlation::models::{BrowseStats, CorrelatedSession, TaskStats, TerminalFocusStats};

/// Вывод проектов в JSON
pub fn projects_json(projects: &[(String, usize, AggregatedUsage)]) {
    let data: Vec<serde_json::Value> = projects
        .iter()
        .map(|(name, sessions, usage)| {
            json!({
                "project": name,
                "sessions": sessions,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "estimated_cost_usd": usage.estimated_cost_usd,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод сессий в JSON
pub fn sessions_json(sessions: &[&ClaudeSession]) {
    let data: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id.to_string(),
                "project": s.project_name,
                "start_time": s.start_time.to_rfc3339(),
                "end_time": s.end_time.to_rfc3339(),
                "duration_secs": s.duration().num_seconds(),
                "turns": s.turns.len(),
                "input_tokens": s.total_usage.input_tokens,
                "output_tokens": s.total_usage.output_tokens,
                "estimated_cost_usd": s.total_usage.estimated_cost_usd,
                "git_branch": s.git_branch,
                "slug": s.slug,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод сводки в JSON
pub fn summary_json(
    total_sessions: usize,
    total_turns: usize,
    total_usage: &AggregatedUsage,
    total_duration_secs: i64,
) {
    let data = json!({
        "sessions": total_sessions,
        "turns": total_turns,
        "duration_secs": total_duration_secs,
        "requests": total_usage.request_count,
        "input_tokens": total_usage.input_tokens,
        "output_tokens": total_usage.output_tokens,
        "cache_creation_tokens": total_usage.cache_creation_tokens,
        "cache_read_tokens": total_usage.cache_read_tokens,
        "estimated_cost_usd": total_usage.estimated_cost_usd,
    });

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод фокуса в JSON
pub fn focus_json(sessions: &[CorrelatedSession]) {
    let data: Vec<serde_json::Value> = sessions
        .iter()
        .map(|cs| {
            let stats = &cs.focus_stats;
            json!({
                "session_id": cs.session.session_id.to_string(),
                "project": cs.session.project_name,
                "total_processing_time_secs": stats.total_processing_time_secs,
                "total_thinking_time_secs": stats.total_thinking_time_secs,
                "focused_secs": stats.focused_during_processing_secs,
                "distracted_secs": stats.distracted_during_processing_secs,
                "afk_secs": stats.afk_during_processing_secs,
                "focus_percentage": stats.focus_percentage,
                "top_apps": stats.top_apps.iter().map(|(app, time)| {
                    json!({"app": app, "time_secs": time})
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод задач в JSON
pub fn tasks_json(tasks: &[TaskStats]) {
    let data: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "display_id": t.display_id,
                "task_id": t.task_id,
                "title": t.title,
                "description": t.description,
                "project": t.project_name,
                "group_source": t.group_source.label(),
                "status": t.status,
                "session_count": t.session_count,
                "session_ids": t.session_ids,
                "turn_count": t.turn_count,
                "human_turn_count": t.human_turn_count,
                "agent_time_secs": t.agent_time_secs,
                "human_time_secs": t.human_time_secs,
                "dirty_human_time_secs": t.dirty_human_time_secs,
                "cost_usd": t.cost_usd,
                "tool_calls": {
                    "total": t.tool_calls.total,
                    "read": t.tool_calls.read,
                    "write": t.tool_calls.write,
                    "bash": t.tool_calls.bash,
                    "mcp": t.tool_calls.mcp,
                    "devboy": t.tool_calls.devboy,
                },
                "first_seen": t.first_seen.to_rfc3339(),
                "last_seen": t.last_seen.to_rfc3339(),
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод browse stats в JSON
pub fn browse_json(
    session: &ClaudeSession,
    browse_stats: &BrowseStats,
    terminal_stats: &TerminalFocusStats,
) {
    let data = json!({
        "session_id": session.session_id.to_string(),
        "project": session.project_name,
        "slug": session.slug,
        "duration_secs": session.duration().num_seconds(),
        "turns": session.turns.len(),
        "cost_usd": session.total_usage.estimated_cost_usd,
        "work_related_pct": browse_stats.work_related_pct,
        "pages": browse_stats.pages.iter().map(|p| {
            json!({
                "title": p.title,
                "category": p.category.label(),
                "is_work_related": p.category.is_work_related(),
                "duration_secs": p.total_duration_secs,
                "visit_count": p.visit_count,
            })
        }).collect::<Vec<_>>(),
        "categories": browse_stats.categories.iter().map(|(cat, time)| {
            json!({
                "category": cat.label(),
                "is_work_related": cat.is_work_related(),
                "time_secs": time,
            })
        }).collect::<Vec<_>>(),
        "terminal_focus": {
            "human_focused_secs": terminal_stats.human_focused_secs,
            "agent_autonomous_secs": terminal_stats.agent_autonomous_secs,
            "afk_secs": terminal_stats.afk_secs,
            "other_app_secs": terminal_stats.other_app_secs,
            "total_processing_secs": terminal_stats.total_processing_secs,
            "total_thinking_secs": terminal_stats.total_thinking_secs,
        },
    });

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}

/// Вывод стоимости в JSON
pub fn cost_json(rows: &[(String, AggregatedUsage)]) {
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|(period, usage)| {
            json!({
                "period": period,
                "requests": usage.request_count,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_creation_tokens": usage.cache_creation_tokens,
                "cache_read_tokens": usage.cache_read_tokens,
                "estimated_cost_usd": usage.estimated_cost_usd,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&data).unwrap());
}
