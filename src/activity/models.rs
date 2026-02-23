use chrono::{DateTime, Utc};

/// Bucket из ActivityWatch (контейнер для событий)
#[derive(Debug)]
pub struct AwBucket {
    pub key: i64,
    pub id: String,
    pub bucket_type: String,
    pub hostname: String,
}

/// Событие активного окна
#[derive(Debug, Clone)]
pub struct AwWindowEvent {
    pub timestamp: DateTime<Utc>,
    pub duration_secs: f64,
    pub app: String,
    pub title: String,
}

impl AwWindowEvent {
    pub fn end_time(&self) -> DateTime<Utc> {
        self.timestamp + chrono::Duration::milliseconds((self.duration_secs * 1000.0) as i64)
    }
}

/// Событие AFK статуса
#[derive(Debug, Clone)]
pub struct AwAfkEvent {
    pub timestamp: DateTime<Utc>,
    pub duration_secs: f64,
    pub status: AfkStatus,
}

impl AwAfkEvent {
    pub fn end_time(&self) -> DateTime<Utc> {
        self.timestamp + chrono::Duration::milliseconds((self.duration_secs * 1000.0) as i64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AfkStatus {
    Afk,
    NotAfk,
}

/// Категория приложения
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AppCategory {
    /// Terminal, VS Code, IDE — работа с кодом
    Development,
    /// Slack, Telegram, Discord — мессенджеры
    Communication,
    /// Chrome, Safari, Firefox — браузер
    Browser,
    /// Всё остальное
    Other,
}

impl AppCategory {
    pub fn is_focused(&self) -> bool {
        matches!(self, AppCategory::Development)
    }

    pub fn label(&self) -> &'static str {
        match self {
            AppCategory::Development => "Development",
            AppCategory::Communication => "Communication",
            AppCategory::Browser => "Browser",
            AppCategory::Other => "Other",
        }
    }
}

/// Категория браузерной страницы (по заголовку вкладки)
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserCategory {
    /// GitLab — MR, issues, pipelines
    GitLab,
    /// GitHub — PR, issues, repos
    GitHub,
    /// ClickUp — задачи, доски
    ClickUp,
    /// Jira — задачи, доски
    Jira,
    /// Claude AI (claude.ai web)
    Claude,
    /// ChatGPT
    ChatGPT,
    /// Документация: Google Docs, Notion, Confluence
    Docs,
    /// Stack Overflow
    StackOverflow,
    /// Техническая документация: docs.rs, MDN, и т.д.
    DevDocs,
    /// Соцсети: Twitter, YouTube, Reddit, Facebook
    Social,
    /// Почта: Gmail, Outlook
    Email,
    /// Кастомное приложение (DevBoy и т.д.)
    Custom(String),
    /// Всё остальное
    Other,
}

impl BrowserCategory {
    /// Человекочитаемая метка категории
    pub fn label(&self) -> &str {
        match self {
            BrowserCategory::GitLab => "GitLab",
            BrowserCategory::GitHub => "GitHub",
            BrowserCategory::ClickUp => "ClickUp",
            BrowserCategory::Jira => "Jira",
            BrowserCategory::Claude => "Claude",
            BrowserCategory::ChatGPT => "ChatGPT",
            BrowserCategory::Docs => "Docs",
            BrowserCategory::StackOverflow => "StackOverflow",
            BrowserCategory::DevDocs => "DevDocs",
            BrowserCategory::Social => "Social",
            BrowserCategory::Email => "Email",
            BrowserCategory::Custom(name) => name.as_str(),
            BrowserCategory::Other => "Other",
        }
    }

    /// Является ли страница рабочей (связанной с разработкой)
    pub fn is_work_related(&self) -> bool {
        matches!(
            self,
            BrowserCategory::GitLab
                | BrowserCategory::GitHub
                | BrowserCategory::ClickUp
                | BrowserCategory::Jira
                | BrowserCategory::Claude
                | BrowserCategory::ChatGPT
                | BrowserCategory::Docs
                | BrowserCategory::StackOverflow
                | BrowserCategory::DevDocs
                | BrowserCategory::Custom(_)
        )
    }
}
