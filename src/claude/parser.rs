use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::models::ClaudeEvent;

/// Информация о найденном JSONL файле
#[derive(Debug, Clone)]
pub struct JsonlFileInfo {
    pub path: PathBuf,
    /// Имя проекта (извлечённое из имени директории)
    pub project_name: String,
    /// Полный путь проекта (восстановленный)
    pub project_path: String,
    /// Является ли файл логом subagent-а
    pub is_subagent: bool,
}

/// Обнаружить все JSONL файлы в директории Claude Code проектов
pub fn discover_jsonl_files(claude_projects_dir: &Path) -> Result<Vec<JsonlFileInfo>> {
    let pattern = format!("{}/**/*.jsonl", claude_projects_dir.display());
    let mut files = Vec::new();

    for entry in glob::glob(&pattern).context("Неверный glob паттерн")? {
        let path = entry.context("Ошибка чтения записи glob")?;

        // Определяем является ли файл subagent логом
        let is_subagent = path
            .to_str()
            .map(|s| s.contains("/subagents/"))
            .unwrap_or(false);

        // Извлекаем имя проекта из имени директории
        // Структура: ~/.claude/projects/-Users-user-projects-name/session.jsonl
        let project_dir = if is_subagent {
            // subagents/agent-xxx.jsonl — поднимаемся на 3 уровня
            path.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
        } else {
            path.parent()
        };

        let (project_name, project_path) = project_dir
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .map(extract_project_info)
            .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

        files.push(JsonlFileInfo {
            path,
            project_name,
            project_path,
            is_subagent,
        });
    }

    Ok(files)
}

/// Извлечь имя и путь проекта из имени директории
/// "-Users-andreymaznyak-projects-meteora-dev-boy-monorepo" → ("meteora/dev-boy-monorepo", "-Users-andreymaznyak-projects-meteora-dev-boy-monorepo")
fn extract_project_info(dir_name: &str) -> (String, String) {
    // Имя директории — это путь с "-" вместо "/"
    // Проблема: дефисы в именах директорий неотличимы от разделителей
    // Стратегия: проверяем реальные пути на файловой системе
    let restored = restore_path_by_checking_fs(dir_name);
    let project_name = if let Some(ref path) = restored {
        // Убираем домашнюю директорию для краткости
        let home = dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_default();
        let clean = path.strip_prefix(&home).unwrap_or(path);
        let clean = clean.strip_prefix('/').unwrap_or(clean);
        // Убираем "projects/" prefix если есть
        let clean = clean.strip_prefix("projects/").unwrap_or(clean);
        clean.to_string()
    } else {
        dir_name.to_string()
    };
    let project_path = restored.unwrap_or_else(|| dir_name.to_string());
    (project_name, project_path)
}

/// Проверить, существует ли путь как директория, пробуя также вариант с "_" вместо "-"
/// Возвращает реальное имя компонента (с дефисами или подчёркиваниями)
fn try_component_variants(base_path: &str, component: &str) -> Option<String> {
    // Вариант 1: как есть (с дефисами)
    let candidate = format!("{}{}", base_path, component);
    if std::path::Path::new(&candidate).exists() {
        return Some(component.to_string());
    }

    // Вариант 2: заменяем дефисы на подчёркивания (DEV-ENV-2 → DEV_ENV_2)
    if component.contains('-') {
        let with_underscores = component.replace('-', "_");
        let candidate = format!("{}{}", base_path, with_underscores);
        if std::path::Path::new(&candidate).exists() {
            return Some(with_underscores);
        }
    }

    None
}

/// Восстановить путь из mangled имени директории, проверяя существование на FS
fn restore_path_by_checking_fs(dir_name: &str) -> Option<String> {
    // Убираем начальный "-"
    let name = dir_name.strip_prefix('-').unwrap_or(dir_name);

    // Рекурсивно пробуем разбить строку на компоненты пути
    // начиная с "/", проверяя каждый уровень на FS
    let mut current_path = String::from("/");
    let mut remaining = name;

    while !remaining.is_empty() {
        let mut found = false;
        // Пробуем все возможные длины для текущего компонента
        // от самого длинного к самому короткому
        let dashes: Vec<usize> = remaining.match_indices('-').map(|(i, _)| i).collect();

        // Сначала пробуем взять всё remaining как последний компонент
        // (с вариантами дефисов/подчёркиваний)
        if let Some(real_name) = try_component_variants(&current_path, remaining) {
            return Some(format!("{}{}", current_path, real_name));
        }

        // Пробуем от самого длинного компонента к самому короткому
        for &dash_pos in dashes.iter().rev() {
            let component = &remaining[..dash_pos];
            if let Some(real_name) = try_component_variants(&current_path, component) {
                let candidate = format!("{}{}", current_path, real_name);
                if std::path::Path::new(&candidate).is_dir() {
                    current_path = format!("{}/", candidate);
                    remaining = &remaining[dash_pos + 1..];
                    found = true;
                    break;
                }
            }
        }

        if !found {
            // Не удалось найти компонент — fallback на первый "-"
            if let Some(dash_pos) = remaining.find('-') {
                let component = &remaining[..dash_pos];
                current_path = format!("{}{}/", current_path, component);
                remaining = &remaining[dash_pos + 1..];
            } else {
                current_path = format!("{}{}", current_path, remaining);
                break;
            }
        }
    }

    // Убираем trailing slash
    let result = current_path.trim_end_matches('/').to_string();
    if result.len() > 1 {
        Some(result)
    } else {
        None
    }
}

/// Заменяет буквальные символы новой строки внутри JSON-строк на escape-последовательности.
///
/// Логи Claude Code иногда содержат tool responses с содержимым файлов,
/// где символы `\n` не экранированы. По стандарту JSON это невалидно,
/// но обрабатываем gracefully.
fn escape_literal_newlines_in_strings(content: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut in_string = false;
    let bytes = content.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string {
            match b {
                b'\\' if i + 1 < bytes.len() => {
                    // Escape-последовательность — копируем два байта как есть
                    output.push(b as char);
                    output.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                b'"' => {
                    in_string = false;
                    output.push('"');
                }
                b'\n' => {
                    // Буквальный перенос строки внутри JSON-строки — экранируем
                    output.push('\\');
                    output.push('n');
                }
                b'\r' => {
                    output.push('\\');
                    output.push('r');
                }
                _ => {
                    output.push(b as char);
                }
            }
        } else {
            match b {
                b'"' => {
                    in_string = true;
                    output.push('"');
                }
                _ => {
                    output.push(b as char);
                }
            }
        }

        i += 1;
    }

    output
}

/// Потоковый парсинг JSONL файла.
///
/// Корректно обрабатывает записи, чьи строковые поля содержат буквальные
/// символы перевода строки (невалидный JSONL, но реально встречается в логах
/// Claude Code при больших tool responses с содержимым файлов).
pub fn parse_jsonl_file(path: &Path) -> Result<Vec<ClaudeEvent>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Не удалось открыть {}", path.display()))?;

    // Исправляем буквальные переносы строк внутри JSON-строк
    let content = escape_literal_newlines_in_strings(&raw);

    let mut events = Vec::new();
    let mut errors = 0u64;

    // Читаем построчно: после pre-processing каждая запись — одна строка
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<ClaudeEvent>(line) {
            Ok(event) => events.push(event),
            Err(_) => {
                errors += 1;
            }
        }
    }

    if errors > 0 {
        eprintln!(
            "  Предупреждение: {} записей не удалось распарсить в {}",
            errors,
            path.display()
        );
    }

    Ok(events)
}

/// Парсинг всех JSONL файлов с прогрессом (однопоточный вариант)
pub fn parse_all_files(files: &[JsonlFileInfo]) -> Vec<(JsonlFileInfo, Vec<ClaudeEvent>)> {
    let mut results = Vec::new();

    for file_info in files {
        match parse_jsonl_file(&file_info.path) {
            Ok(events) if !events.is_empty() => {
                results.push((file_info.clone(), events));
            }
            Ok(_) => {} // Пустой файл — пропускаем
            Err(e) => {
                eprintln!("Ошибка парсинга {}: {}", file_info.path.display(), e);
            }
        }
    }

    results
}
