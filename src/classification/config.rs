/// Провайдер LLM API
#[derive(Debug, Clone, PartialEq)]
pub enum LlmProvider {
    /// Anthropic Messages API (Z.ai proxy)
    Anthropic,
    /// OpenAI-compatible API (Ollama, LM Studio, vLLM)
    OpenAiCompatible,
}

/// Конфигурация LLM клиента для классификации и суммаризации
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Тип провайдера
    pub provider: LlmProvider,
    /// URL endpoint
    pub api_url: String,
    /// API ключ (Optional — не нужен для Ollama)
    pub api_key: Option<String>,
    /// Модель для LLM запросов
    pub model: String,
    /// Количество сообщений в одном батче
    pub batch_size: usize,
    /// Максимальное количество параллельных запросов
    pub concurrency: usize,
    /// Таймаут запроса в секундах
    pub timeout_secs: u64,
}

impl LlmConfig {
    /// Создать конфигурацию из переменных окружения
    ///
    /// Env vars:
    /// - TRACK_CLAUDE_LLM_PROVIDER — "anthropic" | "openai" (default: "anthropic")
    /// - TRACK_CLAUDE_LLM_URL — полный URL endpoint (auto по provider)
    /// - TRACK_CLAUDE_LLM_API_KEY — API ключ (fallback: ANTHROPIC_AUTH_TOKEN)
    /// - TRACK_CLAUDE_LLM_MODEL — модель (default: haiku / qwen2.5:7b по provider)
    /// - ANTHROPIC_AUTH_TOKEN — legacy API ключ для Anthropic
    /// - ANTHROPIC_BASE_URL — legacy base URL для Anthropic
    pub fn from_env() -> Option<Self> {
        let provider = match std::env::var("TRACK_CLAUDE_LLM_PROVIDER")
            .unwrap_or_else(|_| "anthropic".to_string())
            .to_lowercase()
            .as_str()
        {
            "openai" => LlmProvider::OpenAiCompatible,
            _ => LlmProvider::Anthropic,
        };

        // API ключ: TRACK_CLAUDE_LLM_API_KEY → ANTHROPIC_AUTH_TOKEN → None
        let api_key = std::env::var("TRACK_CLAUDE_LLM_API_KEY")
            .ok()
            .or_else(|| std::env::var("ANTHROPIC_AUTH_TOKEN").ok());

        // Для Anthropic provider API key обязателен
        if provider == LlmProvider::Anthropic && api_key.is_none() {
            return None;
        }

        // URL: TRACK_CLAUDE_LLM_URL → дефолт по provider
        let api_url = std::env::var("TRACK_CLAUDE_LLM_URL").unwrap_or_else(|_| match provider {
            LlmProvider::Anthropic => {
                let base = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.z.ai/api/anthropic".to_string());
                format!("{}/v1/messages", base)
            }
            LlmProvider::OpenAiCompatible => {
                "http://localhost:11434/v1/chat/completions".to_string()
            }
        });

        // Модель: TRACK_CLAUDE_LLM_MODEL → дефолт по provider
        let model = std::env::var("TRACK_CLAUDE_LLM_MODEL").unwrap_or_else(|_| match provider {
            LlmProvider::Anthropic => "claude-3-5-haiku-20241022".to_string(),
            LlmProvider::OpenAiCompatible => "qwen2.5:7b".to_string(),
        });

        Some(LlmConfig {
            provider,
            api_url,
            api_key,
            model,
            batch_size: 20,
            concurrency: 3,
            timeout_secs: 60,
        })
    }
}
