use chrono::{DateTime, Utc};

use crate::activity::models::{AppCategory, BrowserCategory};
use crate::claude::session::ClaudeSession;

/// Результат корреляции сессии с ActivityWatch данными
#[derive(Debug)]
pub struct CorrelatedSession {
    pub session: ClaudeSession,
    pub focus_periods: Vec<FocusPeriod>,
    pub focus_stats: FocusStats,
}

/// Период фокуса — отрезок времени пока Claude обрабатывает запрос
#[derive(Debug)]
pub struct FocusPeriod {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub claude_state: ClaudeState,
    pub activities: Vec<UserActivity>,
    pub was_afk: bool,
}

/// Состояние Claude в данный момент
#[derive(Debug, Clone, Copy)]
pub enum ClaudeState {
    /// Claude обрабатывает запрос (от user event до assistant event)
    Processing,
    /// Между ходами — пользователь читает ответ / думает
    UserThinking,
}

/// Активность пользователя в конкретный момент
#[derive(Debug)]
pub struct UserActivity {
    pub app: String,
    pub title: String,
    pub category: AppCategory,
    pub duration_secs: f64,
}

/// Агрегированная статистика фокуса
#[derive(Debug, Default)]
pub struct FocusStats {
    /// Общее время ожидания ответа Claude (сек)
    pub total_processing_time_secs: f64,
    /// Общее время "раздумий" пользователя между ходами (сек)
    pub total_thinking_time_secs: f64,
    /// Время в Development приложениях во время processing
    pub focused_during_processing_secs: f64,
    /// Время в Communication/Browser/Other во время processing
    pub distracted_during_processing_secs: f64,
    /// Время AFK во время processing
    pub afk_during_processing_secs: f64,
    /// Процент фокуса во время ожидания
    pub focus_percentage: f64,
    /// Топ приложений по времени
    pub top_apps: Vec<(String, f64)>,
    /// Статистика браузерных страниц (заполняется при browse анализе)
    pub browse_stats: Option<BrowseStats>,
}

/// Статистика браузерных страниц за время сессии
#[derive(Debug)]
pub struct BrowseStats {
    /// Все уникальные страницы, отсортированные по времени
    pub pages: Vec<BrowsePage>,
    /// Суммарное время по категориям
    pub categories: Vec<(BrowserCategory, f64)>,
    /// Процент рабочих страниц
    pub work_related_pct: f64,
}

/// Одна уникальная браузерная страница
#[derive(Debug)]
pub struct BrowsePage {
    /// Очищенный заголовок (без " - Google Chrome - Profile")
    pub title: String,
    /// Категория страницы
    pub category: BrowserCategory,
    /// Суммарная длительность просмотра (сек)
    pub total_duration_secs: f64,
    /// Количество визитов (переключений на эту страницу)
    pub visit_count: usize,
}

/// Статистика tool calls по категориям
#[derive(Debug, Default, Clone)]
pub struct ToolCallStats {
    pub total: usize,
    pub read: usize,   // Read, Glob, Grep
    pub write: usize,  // Edit, Write, NotebookEdit
    pub bash: usize,   // Bash
    pub mcp: usize,    // все mcp__*
    pub devboy: usize, // mcp__*devboy* или *dev-boy* (подмножество MCP)
}

impl ToolCallStats {
    /// Классифицировать и добавить один tool call
    pub fn add_tool(&mut self, name: &str) {
        self.total += 1;
        let lower = name.to_lowercase();
        if lower.starts_with("mcp__") {
            self.mcp += 1;
            if lower.contains("devboy") || lower.contains("dev-boy") {
                self.devboy += 1;
            }
        } else {
            match name {
                "Read" | "Glob" | "Grep" => self.read += 1,
                "Edit" | "Write" | "NotebookEdit" => self.write += 1,
                "Bash" => self.bash += 1,
                _ => {} // Task, WebFetch, WebSearch и т.д. — только в total
            }
        }
    }

    /// Объединить с другой статистикой
    pub fn merge(&mut self, other: &ToolCallStats) {
        self.total += other.total;
        self.read += other.read;
        self.write += other.write;
        self.bash += other.bash;
        self.mcp += other.mcp;
        self.devboy += other.devboy;
    }
}

/// Источник группировки задачи
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskGroupSource {
    /// Из git branch (DEV-569)
    Branch,
    /// LLM classification
    Llm,
    /// Fallback по session slug
    Session,
}

impl TaskGroupSource {
    pub fn label(&self) -> &str {
        match self {
            TaskGroupSource::Branch => "branch",
            TaskGroupSource::Llm => "llm",
            TaskGroupSource::Session => "session",
        }
    }
}

/// Агрегированная статистика по задаче (task ID из git branch)
#[derive(Debug)]
pub struct TaskStats {
    /// Технический ID для отображения и команд: "DEV-569" (branch) или "b1054e1a" (session ID)
    pub display_id: String,
    /// Внутренний ключ группировки (LLM label, slug, branch ID) — для кеша и суммаризации
    pub task_id: String,
    /// Описание из суффикса branch name или LLM summary
    pub description: Option<String>,
    /// Имя проекта (или "various" если несколько)
    pub project_name: String,
    /// Количество уникальных сессий
    pub session_count: usize,
    /// Короткие ID сессий (первые 8 символов UUID)
    pub session_ids: Vec<String>,
    /// Количество turns на этой ветке
    pub turn_count: usize,
    /// Количество human turns (с реальным сообщением пользователя)
    pub human_turn_count: usize,
    /// Время работы агента (assistant_ts - user_ts), сек
    pub agent_time_secs: f64,
    /// Время фокуса пользователя из TerminalFocusStats (если AW доступен)
    pub human_time_secs: Option<f64>,
    /// Dirty human time: пользователь НЕ AFK пока агент работал (если AW доступен)
    pub dirty_human_time_secs: Option<f64>,
    /// Стоимость в USD
    pub cost_usd: f64,
    /// Первое появление задачи
    pub first_seen: DateTime<Utc>,
    /// Последнее появление задачи
    pub last_seen: DateTime<Utc>,
    /// Источник группировки (branch, llm, session slug)
    pub group_source: TaskGroupSource,
    /// Статус задачи из LLM суммаризации: "completed" | "in_progress" | "blocked"
    pub status: Option<String>,
    /// Короткий заголовок задачи (3-7 слов) из LLM или manual
    pub title: Option<String>,
    /// Статистика tool calls по категориям
    pub tool_calls: ToolCallStats,
}

/// Статистика фокуса терминала: агент vs человек
#[derive(Debug)]
pub struct TerminalFocusStats {
    /// Время когда пользователь держал ЭТОТ терминал в фокусе и не был AFK
    pub human_focused_secs: f64,
    /// Время когда Claude обрабатывал запрос, а пользователь НЕ смотрел на этот терминал
    pub agent_autonomous_secs: f64,
    /// Время AFK за время сессии (терминал в фокусе, но пользователь отошёл)
    pub afk_secs: f64,
    /// Время в других приложениях (не этот терминал, не AFK)
    pub other_app_secs: f64,
    /// Общее время Processing (Claude работает)
    pub total_processing_secs: f64,
    /// Общее время UserThinking (пользователь думает/печатает)
    pub total_thinking_secs: f64,
    /// Dirty human time: пользователь НЕ AFK пока агент обрабатывал запрос (любое приложение)
    pub dirty_human_secs: f64,
}

/// Информация о фокусе пользователя для конкретного turn
#[derive(Debug)]
pub struct TurnFocusInfo {
    /// Основное приложение (максимум по времени) во время processing
    pub primary_app: Option<String>,
    /// Заголовок окна основного приложения
    pub primary_title: Option<String>,
    /// Был ли пользователь AFK во время processing
    pub was_afk: bool,
    /// Смотрел ли пользователь на ЭТОТ терминал
    pub was_watching_terminal: bool,
    /// Длительность processing (сек)
    pub processing_secs: f64,
    /// Время не-AFK во время processing (сек)
    pub not_afk_secs: f64,
    /// Время фокуса на ЭТОМ терминале (не AFK) во время processing (сек)
    pub watching_terminal_secs: f64,
}
