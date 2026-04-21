use std::collections::HashMap;
use uuid::Uuid;

use super::session::{ClaudeSession, McpCallInfo, PIPELINE_TOOLS};

/// Одна "инвокация" pipeline-инструмента: первый запрос + все follow-up запросы
/// Пример: get_issues() → get_issues(chunk=2) → это одна инвокация с 2 чанками
#[derive(Debug, Clone)]
pub struct PipelineInvocation {
    pub session_id: Uuid,
    pub project_name: String,
    pub tool_name: String,
    /// Все вызовы этой инвокации (первый + follow-ups)
    pub calls: Vec<McpCallInfo>,
}

impl PipelineInvocation {
    /// Понадобился ли второй чанк? p₁=0 для этой инвокации
    pub fn needed_pagination(&self) -> bool {
        self.calls.iter().any(|c| c.is_follow_up())
    }

    /// Максимальный номер запрошенного чанка (1 если не было явного chunk параметра)
    pub fn max_chunk(&self) -> u64 {
        self.calls
            .iter()
            .filter_map(|c| c.chunk)
            .max()
            .unwrap_or(1)
    }

    /// Сколько всего чанков было запрошено
    pub fn total_chunks(&self) -> usize {
        // Уникальные chunk-номера + 1 за начальный вызов без chunk
        let has_initial = self.calls.iter().any(|c| c.chunk.is_none() || c.chunk == Some(1));
        let follow_ups: std::collections::HashSet<u64> = self
            .calls
            .iter()
            .filter(|c| c.chunk.map_or(false, |n| n > 1))
            .filter_map(|c| c.chunk)
            .collect();
        (if has_initial { 1 } else { 0 }) + follow_ups.len()
    }
}

/// Статистика по одному pipeline-инструменту
#[derive(Debug)]
pub struct ToolBehaviorStats {
    pub tool_name: String,
    /// Всего инвокаций (независимых задач)
    pub total_invocations: usize,
    /// Инвокации где НЕ потребовался второй чанк (первый чанк дал ответ)
    pub first_chunk_sufficient: usize,
    /// p₁ = вероятность что первого чанка достаточно
    pub p1: f64,
    /// E[chunks] = среднее кол-во чанков на инвокацию
    pub e_chunks: f64,
    /// Максимальный запрошенный чанк
    pub max_chunk_seen: u64,
    /// Распределение: сколько чанков → частота инвокаций
    pub chunk_count_distribution: HashMap<usize, usize>,
    /// Сколько разных сессий использовали этот инструмент
    pub sessions_using: usize,
    /// Сколько разных проектов
    pub projects_using: usize,
}

impl ToolBehaviorStats {
    pub fn p1_percent(&self) -> f64 {
        self.p1 * 100.0
    }
}

/// Извлечь все pipeline инвокации из сессий
///
/// Алгоритм: внутри каждой сессии проходим все turn'ы в хронологическом порядке.
/// Для каждого pipeline-инструмента:
///   - Вызов без chunk (или chunk=1) = старт новой инвокации
///   - Вызов с chunk>1 = продолжение последней открытой инвокации для этого инструмента
pub fn extract_pipeline_invocations(sessions: &[&ClaudeSession]) -> Vec<PipelineInvocation> {
    let mut result = Vec::new();

    for session in sessions {
        if session.is_subagent {
            continue;
        }

        // Открытые инвокации: tool_name → текущая инвокация (пока не встретили новый base call)
        let mut open: HashMap<String, PipelineInvocation> = HashMap::new();

        // Все MCP pipeline вызовы в хронологическом порядке через turn'ы
        for turn in &session.turns {
            for call in &turn.mcp_calls {
                if !PIPELINE_TOOLS.contains(&call.tool_name.as_str()) {
                    continue;
                }

                if call.is_follow_up() {
                    // Продолжение существующей инвокации
                    if let Some(inv) = open.get_mut(&call.tool_name) {
                        inv.calls.push(call.clone());
                    } else {
                        // Orphan follow-up (нет base call в логах — редко, но бывает)
                        // Создаём инвокацию только из follow-up
                        open.insert(
                            call.tool_name.clone(),
                            PipelineInvocation {
                                session_id: session.session_id,
                                project_name: session.project_name.clone(),
                                tool_name: call.tool_name.clone(),
                                calls: vec![call.clone()],
                            },
                        );
                    }
                } else {
                    // Новый base call — закрываем предыдущую инвокацию для этого инструмента
                    if let Some(prev) = open.remove(&call.tool_name) {
                        result.push(prev);
                    }
                    open.insert(
                        call.tool_name.clone(),
                        PipelineInvocation {
                            session_id: session.session_id,
                            project_name: session.project_name.clone(),
                            tool_name: call.tool_name.clone(),
                            calls: vec![call.clone()],
                        },
                    );
                }
            }
        }

        // Закрываем все оставшиеся открытые инвокации
        for (_, inv) in open {
            result.push(inv);
        }
    }

    result
}

/// Вычислить статистику по инструментам из инвокаций
pub fn compute_tool_stats(invocations: &[PipelineInvocation]) -> Vec<ToolBehaviorStats> {
    let mut by_tool: HashMap<&str, Vec<&PipelineInvocation>> = HashMap::new();
    for inv in invocations {
        by_tool.entry(&inv.tool_name).or_default().push(inv);
    }

    let mut stats: Vec<ToolBehaviorStats> = by_tool
        .into_iter()
        .map(|(tool_name, invs)| {
            let total = invs.len();
            let first_chunk_sufficient = invs.iter().filter(|i| !i.needed_pagination()).count();
            let p1 = if total > 0 {
                first_chunk_sufficient as f64 / total as f64
            } else {
                0.0
            };
            let e_chunks = if total > 0 {
                invs.iter().map(|i| i.total_chunks()).sum::<usize>() as f64 / total as f64
            } else {
                0.0
            };
            let max_chunk_seen = invs.iter().map(|i| i.max_chunk()).max().unwrap_or(1);

            let mut chunk_count_distribution: HashMap<usize, usize> = HashMap::new();
            for inv in &invs {
                *chunk_count_distribution
                    .entry(inv.total_chunks())
                    .or_default() += 1;
            }

            let sessions_using: std::collections::HashSet<Uuid> =
                invs.iter().map(|i| i.session_id).collect();
            let projects_using: std::collections::HashSet<&str> =
                invs.iter().map(|i| i.project_name.as_str()).collect();

            ToolBehaviorStats {
                tool_name: tool_name.to_string(),
                total_invocations: total,
                first_chunk_sufficient,
                p1,
                e_chunks,
                max_chunk_seen,
                chunk_count_distribution,
                sessions_using: sessions_using.len(),
                projects_using: projects_using.len(),
            }
        })
        .collect();

    stats.sort_by(|a, b| b.total_invocations.cmp(&a.total_invocations));
    stats
}

/// Итоговый отчёт по поведенческим паттернам
#[derive(Debug)]
pub struct BehaviorReport {
    pub tool_stats: Vec<ToolBehaviorStats>,
    pub total_sessions_analyzed: usize,
    pub sessions_with_pipeline_calls: usize,
    pub total_invocations: usize,
}

/// Построить полный отчёт по поведенческим паттернам
pub fn build_behavior_report(sessions: &[&ClaudeSession]) -> BehaviorReport {
    let invocations = extract_pipeline_invocations(sessions);

    let sessions_with_pipeline: std::collections::HashSet<Uuid> =
        invocations.iter().map(|i| i.session_id).collect();

    let tool_stats = compute_tool_stats(&invocations);

    BehaviorReport {
        tool_stats,
        total_sessions_analyzed: sessions.iter().filter(|s| !s.is_subagent).count(),
        sessions_with_pipeline_calls: sessions_with_pipeline.len(),
        total_invocations: invocations.len(),
    }
}
