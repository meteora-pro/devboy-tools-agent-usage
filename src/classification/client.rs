use anyhow::{Context, Result};

use super::config::{LlmConfig, LlmProvider};

/// Элемент для классификации
pub struct ClassifyItem {
    pub message_preview: String,
    pub git_branch: Option<String>,
    pub project_name: String,
}

/// Контекст одного turn для суммаризации задачи
pub struct TurnContext {
    pub timestamp: String,
    pub user_preview: Option<String>,
    pub tool_calls: Vec<String>,
    pub agent_time_secs: f64,
}

/// Контекст задачи для суммаризации
pub struct TaskContext {
    pub task_id: String,
    pub project_name: String,
    pub first_seen: String,
    pub last_seen: String,
    pub turns: Vec<TurnContext>,
}

/// Результат суммаризации задачи
#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub summary: String,
    pub status: Option<String>,
    pub title: Option<String>,
}

/// Контекст для combine-уровня иерархической суммаризации
pub struct CombineContext {
    pub task_id: String,
    pub project_name: String,
    pub first_seen: String,
    pub last_seen: String,
    pub chunk_summaries: Vec<String>,
    pub total_turns: usize,
}

/// Статистика LLM вызовов (для отчёта пользователю)
#[derive(Debug, Default, Clone)]
pub struct LlmUsageStats {
    /// Количество API вызовов
    pub request_count: usize,
    /// Суммарные input tokens
    pub input_tokens: u64,
    /// Суммарные output tokens
    pub output_tokens: u64,
}

/// HTTP клиент для LLM API (Anthropic / OpenAI-compatible)
pub struct LlmClient {
    agent: ureq::Agent,
    provider: LlmProvider,
    api_url: String,
    api_key: Option<String>,
    model: String,
    /// Аккумулятор usage за текущий запуск (thread-safe для rayon)
    usage: std::sync::Mutex<LlmUsageStats>,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let agent = ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(config.timeout_secs)))
            .build()
            .new_agent();
        LlmClient {
            agent,
            provider: config.provider,
            api_url: config.api_url,
            api_key: config.api_key,
            model: config.model,
            usage: std::sync::Mutex::new(LlmUsageStats::default()),
        }
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Получить накопленную статистику LLM вызовов
    pub fn usage_stats(&self) -> LlmUsageStats {
        self.usage.lock().unwrap().clone()
    }

    /// Записать usage из ответа API
    fn track_usage(&self, response_json: &serde_json::Value) {
        let mut stats = self.usage.lock().unwrap();
        stats.request_count += 1;

        // Anthropic: usage.input_tokens / usage.output_tokens
        // OpenAI: usage.prompt_tokens / usage.completion_tokens
        if let Some(usage) = response_json.get("usage") {
            if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                stats.input_tokens += input;
            }
            if let Some(input) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
                stats.input_tokens += input;
            }
            if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                stats.output_tokens += output;
            }
            if let Some(output) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
                stats.output_tokens += output;
            }
        }
    }

    /// Классифицировать батч сообщений
    ///
    /// Возвращает Vec<String> — activity labels в том же порядке что и items
    pub fn classify_batch(&self, items: &[ClassifyItem]) -> Result<Vec<String>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }

        let prompt = build_classify_prompt(items);
        let text = self.call_llm(&prompt, 1024)?;
        parse_labels(&text, items.len()).with_context(|| {
            format!(
                "classify_batch failed for {} items. LLM raw response (first 500 chars): {}",
                items.len(),
                &text[..text.len().min(500)]
            )
        })
    }

    /// Суммаризировать диалог по задаче
    pub fn summarize_task(&self, context: &TaskContext) -> Result<TaskSummary> {
        let prompt = build_summarize_prompt(context);
        let text = self.call_llm(&prompt, 512)?;
        parse_summary(&text)
    }

    /// Суммаризировать один чанк turns (layer 0)
    pub fn summarize_task_chunk(
        &self,
        context: &TaskContext,
        chunk_index: usize,
        total_chunks: usize,
    ) -> Result<TaskSummary> {
        let prompt = build_summarize_chunk_prompt(context, chunk_index, total_chunks);
        let text = self.call_llm(&prompt, 512)?;
        parse_summary(&text)
    }

    /// Объединить промежуточные суммаризации (layer 1+)
    pub fn combine_summaries(&self, context: &CombineContext) -> Result<TaskSummary> {
        let prompt = build_combine_prompt(context);
        let text = self.call_llm(&prompt, 512)?;
        parse_summary(&text)
    }

    /// Единая точка входа для LLM вызовов — диспатч по provider
    fn call_llm(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        match self.provider {
            LlmProvider::Anthropic => self.call_anthropic(prompt, max_tokens),
            LlmProvider::OpenAiCompatible => self.call_openai_compat(prompt, max_tokens),
        }
    }

    /// Вызов Anthropic Messages API
    fn call_anthropic(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let api_key = self
            .api_key
            .as_deref()
            .context("Anthropic API key is required")?;

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": prompt}]
        });

        let response = self
            .agent
            .post(&self.api_url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .send_json(&body)
            .context("Anthropic API request failed")?;

        let response_json: serde_json::Value = response
            .into_body()
            .read_json()
            .context("Failed to parse Anthropic response")?;

        self.track_usage(&response_json);

        // Извлекаем текст из response.content[0].text
        response_json
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|block| block.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .context("Unexpected Anthropic response format")
    }

    /// Вызов OpenAI-compatible API (Ollama, LM Studio, vLLM)
    fn call_openai_compat(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": [{"role": "user", "content": prompt}]
        });

        let mut request = self
            .agent
            .post(&self.api_url)
            .header("content-type", "application/json");

        // Bearer auth если ключ задан (опционально для Ollama)
        if let Some(ref key) = self.api_key {
            request = request.header("authorization", &format!("Bearer {}", key));
        }

        let response = request
            .send_json(&body)
            .context("OpenAI-compatible API request failed")?;

        let response_json: serde_json::Value = response
            .into_body()
            .read_json()
            .context("Failed to parse OpenAI-compatible response")?;

        self.track_usage(&response_json);

        // Извлекаем текст из response.choices[0].message.content
        response_json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|msg| msg.get("content"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .context("Unexpected OpenAI-compatible response format")
    }
}

/// Prompt для классификации батча (русский)
fn build_classify_prompt(items: &[ClassifyItem]) -> String {
    let mut messages_list = String::new();
    for (i, item) in items.iter().enumerate() {
        let branch_info = item
            .git_branch
            .as_deref()
            .map(|b| format!(", branch: {}", b))
            .unwrap_or_default();
        messages_list.push_str(&format!(
            "{}. [project: {}{}] \"{}\"\n",
            i + 1,
            item.project_name,
            branch_info,
            item.message_preview
        ));
    }

    format!(
        r#"Ты классификатор рабочих активностей. Классифицируй каждое сообщение в короткую метку (2-5 слов НА РУССКОМ). Если есть task ID — включи его.

Правила:
- Если сообщение о конкретной задаче — назови задачу кратко
- Если это обсуждение встречи/стендапа — "анализ встречи" или "обзор стендапа"
- Если это code review — "ревью кода"
- Если это планирование — "планирование задач"
- Если это дебаг — "дебаг [тема]"
- Если это реализация — "реализация [фича]"
- Будь конкретным но кратким

Сообщения:
{}
Ответь ТОЛЬКО JSON массивом меток, по одной на сообщение:
["метка1", "метка2", ...]"#,
        messages_list
    )
}

/// Prompt для суммаризации задачи (русский)
fn build_summarize_prompt(ctx: &TaskContext) -> String {
    let mut timeline = String::new();
    for (i, turn) in ctx.turns.iter().enumerate() {
        let user_text = turn.user_preview.as_deref().unwrap_or("[нет превью]");
        let tools = if turn.tool_calls.is_empty() {
            "-".to_string()
        } else {
            // Группируем одинаковые tool calls: Read(3), Edit(2)
            let mut tool_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for tc in &turn.tool_calls {
                *tool_counts.entry(tc.as_str()).or_default() += 1;
            }
            let mut parts: Vec<String> = tool_counts
                .into_iter()
                .map(|(name, count)| {
                    if count > 1 {
                        format!("{}({})", name, count)
                    } else {
                        name.to_string()
                    }
                })
                .collect();
            parts.sort();
            parts.join(", ")
        };
        let time_str = format!("{:.0}s", turn.agent_time_secs);
        timeline.push_str(&format!(
            "{}. [{}] User: \"{}\"\n   Agent: {} -> {}\n",
            i + 1,
            turn.timestamp,
            user_text,
            tools,
            time_str,
        ));
    }

    format!(
        r#"Проанализируй диалог разработчика с AI-ассистентом и напиши краткое описание на русском.

Задача: {task_id}
Проект: {project}
Период: {first_seen} — {last_seen}
Turns: {turn_count}

Хронология диалога:
{timeline}
Ответь ТОЛЬКО JSON (без markdown):
{{"title": "краткий заголовок задачи НА РУССКОМ, 3-7 слов", "summary": "1-3 предложения: что просил человек, что сделал агент, на чём остановились", "status": "completed | in_progress | blocked"}}"#,
        task_id = ctx.task_id,
        project = ctx.project_name,
        first_seen = ctx.first_seen,
        last_seen = ctx.last_seen,
        turn_count = ctx.turns.len(),
        timeline = timeline,
    )
}

/// Prompt для суммаризации одного чанка (часть X из Y)
fn build_summarize_chunk_prompt(
    ctx: &TaskContext,
    chunk_index: usize,
    total_chunks: usize,
) -> String {
    let mut timeline = String::new();
    for (i, turn) in ctx.turns.iter().enumerate() {
        let user_text = turn.user_preview.as_deref().unwrap_or("[нет превью]");
        let tools = if turn.tool_calls.is_empty() {
            "-".to_string()
        } else {
            let mut tool_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for tc in &turn.tool_calls {
                *tool_counts.entry(tc.as_str()).or_default() += 1;
            }
            let mut parts: Vec<String> = tool_counts
                .into_iter()
                .map(|(name, count)| {
                    if count > 1 {
                        format!("{}({})", name, count)
                    } else {
                        name.to_string()
                    }
                })
                .collect();
            parts.sort();
            parts.join(", ")
        };
        let time_str = format!("{:.0}s", turn.agent_time_secs);
        timeline.push_str(&format!(
            "{}. [{}] User: \"{}\"\n   Agent: {} -> {}\n",
            i + 1,
            turn.timestamp,
            user_text,
            tools,
            time_str,
        ));
    }

    format!(
        r#"Проанализируй ЧАСТЬ {chunk_num} из {total_chunks} диалога разработчика с AI-ассистентом.

Задача: {task_id}
Проект: {project}
Период: {first_seen} — {last_seen}

Хронология (часть {chunk_num}/{total_chunks}):
{timeline}
Это промежуточная суммаризация. Опиши основные действия и решения в этой части диалога.
Ответь ТОЛЬКО JSON (без markdown):
{{"title": "краткий заголовок задачи НА РУССКОМ, 3-7 слов", "summary": "1-3 предложения: ключевые действия в этой части", "status": "in_progress | completed | blocked"}}"#,
        chunk_num = chunk_index + 1,
        total_chunks = total_chunks,
        task_id = ctx.task_id,
        project = ctx.project_name,
        first_seen = ctx.first_seen,
        last_seen = ctx.last_seen,
        timeline = timeline,
    )
}

/// Prompt для объединения промежуточных суммаризаций
fn build_combine_prompt(ctx: &CombineContext) -> String {
    let mut parts_list = String::new();
    for (i, summary) in ctx.chunk_summaries.iter().enumerate() {
        parts_list.push_str(&format!("Часть {}: {}\n", i + 1, summary));
    }

    format!(
        r#"Объедини промежуточные описания диалога разработчика с AI-ассистентом в одно итоговое описание.

Задача: {task_id}
Проект: {project}
Период: {first_seen} — {last_seen}
Всего turns: {total_turns}

Промежуточные описания:
{parts_list}
Напиши единое краткое описание всего диалога и определи итоговый статус.
Ответь ТОЛЬКО JSON (без markdown):
{{"title": "краткий заголовок задачи НА РУССКОМ, 3-7 слов", "summary": "1-3 предложения: что просил человек, что сделал агент, на чём остановились", "status": "completed | in_progress | blocked"}}"#,
        task_id = ctx.task_id,
        project = ctx.project_name,
        first_seen = ctx.first_seen,
        last_seen = ctx.last_seen,
        total_turns = ctx.total_turns,
        parts_list = parts_list,
    )
}

/// Парсинг JSON array of labels из ответа LLM
///
/// Устойчив к типичным проблемам LLM:
/// - markdown code fences (```json ... ```)
/// - null вместо строк: ["label", null, "label"]
/// - trailing commas: ["a", "b",]
/// - обрезанный ответ (max_tokens): ["a", "b"  (без закрывающей ])
/// - объекты вместо строк: [{"label": "foo"}]
/// - single quotes: ['a', 'b']
fn parse_labels(text: &str, expected_count: usize) -> Result<Vec<String>> {
    let trimmed = text.trim();

    // 1. Убираем markdown code fences если есть
    let cleaned = strip_markdown_fences(trimmed);

    // 2. Находим JSON array (если нет ] — ответ обрезан, добавляем)
    let json_start = cleaned
        .find('[')
        .context("No JSON array found in LLM response")?;
    let json_str = match cleaned.rfind(']') {
        Some(json_end) => cleaned[json_start..=json_end].to_string(),
        None => {
            // Ответ обрезан по max_tokens — пробуем починить
            let partial = cleaned[json_start..].trim_end();
            // Убираем незавершённую строку/запятую в конце и закрываем массив
            let trimmed_partial = partial.trim_end_matches([',', ' ', '\n', '"']);
            // Если последний символ — не кавычка и не null, обрезаем до последнего полного элемента
            let last_comma = trimmed_partial.rfind(',').unwrap_or(trimmed_partial.len());
            let safe_part = &trimmed_partial[..last_comma];
            format!("{}]", safe_part)
        }
    };

    // 3. Применяем fix (trailing commas, single quotes)
    let fixed = fix_json_array(&json_str);

    // 4. Парсим как Vec<Value> — универсально обрабатывает null, string, object
    let labels: Vec<String> = match serde_json::from_str::<Vec<serde_json::Value>>(&fixed) {
        Ok(values) => values
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => "unclassified".to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Object(obj) => obj
                    .values()
                    .find_map(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unclassified".to_string()),
                other => other.to_string(),
            })
            .collect(),
        Err(_) => {
            // 5. Последний fallback: regex-extraction строк в кавычках
            let fallback = extract_quoted_strings(&json_str);
            if fallback.is_empty() {
                anyhow::bail!(
                    "Failed to parse labels. Raw: {}",
                    &trimmed[..trimmed.len().min(300)]
                );
            }
            fallback
        }
    };

    // 6. Санитизируем: "null", пустые строки → "unclassified"
    let labels: Vec<String> = labels
        .into_iter()
        .map(|l| {
            let t = l.trim().to_string();
            if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "N/A" {
                "unclassified".to_string()
            } else {
                t
            }
        })
        .collect();

    // 7. Дополняем/обрезаем до expected_count
    let mut result = labels;
    while result.len() < expected_count {
        result.push("unclassified".to_string());
    }
    result.truncate(expected_count);

    Ok(result)
}

/// Убрать markdown code fences (```json ... ``` или ``` ... ```)
fn strip_markdown_fences(text: &str) -> String {
    let mut s = text.to_string();
    // Убираем открывающий fence с optional language tag
    if let Some(start) = s.find("```") {
        let after_fence = &s[start + 3..];
        // Пропускаем optional language tag до конца строки
        if let Some(newline) = after_fence.find('\n') {
            let before = &s[..start];
            let content = &after_fence[newline + 1..];
            s = format!("{}{}", before, content);
        }
    }
    // Убираем закрывающий fence
    if let Some(end) = s.rfind("```") {
        s = s[..end].to_string();
    }
    s
}

/// Исправить типичные JSON проблемы: trailing commas, single quotes
fn fix_json_array(json_str: &str) -> String {
    let mut s = json_str.to_string();

    // Trailing comma перед закрывающей скобкой: ,] → ]
    // Учитываем пробелы/переносы между , и ]
    if let Some(pos) = s.rfind(',') {
        let after = s[pos + 1..].trim_start();
        if after.starts_with(']') {
            s = format!("{}{}", &s[..pos], &s[pos + 1..].replacen(',', "", 0));
            // Удаляем запятую
            s.remove(pos);
            s.insert(pos, ' ');
        }
    }

    // Single quotes → double quotes (грубо, но для простых массивов работает)
    if !s.contains('"') && s.contains('\'') {
        s = s.replace('\'', "\"");
    }

    s
}

/// Fallback: извлечь строки в кавычках из текста
fn extract_quoted_strings(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::new();
            loop {
                match chars.next() {
                    Some('"') | None => break,
                    Some('\\') => {
                        if let Some(escaped) = chars.next() {
                            s.push(escaped);
                        }
                    }
                    Some(ch) => s.push(ch),
                }
            }
            if !s.is_empty() {
                result.push(s);
            }
        }
    }
    result
}

/// Парсинг JSON summary из ответа LLM
fn parse_summary(text: &str) -> Result<TaskSummary> {
    let trimmed = text.trim();

    // Убираем markdown code fences
    let cleaned = strip_markdown_fences(trimmed);

    // Ищем JSON object в ответе
    let json_start = cleaned
        .find('{')
        .context("No JSON object found in LLM summary response")?;
    let json_end = cleaned
        .rfind('}')
        .context("No closing brace in LLM summary response")?;
    let json_str = &cleaned[json_start..=json_end];

    let parsed: serde_json::Value = serde_json::from_str(json_str).with_context(|| {
        format!(
            "Failed to parse summary JSON. Raw: {}",
            &json_str[..json_str.len().min(300)]
        )
    })?;

    let summary = parsed
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("(не удалось получить описание)")
        .to_string();

    let status = parsed
        .get("status")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok(TaskSummary {
        summary,
        status,
        title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_labels() {
        let text = r#"["meeting analysis", "code review", "task planning"]"#;
        let labels = parse_labels(text, 3).unwrap();
        assert_eq!(
            labels,
            vec!["meeting analysis", "code review", "task planning"]
        );
    }

    #[test]
    fn test_parse_labels_with_extra_text() {
        let text = r#"Here are the labels:
["meeting analysis", "debugging auth"]
Hope this helps!"#;
        let labels = parse_labels(text, 2).unwrap();
        assert_eq!(labels, vec!["meeting analysis", "debugging auth"]);
    }

    #[test]
    fn test_parse_labels_fewer_than_expected() {
        let text = r#"["only one"]"#;
        let labels = parse_labels(text, 3).unwrap();
        assert_eq!(labels, vec!["only one", "unclassified", "unclassified"]);
    }

    #[test]
    fn test_build_classify_prompt() {
        let items = vec![ClassifyItem {
            message_preview: "implement the login page".to_string(),
            git_branch: Some("feat/DEV-123".to_string()),
            project_name: "my-app".to_string(),
        }];
        let prompt = build_classify_prompt(&items);
        assert!(prompt.contains("[project: my-app, branch: feat/DEV-123]"));
        assert!(prompt.contains("implement the login page"));
    }

    #[test]
    fn test_parse_summary() {
        let text = r#"{"title": "Фильтрация Jira проектов", "summary": "Настроили фильтрацию Jira проектов.", "status": "completed"}"#;
        let result = parse_summary(text).unwrap();
        assert_eq!(result.summary, "Настроили фильтрацию Jira проектов.");
        assert_eq!(result.status, Some("completed".to_string()));
        assert_eq!(result.title, Some("Фильтрация Jira проектов".to_string()));
    }

    #[test]
    fn test_parse_summary_with_extra_text() {
        let text = r#"Here is the summary:
{"summary": "Интеграция Langfuse.", "status": "in_progress"}
Done!"#;
        let result = parse_summary(text).unwrap();
        assert_eq!(result.summary, "Интеграция Langfuse.");
        assert_eq!(result.status, Some("in_progress".to_string()));
        assert_eq!(result.title, None);
    }

    #[test]
    fn test_parse_summary_no_status() {
        let text = r#"{"summary": "Работа над фичей"}"#;
        let result = parse_summary(text).unwrap();
        assert_eq!(result.summary, "Работа над фичей");
        assert_eq!(result.status, None);
        assert_eq!(result.title, None);
    }

    #[test]
    fn test_parse_labels_markdown_fences() {
        let text = "```json\n[\"метка один\", \"ревью кода\"]\n```";
        let labels = parse_labels(text, 2).unwrap();
        assert_eq!(labels, vec!["метка один", "ревью кода"]);
    }

    #[test]
    fn test_parse_labels_trailing_comma() {
        let text = r#"["label1", "label2",]"#;
        let labels = parse_labels(text, 2).unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0], "label1");
    }

    #[test]
    fn test_parse_labels_single_quotes() {
        let text = "['метка один', 'ревью кода']";
        let labels = parse_labels(text, 2).unwrap();
        assert_eq!(labels, vec!["метка один", "ревью кода"]);
    }

    #[test]
    fn test_parse_labels_objects_instead_of_strings() {
        let text = r#"[{"label": "анализ кода"}, {"label": "ревью"}]"#;
        let labels = parse_labels(text, 2).unwrap();
        assert_eq!(labels, vec!["анализ кода", "ревью"]);
    }

    #[test]
    fn test_parse_labels_with_nulls() {
        let text = r#"["проверка деплоя", null, "дебаг подов", null]"#;
        let labels = parse_labels(text, 4).unwrap();
        assert_eq!(
            labels,
            vec![
                "проверка деплоя",
                "unclassified",
                "дебаг подов",
                "unclassified"
            ]
        );
    }

    #[test]
    fn test_parse_labels_truncated_response() {
        // Ответ обрезан по max_tokens — нет закрывающей ]
        let text = r#"```json
["проверка деплоя", "дебаг интерфейса", "выполнение задачи""#;
        let labels = parse_labels(text, 3).unwrap();
        assert_eq!(labels[0], "проверка деплоя");
        assert_eq!(labels[1], "дебаг интерфейса");
        // Третий может быть "выполнение задачи" или "unclassified" в зависимости от обрезки
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn test_parse_labels_nulls_in_markdown_fences() {
        let text = "```json\n[\"label1\", null, null, \"label2\"]\n```";
        let labels = parse_labels(text, 4).unwrap();
        assert_eq!(labels[0], "label1");
        assert_eq!(labels[1], "unclassified");
        assert_eq!(labels[2], "unclassified");
        assert_eq!(labels[3], "label2");
    }

    #[test]
    fn test_parse_summary_markdown_fences() {
        let text = "```json\n{\"title\": \"Тест\", \"summary\": \"Описание.\", \"status\": \"completed\"}\n```";
        let result = parse_summary(text).unwrap();
        assert_eq!(result.title, Some("Тест".to_string()));
        assert_eq!(result.summary, "Описание.");
    }

    #[test]
    fn test_strip_markdown_fences() {
        assert_eq!(strip_markdown_fences("```json\n[\"a\"]\n```"), "[\"a\"]\n");
        assert_eq!(
            strip_markdown_fences("```\n{\"a\": 1}\n```"),
            "{\"a\": 1}\n"
        );
        assert_eq!(
            strip_markdown_fences("[\"no fences\"]"),
            "[\"no fences\"]".to_string()
        );
    }

    #[test]
    fn test_extract_quoted_strings() {
        let text = r#"["hello", "world"]"#;
        assert_eq!(extract_quoted_strings(text), vec!["hello", "world"]);
    }

    #[test]
    fn test_extract_quoted_strings_escaped() {
        let text = r#"["he said \"hi\"", "ok"]"#;
        assert_eq!(extract_quoted_strings(text), vec!["he said \"hi\"", "ok"]);
    }
}
