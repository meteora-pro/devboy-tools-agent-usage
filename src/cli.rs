#![allow(clippy::large_enum_variant)]
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

    /// Сбор и анализ tmux activity (focus tracking без ActivityWatch).
    Activity {
        #[command(subcommand)]
        action: ActivityAction,
    },

    /// Компактная сводка для tmux status-bar (5h блок + weekly % + plan).
    ///
    /// Width-stable: при разной нагрузке ширина строки сохраняется.
    /// Идеально подходит для интеграции в #[status-right].
    Statusline {
        /// Account override (по умолчанию — detect_current()).
        #[arg(long)]
        account: Option<String>,

        /// Формат:
        ///   tmux  — одна width-stable строка для status-bar
        ///   raw   — человеко-читаемая одна строка
        ///   json  — все поля для кастомного рендеринга
        #[arg(short, long, default_value = "tmux")]
        format: StatuslineFormat,
    },

    /// Биом-классификация сессий (aquarium: 🐋🦈🐬🐟🦐🦠).
    Biome {
        /// Фильтр по account_id.
        #[arg(long)]
        account: Option<String>,

        /// Начальная дата (YYYY-MM-DD).
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата (YYYY-MM-DD).
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Reconciliation: сопоставить local JSONL tokens с endpoint utilization %.
    ///
    /// Показывает: какая часть Δ% endpoint объясняется работой с этой машины,
    /// сколько local tokens = 1% utilization, моменты drift (другие машины тратили).
    Reconcile {
        /// Account (по умолчанию detect_current).
        #[arg(long)]
        account: Option<String>,

        /// Начальная дата (YYYY-MM-DD).
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата.
        #[arg(long)]
        to: Option<String>,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Real-time usage из Anthropic OAuth /api/oauth/usage.
    ///
    /// Те же проценты что показывает /status внутри Claude Code.
    /// Cache TTL 120 сек по умолчанию (защита от 429 endpoint'а).
    Usage {
        /// Принудительно сделать fresh fetch минуя cache.
        #[arg(long)]
        refresh: bool,

        /// TTL cache в секундах (default 120). Используется для расчёта Cached/Stale.
        #[arg(long, default_value = "120")]
        ttl: i64,

        /// Account для записи в snapshot (по умолчанию — detect_current).
        #[arg(long)]
        account: Option<String>,

        /// Показать историю snapshots вместо текущего значения (с delta-колонками).
        #[arg(long)]
        history: bool,

        /// История: начало (YYYY-MM-DD).
        #[arg(long)]
        from: Option<String>,

        /// История: конец (YYYY-MM-DD).
        #[arg(long)]
        to: Option<String>,

        /// История: лимит rows.
        #[arg(short, long, default_value = "30")]
        limit: usize,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Управление weekly ceiling: show / set manual override.
    ///
    /// Без флагов — показать текущий ceiling и его источник (manual /
    /// calibrated / default-community).
    /// С --set — записать manual override (например после узнавания точной
    /// цифры через интерактивный /status в Claude Code).
    Ceiling {
        /// Account override (по умолчанию — detect_current()).
        #[arg(long)]
        account: Option<String>,

        /// Установить manual ceiling. Понимает суффиксы: 44M, 220M, 100k.
        #[arg(long)]
        set: Option<String>,

        /// Опциональная заметка к override.
        #[arg(long)]
        notes: Option<String>,

        /// Формат вывода (для show).
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Аккаунты Claude Code в индексе.
    ///
    /// По умолчанию list. `--switches` показывает историю переключений.
    Accounts {
        /// Показать историю переключений вместо списка аккаунтов.
        #[arg(long)]
        switches: bool,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Недельный rate-limit usage per account (% от ceiling плана).
    ///
    /// По умолчанию — текущее окно для текущего аккаунта.
    Limits {
        /// Фильтр по account_id (по умолчанию — текущий из credentials).
        #[arg(long)]
        account: Option<String>,

        /// Какое окно: "current" (default), число (W0/W1/...) или "all" (последние N).
        #[arg(long, default_value = "current")]
        week: String,

        /// Сколько окон при --week=all (default 5).
        #[arg(short, long, default_value = "5")]
        limit: usize,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// 5-часовые rate-limit блоки (Anthropic Claude Code).
    ///
    /// По умолчанию показывает последние N блоков. С --active — только текущий.
    Blocks {
        /// Только активный блок (now < end_ms).
        #[arg(long)]
        active: bool,

        /// Фильтр по account_id (как из таблицы accounts).
        #[arg(long)]
        account: Option<String>,

        /// Сколько последних блоков показать (без --active).
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },

    /// Обновить SQLite-индекс по JSONL логам (incremental).
    ///
    /// При первом запуске парсит весь ~/.claude/projects/ (cold ≈ 30-60 сек на 2 GB).
    /// Последующие запуски обрабатывают только новые/изменённые файлы (warm ≈ <500 ms).
    Index {
        /// Полная переиндексация: очистить parsed_files и turns, парсить всё заново.
        #[arg(long)]
        full: bool,

        /// Тихий режим — только финальная сводка на одной строке (для cron / tmux wrapper).
        #[arg(long, short)]
        quiet: bool,
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

#[derive(Subcommand)]
pub enum ActivityAction {
    /// Снять single snapshot tmux state и записать в БД.
    Collect {
        /// Не записывать в БД, только напечатать результат (dry-run).
        #[arg(long)]
        dry_run: bool,
    },

    /// Long-running daemon: периодически снимает snapshot.
    /// Запускается отдельным процессом (например через systemd-user).
    Watch {
        /// Интервал между snapshot в секундах.
        #[arg(short, long, default_value = "10")]
        interval: u64,
    },

    /// Отчёт по собранной активности.
    Report {
        /// Начальная дата (YYYY-MM-DD).
        #[arg(long)]
        from: Option<String>,

        /// Конечная дата.
        #[arg(long)]
        to: Option<String>,

        /// Сколько top commands показать.
        #[arg(short, long, default_value = "10")]
        top: usize,

        /// Формат вывода.
        #[arg(short, long, default_value = "table")]
        format: OutputFormat,
    },
}

#[derive(Clone, ValueEnum)]
pub enum StatuslineFormat {
    Tmux,
    Raw,
    Json,
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
