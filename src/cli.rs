use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "devboy-agent-usage",
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
