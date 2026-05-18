use super::models::TokenUsage;

/// Цены за 1M токенов (USD)
struct ModelPricing {
    input_per_mtok: f64,
    output_per_mtok: f64,
    cache_write_per_mtok: f64,
    cache_read_per_mtok: f64,
}

/// Получить цены для модели
fn get_pricing(model: &str) -> ModelPricing {
    let model_lower = model.to_lowercase();
    if model_lower.contains("opus") {
        ModelPricing {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
            cache_write_per_mtok: 18.75,
            cache_read_per_mtok: 1.5,
        }
    } else if model_lower.contains("haiku") {
        ModelPricing {
            input_per_mtok: 0.80,
            output_per_mtok: 4.0,
            cache_write_per_mtok: 1.0,
            cache_read_per_mtok: 0.08,
        }
    } else {
        // Sonnet по умолчанию
        ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 3.75,
            cache_read_per_mtok: 0.30,
        }
    }
}

/// Рассчитать стоимость запроса
pub fn calculate_cost(usage: &TokenUsage, model: &str) -> f64 {
    calculate_cost_from_parts(
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        model,
    )
}

/// Рассчитать стоимость из raw token counts. Используется индексером,
/// который парсит JSONL без построения типизированной структуры TokenUsage.
pub fn calculate_cost_from_parts(
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    model: &str,
) -> f64 {
    let p = get_pricing(model);
    let input_cost = input as f64 * p.input_per_mtok / 1_000_000.0;
    let output_cost = output as f64 * p.output_per_mtok / 1_000_000.0;
    let cache_write_cost = cache_create as f64 * p.cache_write_per_mtok / 1_000_000.0;
    let cache_read_cost = cache_read as f64 * p.cache_read_per_mtok / 1_000_000.0;
    input_cost + output_cost + cache_write_cost + cache_read_cost
}

/// Форматировать количество токенов для отображения
pub fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        format!("{}", count)
    }
}

/// Форматировать стоимость
pub fn format_cost(cost: f64) -> String {
    if cost >= 1.0 {
        format!("${:.2}", cost)
    } else if cost >= 0.01 {
        format!("${:.3}", cost)
    } else {
        format!("${:.4}", cost)
    }
}
