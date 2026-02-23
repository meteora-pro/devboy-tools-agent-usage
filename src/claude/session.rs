use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use super::models::{AssistantEvent, ClaudeEvent, ContentBlock, TokenUsage};
use super::parser::JsonlFileInfo;
use super::tokens;

/// Событие context compaction внутри сессии
#[derive(Debug, Clone)]
pub struct CompactionEvent {
    pub timestamp: DateTime<Utc>,
    pub trigger: String,
    pub pre_tokens: Option<u64>,
}

/// Собранная сессия Claude Code
#[derive(Debug)]
pub struct ClaudeSession {
    pub session_id: Uuid,
    pub project_name: String,
    pub project_path: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub slug: Option<String>,
    pub turns: Vec<Turn>,
    pub total_usage: AggregatedUsage,
    pub is_subagent: bool,
    /// Context compaction events
    pub compactions: Vec<CompactionEvent>,
}

impl ClaudeSession {
    pub fn duration(&self) -> Duration {
        self.end_time - self.start_time
    }

    pub fn duration_display(&self) -> String {
        let secs = self.duration().num_seconds();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

/// Один "ход" — запрос пользователя + ответ ассистента
#[derive(Debug)]
pub struct Turn {
    pub user_timestamp: DateTime<Utc>,
    pub assistant_timestamp: Option<DateTime<Utc>>,
    /// Длительность хода из system/turn_duration (мс)
    pub turn_duration_ms: Option<u64>,
    pub tool_calls: Vec<String>,
    /// Детали tool calls: (name, short_detail) — путь файла, паттерн, команда и т.д.
    pub tool_call_details: Vec<(String, String)>,
    pub usage: Option<TokenUsage>,
    pub model: Option<String>,
    /// Git branch на момент этого turn (из UserEvent)
    pub git_branch: Option<String>,
    /// Превью первого сообщения пользователя (первые 120 chars)
    pub user_message_preview: Option<String>,
    /// Размер контекста = input_tokens + cache_read_input_tokens
    pub context_tokens: Option<u64>,
}

impl Turn {
    /// Время ожидания ответа (от user до assistant)
    pub fn wait_duration(&self) -> Option<Duration> {
        self.assistant_timestamp.map(|at| at - self.user_timestamp)
    }
}

/// Агрегированный расход токенов
#[derive(Debug, Default, Clone)]
pub struct AggregatedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub estimated_cost_usd: f64,
    pub request_count: u64,
}

impl AggregatedUsage {
    pub fn add(&mut self, usage: &TokenUsage, model: &str) {
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_creation_tokens += usage.cache_creation_input_tokens;
        self.cache_read_tokens += usage.cache_read_input_tokens;
        self.estimated_cost_usd += tokens::calculate_cost(usage, model);
        self.request_count += 1;
    }

    pub fn merge(&mut self, other: &AggregatedUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.estimated_cost_usd += other.estimated_cost_usd;
        self.request_count += other.request_count;
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Построить сессии из распарсенных событий
pub fn build_sessions(parsed_files: Vec<(JsonlFileInfo, Vec<ClaudeEvent>)>) -> Vec<ClaudeSession> {
    // Группируем события по sessionId
    let mut session_map: HashMap<Uuid, SessionBuilder> = HashMap::new();

    for (file_info, events) in parsed_files {
        for event in events {
            // Пропускаем события без sessionId
            let session_id = match event.session_id() {
                Some(id) => id,
                None => continue,
            };

            let builder = session_map
                .entry(session_id)
                .or_insert_with(|| SessionBuilder {
                    session_id,
                    project_name: file_info.project_name.clone(),
                    project_path: file_info.project_path.clone(),
                    is_subagent: file_info.is_subagent,
                    events: Vec::new(),
                });

            // Если хотя бы один файл не subagent — сессия считается основной.
            // Subagent-файлы могут содержать тот же sessionId (compact/task agents),
            // но сессия всё равно принадлежит пользователю.
            if !file_info.is_subagent {
                builder.is_subagent = false;
            }

            builder.events.push((event, file_info.is_subagent));
        }
    }

    // Строим сессии из сгруппированных событий
    let mut sessions: Vec<ClaudeSession> = session_map
        .into_values()
        .filter_map(|builder| builder.build())
        .collect();

    // Сортируем по времени начала
    sessions.sort_by_key(|s| s.start_time);
    sessions
}

struct SessionBuilder {
    session_id: Uuid,
    project_name: String,
    project_path: String,
    is_subagent: bool,
    /// События с пометкой: (event, from_subagent_file)
    events: Vec<(ClaudeEvent, bool)>,
}

impl SessionBuilder {
    fn build(mut self) -> Option<ClaudeSession> {
        if self.events.is_empty() {
            return None;
        }

        // Сортируем по timestamp
        self.events.sort_by_key(|(e, _)| e.timestamp());

        let mut turns = Vec::new();
        let mut total_usage = AggregatedUsage::default();
        let mut git_branch: Option<String> = None;
        let mut version: Option<String> = None;
        let mut slug: Option<String> = None;
        let mut min_time: Option<DateTime<Utc>> = None;
        let mut max_time: Option<DateTime<Utc>> = None;

        // Собираем turn_duration из system событий
        let mut turn_durations: HashMap<Option<Uuid>, u64> = HashMap::new();
        // Собираем git_branch per user event UUID
        let mut user_branches: HashMap<Uuid, Option<String>> = HashMap::new();
        // Собираем message preview per user event UUID
        let mut user_previews: HashMap<Uuid, Option<String>> = HashMap::new();
        // Собираем compaction events
        let mut compactions: Vec<CompactionEvent> = Vec::new();

        // Первый проход: собираем метаданные и turn_duration
        // (используем ВСЕ события для метаданных, включая subagent)
        for (event, _from_subagent) in &self.events {
            if let Some(ts) = event.timestamp() {
                min_time = Some(min_time.map_or(ts, |m: DateTime<Utc>| m.min(ts)));
                max_time = Some(max_time.map_or(ts, |m: DateTime<Utc>| m.max(ts)));
            }

            match event {
                ClaudeEvent::User(e) => {
                    // Предпочитаем не-main branch для session-level git_branch
                    if let Some(ref b) = e.base.git_branch {
                        if !is_non_task_branch(b) || git_branch.is_none() {
                            git_branch = Some(b.clone());
                        }
                    }
                    if version.is_none() {
                        version = e.base.version.clone();
                    }
                    if slug.is_none() {
                        slug = e.base.slug.clone();
                    }
                    // Сохраняем branch для каждого user event
                    user_branches.insert(e.base.uuid, e.base.git_branch.clone());
                    // Извлекаем и сохраняем превью сообщения
                    let preview = e
                        .message
                        .as_ref()
                        .and_then(|msg| extract_message_preview(&msg.content, 120));
                    user_previews.insert(e.base.uuid, preview);
                }
                ClaudeEvent::System(e) => {
                    if e.subtype.as_deref() == Some("turn_duration") {
                        if let Some(ms) = e.duration_ms {
                            turn_durations.insert(e.base.parent_uuid, ms);
                        }
                    }
                    // Парсим compact_boundary events
                    if e.subtype.as_deref() == Some("compact_boundary") {
                        let trigger = e
                            .compact_metadata
                            .as_ref()
                            .and_then(|m| m.trigger.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        let pre_tokens = e.compact_metadata.as_ref().and_then(|m| m.pre_tokens);
                        compactions.push(CompactionEvent {
                            timestamp: e.base.timestamp,
                            trigger,
                            pre_tokens,
                        });
                    }
                }
                _ => {}
            }
        }

        // Считаем стоимость из ВСЕХ assistant событий (включая subagent)
        for (event, _) in &self.events {
            if let ClaudeEvent::Assistant(e) = event {
                if let Some(ref u) = e.message.usage {
                    total_usage.add(u, e.message.model.as_deref().unwrap_or("unknown"));
                }
            }
        }

        // Второй проход: строим Turn ТОЛЬКО из основных (не subagent) событий.
        // Subagent события перемешиваются по timestamp и ломают пару user→assistant.
        let mut pending_user_ts: Option<DateTime<Utc>> = None;
        let mut pending_user_uuid: Option<Uuid> = None;

        for (event, from_subagent) in &self.events {
            // Пропускаем subagent-события при построении turns
            if *from_subagent {
                continue;
            }

            match event {
                ClaudeEvent::User(e) => {
                    // Пропускаем internal/tool_result сообщения
                    if e.user_type.as_deref() == Some("internal") {
                        continue;
                    }
                    // Если есть необработанный user без assistant — добавляем turn без ответа
                    if let Some(user_ts) = pending_user_ts {
                        let branch = pending_user_uuid
                            .and_then(|uuid| user_branches.get(&uuid).cloned().flatten());
                        let preview = pending_user_uuid
                            .and_then(|uuid| user_previews.get(&uuid).cloned().flatten());
                        turns.push(Turn {
                            user_timestamp: user_ts,
                            assistant_timestamp: None,
                            turn_duration_ms: None,
                            tool_calls: Vec::new(),
                            tool_call_details: Vec::new(),
                            usage: None,
                            model: None,
                            git_branch: branch,
                            user_message_preview: preview,
                            context_tokens: None,
                        });
                    }
                    pending_user_ts = Some(e.base.timestamp);
                    pending_user_uuid = Some(e.base.uuid);
                }
                ClaudeEvent::Assistant(e) => {
                    let tool_calls = extract_tool_calls(e);
                    let tool_call_details = extract_tool_call_details(e);

                    if pending_user_ts.is_some() {
                        // Нормальный turn: user → assistant
                        let user_ts = pending_user_ts.unwrap();
                        let model = e.message.model.clone();
                        let usage = e.message.usage.clone();

                        let turn_duration_ms = pending_user_uuid
                            .and_then(|uuid| turn_durations.get(&Some(uuid)).copied());

                        let branch = pending_user_uuid
                            .and_then(|uuid| user_branches.get(&uuid).cloned().flatten());
                        let preview = pending_user_uuid
                            .and_then(|uuid| user_previews.get(&uuid).cloned().flatten());

                        // context_tokens = input_tokens + cache_read_input_tokens
                        let context_tokens = usage
                            .as_ref()
                            .map(|u| u.input_tokens + u.cache_read_input_tokens);

                        turns.push(Turn {
                            user_timestamp: user_ts,
                            assistant_timestamp: Some(e.base.timestamp),
                            turn_duration_ms,
                            tool_calls,
                            tool_call_details,
                            usage,
                            model,
                            git_branch: branch,
                            user_message_preview: preview,
                            context_tokens,
                        });

                        pending_user_ts = None;
                        pending_user_uuid = None;
                    } else if let Some(last_turn) = turns.last_mut() {
                        // Orphan assistant: продолжение предыдущего turn (tool use chain).
                        // Мержим в последний turn: обновляем timestamp, добавляем tool calls и usage.
                        last_turn.assistant_timestamp = Some(e.base.timestamp);
                        last_turn.tool_calls.extend(tool_calls);
                        last_turn.tool_call_details.extend(tool_call_details);
                        if let Some(ref u) = e.message.usage {
                            match &mut last_turn.usage {
                                Some(existing) => {
                                    existing.input_tokens += u.input_tokens;
                                    existing.output_tokens += u.output_tokens;
                                    existing.cache_creation_input_tokens +=
                                        u.cache_creation_input_tokens;
                                    existing.cache_read_input_tokens += u.cache_read_input_tokens;
                                }
                                None => {
                                    last_turn.usage = Some(u.clone());
                                }
                            }
                        }
                    }
                    // Если нет pending user И нет предыдущих turns — пропускаем
                }
                _ => {}
            }
        }

        let start_time = min_time?;
        let end_time = max_time.unwrap_or(start_time);

        // Сортируем compaction events по timestamp
        compactions.sort_by_key(|c| c.timestamp);

        Some(ClaudeSession {
            session_id: self.session_id,
            project_name: self.project_name,
            project_path: self.project_path,
            start_time,
            end_time,
            git_branch,
            version,
            slug,
            turns,
            total_usage,
            is_subagent: self.is_subagent,
            compactions,
        })
    }
}

/// Извлечь превью из user message content (string или array of content blocks)
///
/// Фильтрует служебные теги (`<local-command-caveat>`, `<command-name>`, `[Request interrupted]`)
/// и обрезает до max_len символов (безопасно для UTF-8)
pub fn extract_message_preview(content: &serde_json::Value, max_len: usize) -> Option<String> {
    let raw_text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // Собираем текст из всех text блоков
            let texts: Vec<&str> = arr
                .iter()
                .filter_map(|block| {
                    if block.get("type")?.as_str()? == "text" {
                        block.get("text")?.as_str()
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                return None;
            }
            texts.join(" ")
        }
        _ => return None,
    };

    // Фильтруем служебные теги и контент
    let cleaned = clean_message_text(&raw_text);
    if cleaned.is_empty() {
        return None;
    }

    // Truncate по символам (безопасно для UTF-8)
    let truncated: String = cleaned.chars().take(max_len).collect();
    let truncated = if cleaned.chars().count() > max_len {
        format!("{}...", truncated)
    } else {
        truncated
    };

    Some(truncated)
}

/// Очистить текст от служебных тегов и нормализовать пробелы
fn clean_message_text(text: &str) -> String {
    let mut result = text.to_string();

    // Удаляем XML-подобные служебные теги с содержимым
    let tag_patterns = [
        "local-command-caveat",
        "command-name",
        "command-message",
        "command-args",
        "system-reminder",
        "user-prompt-submit-hook",
    ];
    for tag in &tag_patterns {
        // Удаляем <tag>...</tag> конструкции
        let open = format!("<{}", tag);
        while let Some(start) = result.find(&open) {
            let close_tag = format!("</{}>", tag);
            if let Some(end) = result.find(&close_tag) {
                result = format!("{}{}", &result[..start], &result[end + close_tag.len()..]);
            } else {
                // Незакрытый тег — удаляем до конца
                result = result[..start].to_string();
                break;
            }
        }
    }

    // Удаляем "[Request interrupted]" и подобные маркеры
    result = result.replace("[Request interrupted]", "");

    // Нормализуем пробелы
    result = result.split_whitespace().collect::<Vec<_>>().join(" ");
    result = result.trim().to_string();

    result
}

/// Проверка: это branch без привязки к задаче?
pub fn is_non_task_branch(branch: &str) -> bool {
    matches!(
        branch.to_lowercase().as_str(),
        "main" | "master" | "head" | "develop" | "dev" | "staging" | "production" | "release"
    )
}

/// Извлечь имена tool calls из assistant event
fn extract_tool_calls(event: &AssistantEvent) -> Vec<String> {
    event
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Извлечь детали tool calls (name + detail) из assistant event
fn extract_tool_call_details(event: &AssistantEvent) -> Vec<(String, String)> {
    event
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => {
                let detail = extract_tool_detail(name, input);
                Some((name.clone(), detail))
            }
            _ => None,
        })
        .collect()
}

/// Извлечь краткое описание из input JSON для разных типов tool calls
fn extract_tool_detail(name: &str, input: &serde_json::Value) -> String {
    match name {
        // Файловые операции — показываем путь
        "Read" | "Write" => get_short_path(input, "file_path"),
        "Edit" => get_short_path(input, "file_path"),
        "NotebookEdit" => get_short_path(input, "notebook_path"),

        // Поиск — показываем паттерн
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .map(shorten_path)
                .unwrap_or_default();
            if path.is_empty() {
                pattern.to_string()
            } else {
                format!("{} in {}", pattern, path)
            }
        }

        // Bash — показываем команду (обрезанную)
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            // Берём первую строку, обрезаем до 80 символов
            let first_line = cmd.lines().next().unwrap_or(cmd);
            let truncated: String = first_line.chars().take(80).collect();
            if first_line.chars().count() > 80 {
                format!("{}...", truncated)
            } else {
                truncated
            }
        }

        // Task tools
        "TaskCreate" => input
            .get("subject")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 60))
            .unwrap_or_default(),
        "TaskUpdate" => {
            let id = input.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status.is_empty() {
                format!("#{}", id)
            } else {
                format!("#{} → {}", id, status)
            }
        }
        "TaskOutput" | "TaskGet" => input
            .get("task_id")
            .or_else(|| input.get("taskId"))
            .and_then(|v| v.as_str())
            .map(|s| format!("#{}", s))
            .unwrap_or_default(),

        // Web
        "WebFetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 60))
            .unwrap_or_default(),
        "WebSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 60))
            .unwrap_or_default(),

        // Task (subagent)
        "Task" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, 60))
            .unwrap_or_default(),

        // MCP tools — пытаемся извлечь ключевые параметры
        _ if name.starts_with("mcp__") => extract_mcp_detail(input),

        _ => String::new(),
    }
}

/// Извлечь краткий path из input JSON, сократив до basename или последних компонентов
fn get_short_path(input: &serde_json::Value, key: &str) -> String {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(shorten_path)
        .unwrap_or_default()
}

/// Сократить путь файла — оставляем последние 2-3 компонента
fn shorten_path(path: &str) -> String {
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if components.len() <= 3 {
        return path.to_string();
    }
    // Оставляем последние 3 компонента
    format!(".../{}", components[components.len() - 3..].join("/"))
}

/// Обрезать строку до max_len символов
fn truncate_str(s: &str, max_len: usize) -> String {
    let chars: String = s.chars().take(max_len).collect();
    if s.chars().count() > max_len {
        format!("{}...", chars)
    } else {
        chars
    }
}

/// Извлечь деталь из MCP tool input — берём первое строковое значение из параметров
fn extract_mcp_detail(input: &serde_json::Value) -> String {
    if let Some(obj) = input.as_object() {
        // Приоритетные ключи для MCP
        for key in &["issueKey", "mrKey", "query", "chatKey", "body", "title"] {
            if let Some(val) = obj.get(*key).and_then(|v| v.as_str()) {
                return truncate_str(val, 50);
            }
        }
        // Fallback: первое строковое значение
        for (_k, v) in obj {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return truncate_str(s, 50);
                }
            }
        }
    }
    String::new()
}
