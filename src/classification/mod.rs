pub mod cache;
mod client;
pub mod config;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use anyhow::Result;
use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

pub use cache::ClassificationCache;
pub use client::{
    ClassifyItem, CombineContext, LlmClient, LlmUsageStats, TaskContext, TaskSummary, TurnContext,
};
use config::LlmConfig;

use crate::correlation::models::TaskGroupSource;

/// Размер чанка (turns) для layer 0
const CHUNK_SIZE: usize = 30;
/// Максимум промежуточных summary для combine (layer 1+)
const COMBINE_SIZE: usize = 10;

/// Запрос на классификацию одного turn
pub struct ClassifyRequest {
    pub session_id: String,
    pub turn_timestamp: DateTime<Utc>,
    pub message_preview: String,
    pub git_branch: Option<String>,
    pub project_name: String,
    pub session_slug: Option<String>,
}

/// Результат классификации
pub struct Classification {
    pub label: String,
    pub source: TaskGroupSource,
}

/// Запрос на суммаризацию задачи
pub struct TaskSummaryRequest {
    pub task_id: String,
    pub project_name: String,
    pub turn_count: usize,
    pub last_turn_ts: DateTime<Utc>,
    pub first_seen: DateTime<Utc>,
    pub turns: Vec<TurnContext>,
}

/// Оркестратор классификации и суммаризации: cache -> LLM -> fallback
pub struct Classifier {
    cache: Mutex<ClassificationCache>,
    client: Option<LlmClient>,
    model: String,
    batch_size: usize,
    concurrency: usize,
}

impl Classifier {
    /// Создать Classifier
    ///
    /// Если API ключ не настроен (для Anthropic) — LLM client будет None,
    /// но кеш всё равно доступен
    pub fn new() -> Result<Self> {
        let cache = ClassificationCache::open()?;
        let config = LlmConfig::from_env();

        match config {
            Some(cfg) => {
                let model = cfg.model.clone();
                let batch_size = cfg.batch_size;
                let concurrency = cfg.concurrency;
                let client = LlmClient::new(cfg);
                Ok(Classifier {
                    cache: Mutex::new(cache),
                    client: Some(client),
                    model,
                    batch_size,
                    concurrency,
                })
            }
            None => {
                eprintln!("Warning: LLM provider not configured, classification disabled (using cache only)");
                Ok(Classifier {
                    cache: Mutex::new(cache),
                    client: None,
                    model: "unknown".to_string(),
                    batch_size: 20,
                    concurrency: 3,
                })
            }
        }
    }

    /// Получить ручные заголовки из кеша для списка задач
    pub fn get_manual_titles(&self, task_ids: &[String]) -> HashMap<String, String> {
        self.cache.lock().unwrap().get_manual_titles(task_ids)
    }

    /// Получить статистику LLM вызовов за текущий запуск
    pub fn get_usage_stats(&self) -> LlmUsageStats {
        match &self.client {
            Some(c) => c.usage_stats(),
            None => LlmUsageStats::default(),
        }
    }

    /// Классифицировать turns: cache -> LLM -> fallback
    ///
    /// Возвращает HashMap<(session_id, timestamp_rfc3339), Classification>
    pub fn classify_turns(
        &self,
        items: &[ClassifyRequest],
    ) -> HashMap<(String, String), Classification> {
        let mut result: HashMap<(String, String), Classification> = HashMap::new();

        if items.is_empty() {
            return result;
        }

        // 1. Проверяем кеш
        let cache_keys: Vec<(String, DateTime<Utc>)> = items
            .iter()
            .map(|r| (r.session_id.clone(), r.turn_timestamp))
            .collect();

        let cached = self.cache.lock().unwrap().get_batch(&cache_keys);

        // Добавляем кешированные результаты
        for ((sid, ts_str), label) in &cached {
            result.insert(
                (sid.clone(), ts_str.clone()),
                Classification {
                    label: label.clone(),
                    source: TaskGroupSource::Llm,
                },
            );
        }

        // 2. Собираем некешированные items
        let uncached: Vec<&ClassifyRequest> = items
            .iter()
            .filter(|r| {
                let ts_str = r.turn_timestamp.to_rfc3339();
                !cached.contains_key(&(r.session_id.clone(), ts_str))
            })
            .collect();

        let client = match &self.client {
            Some(c) => c,
            None => return result,
        };

        if uncached.is_empty() {
            return result;
        }

        // 3. Батчим и отправляем на LLM
        let batches: Vec<Vec<&ClassifyRequest>> = uncached
            .chunks(self.batch_size)
            .map(|chunk| chunk.to_vec())
            .collect();

        let new_results: Mutex<Vec<(String, DateTime<Utc>, String)>> = Mutex::new(Vec::new());

        let pool = ThreadPoolBuilder::new()
            .num_threads(self.concurrency)
            .build();

        match pool {
            Ok(pool) => {
                pool.install(|| {
                    batches.par_iter().for_each(|batch| {
                        let classify_items: Vec<ClassifyItem> = batch
                            .iter()
                            .map(|r| ClassifyItem {
                                message_preview: r.message_preview.clone(),
                                git_branch: r.git_branch.clone(),
                                project_name: r.project_name.clone(),
                            })
                            .collect();

                        match client.classify_batch(&classify_items) {
                            Ok(labels) => {
                                let mut results = new_results.lock().unwrap();
                                for (req, label) in batch.iter().zip(labels.iter()) {
                                    results.push((
                                        req.session_id.clone(),
                                        req.turn_timestamp,
                                        label.clone(),
                                    ));
                                }
                            }
                            Err(e) => {
                                eprintln!("LLM batch classification error: {}", e);
                            }
                        }
                    });
                });
            }
            Err(e) => {
                eprintln!("Failed to create thread pool: {}", e);
            }
        }

        // 4. Сохраняем в кеш и добавляем в результат
        let new_items = new_results.into_inner().unwrap();
        if !new_items.is_empty() {
            if let Err(e) = self
                .cache
                .lock()
                .unwrap()
                .store_batch(&new_items, &self.model)
            {
                eprintln!("Failed to cache classifications: {}", e);
            }

            for (sid, ts, label) in new_items {
                let ts_str = ts.to_rfc3339();
                result.insert(
                    (sid, ts_str),
                    Classification {
                        label,
                        source: TaskGroupSource::Llm,
                    },
                );
            }
        }

        result
    }

    /// Суммаризировать диалоги по задачам с иерархическим map-reduce
    ///
    /// Возвращает HashMap<task_id, TaskSummary>
    pub fn summarize_tasks(&self, requests: &[TaskSummaryRequest]) -> HashMap<String, TaskSummary> {
        let mut result: HashMap<String, TaskSummary> = HashMap::new();

        if requests.is_empty() {
            return result;
        }

        // 1. Проверяем top-level кеш (task_summaries) — быстрая проверка "уже всё посчитано?"
        let mut uncached: Vec<&TaskSummaryRequest> = Vec::new();

        for req in requests {
            let last_ts = req.last_turn_ts.to_rfc3339();
            if let Some(cached) =
                self.cache
                    .lock()
                    .unwrap()
                    .get_summary(&req.task_id, req.turn_count, &last_ts)
            {
                result.insert(req.task_id.clone(), cached);
            } else {
                uncached.push(req);
            }
        }

        let client = match &self.client {
            Some(c) => c,
            None => return result,
        };

        if uncached.is_empty() {
            return result;
        }

        // 2. Считаем общее число нод для progress bar
        let total_nodes: u64 = uncached
            .iter()
            .map(|req| count_nodes(req.turns.len()) as u64)
            .sum();

        let pb = ProgressBar::new(total_nodes);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} Summarizing [{bar:40.cyan/blue}] {pos}/{len} nodes ({msg})",
            )
            .unwrap()
            .progress_chars("█░░"),
        );

        // 3. Суммаризируем через hierarchical pipeline
        let new_results: Mutex<Vec<(String, usize, String, TaskSummary)>> = Mutex::new(Vec::new());

        let pool = ThreadPoolBuilder::new()
            .num_threads(self.concurrency)
            .build();

        match pool {
            Ok(pool) => {
                pool.install(|| {
                    uncached.par_iter().for_each(|req| {
                        match self.summarize_task_hierarchical(req, client, &pb) {
                            Ok(summary) => {
                                let last_ts = req.last_turn_ts.to_rfc3339();
                                let mut results = new_results.lock().unwrap();
                                results.push((
                                    req.task_id.clone(),
                                    req.turn_count,
                                    last_ts,
                                    summary,
                                ));
                            }
                            Err(e) => {
                                eprintln!("LLM summarization error for {}: {}", req.task_id, e);
                            }
                        }
                    });
                });
            }
            Err(e) => {
                eprintln!("Failed to create thread pool: {}", e);
            }
        }

        pb.finish_and_clear();

        // 4. Сохраняем в top-level кеш и добавляем в результат
        let new_items = new_results.into_inner().unwrap();
        for (task_id, turn_count, last_ts, summary) in new_items {
            if let Err(e) = self.cache.lock().unwrap().store_summary(
                &task_id,
                turn_count,
                &last_ts,
                &summary,
                &self.model,
            ) {
                eprintln!("Failed to cache summary for {}: {}", task_id, e);
            }
            result.insert(task_id, summary);
        }

        result
    }

    /// Иерархическая суммаризация одной задачи
    ///
    /// ≤30 turns: fast path (одиночный вызов LLM)
    /// >30: layer 0 chunks → layer 1+ combine → final
    fn summarize_task_hierarchical(
        &self,
        req: &TaskSummaryRequest,
        client: &LlmClient,
        pb: &ProgressBar,
    ) -> Result<TaskSummary> {
        let first_seen = req.first_seen.format("%Y-%m-%d %H:%M").to_string();
        let last_seen = req.last_turn_ts.format("%Y-%m-%d %H:%M").to_string();
        let project_name = req.project_name.clone();

        // Fast path: ≤ CHUNK_SIZE turns — одиночный вызов как раньше
        if req.turns.len() <= CHUNK_SIZE {
            pb.set_message(format!("task {}", req.task_id));
            let context = TaskContext {
                task_id: req.task_id.clone(),
                project_name,
                first_seen,
                last_seen,
                turns: req
                    .turns
                    .iter()
                    .map(|t| TurnContext {
                        timestamp: t.timestamp.clone(),
                        user_preview: t.user_preview.clone(),
                        tool_calls: t.tool_calls.clone(),
                        agent_time_secs: t.agent_time_secs,
                    })
                    .collect(),
            };
            let result = client.summarize_task(&context);
            pb.inc(1);
            return result;
        }

        // Layer 0: разбиваем turns на чанки по CHUNK_SIZE
        let chunks: Vec<&[TurnContext]> = req.turns.chunks(CHUNK_SIZE).collect();
        let total_chunks = chunks.len();
        let mut summaries: Vec<String> = Vec::with_capacity(total_chunks);

        for (i, chunk) in chunks.iter().enumerate() {
            pb.set_message(format!(
                "task {}, layer 0, chunk {}/{}",
                req.task_id,
                i + 1,
                total_chunks
            ));

            let chunk_hash = compute_chunk_hash_turns(chunk);

            // Проверяем chunk cache
            if let Some(cached) =
                self.cache
                    .lock()
                    .unwrap()
                    .get_chunk_summary(&req.task_id, 0, i, &chunk_hash)
            {
                summaries.push(cached.summary);
                pb.inc(1);
                continue;
            }

            // LLM вызов для чанка
            let context = TaskContext {
                task_id: req.task_id.clone(),
                project_name: project_name.clone(),
                first_seen: first_seen.clone(),
                last_seen: last_seen.clone(),
                turns: chunk
                    .iter()
                    .map(|t| TurnContext {
                        timestamp: t.timestamp.clone(),
                        user_preview: t.user_preview.clone(),
                        tool_calls: t.tool_calls.clone(),
                        agent_time_secs: t.agent_time_secs,
                    })
                    .collect(),
            };

            let chunk_summary = client.summarize_task_chunk(&context, i, total_chunks)?;

            // Сохраняем в chunk cache
            if let Err(e) = self.cache.lock().unwrap().store_chunk_summary(
                &req.task_id,
                0,
                i,
                &chunk_hash,
                &chunk_summary,
                &self.model,
            ) {
                eprintln!("Failed to cache chunk summary: {}", e);
            }

            summaries.push(chunk_summary.summary);
            pb.inc(1);
        }

        // Layer 1+: combine промежуточных summary
        let mut level = 1;
        while summaries.len() > 1 {
            let combine_chunks: Vec<&[String]> = summaries.chunks(COMBINE_SIZE).collect();
            let total_combine = combine_chunks.len();
            let mut next_summaries: Vec<String> = Vec::with_capacity(total_combine);

            for (i, chunk) in combine_chunks.iter().enumerate() {
                pb.set_message(format!(
                    "task {}, layer {}, chunk {}/{}",
                    req.task_id,
                    level,
                    i + 1,
                    total_combine
                ));

                let chunk_hash = compute_chunk_hash_summaries(chunk);

                // Проверяем chunk cache
                if let Some(cached) = self.cache.lock().unwrap().get_chunk_summary(
                    &req.task_id,
                    level,
                    i,
                    &chunk_hash,
                ) {
                    next_summaries.push(cached.summary);
                    pb.inc(1);
                    continue;
                }

                let combine_ctx = CombineContext {
                    task_id: req.task_id.clone(),
                    project_name: project_name.clone(),
                    first_seen: first_seen.clone(),
                    last_seen: last_seen.clone(),
                    chunk_summaries: chunk.to_vec(),
                    total_turns: req.turns.len(),
                };

                let combined = client.combine_summaries(&combine_ctx)?;

                // Сохраняем в chunk cache
                if let Err(e) = self.cache.lock().unwrap().store_chunk_summary(
                    &req.task_id,
                    level,
                    i,
                    &chunk_hash,
                    &combined,
                    &self.model,
                ) {
                    eprintln!("Failed to cache combine summary: {}", e);
                }

                next_summaries.push(combined.summary.clone());
                pb.inc(1);

                // Если это единственный combine на последнем уровне — возвращаем с status
                if total_combine == 1 {
                    return Ok(combined);
                }
            }

            summaries = next_summaries;
            level += 1;
        }

        // Единственный summary остался — это финальный
        Ok(TaskSummary {
            summary: summaries.into_iter().next().unwrap_or_default(),
            status: Some("in_progress".to_string()),
            title: None,
        })
    }
}

/// Подсчёт общего числа нод (LLM вызовов) для задачи с заданным числом turns
fn count_nodes(turn_count: usize) -> usize {
    if turn_count <= CHUNK_SIZE {
        return 1;
    }

    let mut total = 0;
    let mut count = turn_count.div_ceil(CHUNK_SIZE); // layer 0 chunks
    total += count;

    while count > 1 {
        count = count.div_ceil(COMBINE_SIZE);
        total += count;
    }

    total
}

/// Hash содержимого чанка turns для инвалидации кеша
fn compute_chunk_hash_turns(turns: &[TurnContext]) -> String {
    let mut hasher = DefaultHasher::new();
    for turn in turns {
        turn.timestamp.hash(&mut hasher);
        turn.user_preview.hash(&mut hasher);
        turn.tool_calls.hash(&mut hasher);
        // f64 не реализует Hash, конвертируем в bits
        turn.agent_time_secs.to_bits().hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

/// Hash конкатенации промежуточных summary для инвалидации кеша
fn compute_chunk_hash_summaries(summaries: &[String]) -> String {
    let mut hasher = DefaultHasher::new();
    for s in summaries {
        s.hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_nodes_small() {
        // ≤30 turns: 1 нода
        assert_eq!(count_nodes(1), 1);
        assert_eq!(count_nodes(30), 1);
    }

    #[test]
    fn test_count_nodes_medium() {
        // 95 turns: ceil(95/30) = 4 chunks (layer 0) + 1 combine (layer 1) = 5
        assert_eq!(count_nodes(95), 5);
    }

    #[test]
    fn test_count_nodes_large() {
        // 469 turns:
        // Layer 0: ceil(469/30) = 16 chunks
        // Layer 1: ceil(16/10) = 2 chunks
        // Layer 2: ceil(2/10) = 1 chunk
        // Total: 16 + 2 + 1 = 19
        assert_eq!(count_nodes(469), 19);
    }

    #[test]
    fn test_compute_chunk_hash_turns_deterministic() {
        let turns = vec![TurnContext {
            timestamp: "10:00".to_string(),
            user_preview: Some("hello".to_string()),
            tool_calls: vec!["Read".to_string()],
            agent_time_secs: 5.0,
        }];
        let h1 = compute_chunk_hash_turns(&turns);
        let h2 = compute_chunk_hash_turns(&turns);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_compute_chunk_hash_turns_different() {
        let turns1 = vec![TurnContext {
            timestamp: "10:00".to_string(),
            user_preview: Some("hello".to_string()),
            tool_calls: vec![],
            agent_time_secs: 5.0,
        }];
        let turns2 = vec![TurnContext {
            timestamp: "10:00".to_string(),
            user_preview: Some("world".to_string()),
            tool_calls: vec![],
            agent_time_secs: 5.0,
        }];
        assert_ne!(
            compute_chunk_hash_turns(&turns1),
            compute_chunk_hash_turns(&turns2)
        );
    }

    #[test]
    fn test_compute_chunk_hash_summaries_deterministic() {
        let s = vec!["one".to_string(), "two".to_string()];
        assert_eq!(
            compute_chunk_hash_summaries(&s),
            compute_chunk_hash_summaries(&s)
        );
    }
}
