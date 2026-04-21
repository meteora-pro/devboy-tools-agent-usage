use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "devboy-tools-agent-usage",
    version,
    about = "Анализ использования AI-агентов (Claude Code) с корреляцией ActivityWatch"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Общая сводка по всем сессиям
    Summary {
        /// Фильтр по проекту (подстрока в имени)
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Список сессий с фильтрацией
    Sessions {
        /// Фильтр по проекту
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата
        #[arg(long)]
        to: Option<String>,

        /// Максимальное количество сессий
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Детальный отчёт по конкретной сессии
    Session {
        /// ID сессии (UUID или подстрока)
        session_id: String,

        /// Показать корреляцию с ActivityWatch
        #[arg(long, default_value_t = true)]
        correlate: bool,

        /// Использовать LLM chunk summaries для расширенного вида
        #[arg(long, default_value_t = false)]
        with_llm: bool,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Список проектов с базовой статистикой
    Projects {
        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Анализ фокуса: чем занимался пользователь пока Claude работал
    Focus {
        /// Фильтр по проекту
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Детальная временная шкала сессии или группы сессий задачи
    Timeline {
        /// Task ID (DEV-570), session UUID или подстрока
        id: String,
    },

    /// Анализ браузерных страниц во время сессии Claude
    Browse {
        /// ID сессии (UUID или подстрока)
        session_id: String,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Группировка сессий по задачам (из git branch)
    Tasks {
        /// Фильтр по проекту
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата
        #[arg(long)]
        to: Option<String>,

        /// Включить данные ActivityWatch (human time)
        #[arg(long, default_value_t = false)]
        with_aw: bool,

        /// Использовать LLM для классификации и суммаризации активностей
        #[arg(long, env = "TRACK_CLAUDE_LLM_ENABLED", default_value_t = false)]
        with_llm: bool,

        /// Сортировка
        #[arg(long, default_value = "cost")]
        sort: TaskSortBy,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Очистить кеш суммаризации задач для пересуммаризации
    Reclassify {
        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Фильтр по проекту
        #[arg(short, long)]
        project: Option<String>,
    },

    /// Установить ручной заголовок задачи
    Retitle {
        /// ID задачи (например DEV-531, ~session-slug)
        task_id: String,

        /// Новый заголовок (3-7 слов)
        title: String,
    },

    /// Отчёт по расходу токенов и стоимости
    Cost {
        /// Фильтр по проекту
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата
        #[arg(long)]
        to: Option<String>,

        /// Группировка
        #[arg(short, long, default_value = "day")]
        group_by: GroupBy,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Анализ гипотезы обогащения контекста: корреляция chars_per_item → enrichment calls
    /// Показывает: при малом контексте айтема агент делает больше follow-up вызовов
    ContextEnrichment {
        /// Фильтр по исходному инструменту (default: get_issues)
        #[arg(short, long, default_value = "get_issues")]
        tool: String,

        /// Фильтр по проекту (подстрока в имени)
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Анализ поведения агента после получения больших MCP ответов
    /// Показывает что агент делал в том же turn'е и следующем после большого ответа
    ToolBehavior {
        /// Фильтр по инструменту (подстрока, например "issues")
        #[arg(short, long)]
        tool: Option<String>,

        /// Порог "большого" ответа в символах (default: 14000 ≈ 4k tokens)
        #[arg(long, default_value = "14000")]
        large_threshold: usize,

        /// Фильтр по проекту (подстрока в имени)
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Статистика размеров ответов MCP инструментов (chars, lines, percentiles)
    /// Показывает сколько токенов реально возвращают pipeline инструменты
    ToolResponseStats {
        /// Фильтр по проекту (подстрока в имени)
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Анализ поведенческих паттернов использования MCP pipeline инструментов
    /// Показывает p₁ (вероятность что первого чанка достаточно) и E[chunks] по инструментам
    McpPatterns {
        /// Фильтр по проекту (подстрока в имени)
        #[arg(short, long)]
        project: Option<String>,

        /// Начальная дата (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD)
        #[arg(long)]
        to: Option<String>,

        /// Показать детали по инвокациям (все вызовы)
        #[arg(long, default_value_t = false)]
        verbose: bool,

        /// Формат вывода
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Установить skill для AI-агентов (Claude Code, Cursor, Windsurf, Cline, Copilot)
    Install {
        /// Установить глобально (только для Claude Code)
        #[arg(long, short)]
        global: bool,

        /// Перезаписать существующие файлы
        #[arg(long, short)]
        force: bool,

        /// Целевой агент (по умолчанию: автоопределение)
        /// Можно указать несколько через запятую: --agent claude,cursor
        #[arg(long, short, value_delimiter = ',')]
        agent: Option<Vec<Agent>>,
    },
}

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
}

#[derive(Clone, ValueEnum)]
pub enum TaskSortBy {
    Cost,
    Time,
    Sessions,
    Recent,
}

#[derive(Clone, ValueEnum)]
pub enum GroupBy {
    Day,
    Week,
    Month,
    Session,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum Agent {
    Claude,
    Cursor,
    Windsurf,
    Cline,
    Copilot,
}
