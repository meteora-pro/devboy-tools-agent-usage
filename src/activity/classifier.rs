use super::models::{AppCategory, BrowserCategory};

/// Классифицировать приложение по категории
pub fn classify_app(app: &str) -> AppCategory {
    let app_lower = app.to_lowercase();

    // IDE и терминалы
    if matches!(
        app_lower.as_str(),
        "terminal"
            | "iterm2"
            | "iterm"
            | "alacritty"
            | "kitty"
            | "wezterm"
            | "warp"
            | "hyper"
            | "code"
            | "code - insiders"
            | "visual studio code"
            | "cursor"
            | "zed"
            | "intellij idea"
            | "webstorm"
            | "pycharm"
            | "rustrover"
            | "clion"
            | "goland"
            | "neovim"
            | "vim"
            | "emacs"
            | "sublime text"
            | "xcode"
    ) {
        return AppCategory::Development;
    }

    // Мессенджеры
    if matches!(
        app_lower.as_str(),
        "slack"
            | "telegram"
            | "whatsapp"
            | "discord"
            | "microsoft teams"
            | "teams"
            | "zoom"
            | "skype"
            | "signal"
            | "messages"
    ) {
        return AppCategory::Communication;
    }

    // Браузеры
    if matches!(
        app_lower.as_str(),
        "google chrome"
            | "chrome"
            | "safari"
            | "firefox"
            | "brave browser"
            | "brave"
            | "microsoft edge"
            | "arc"
            | "opera"
            | "vivaldi"
    ) {
        return AppCategory::Browser;
    }

    AppCategory::Other
}

/// Очистить заголовок окна браузера от суффикса с именем браузера и профиля.
///
/// Примеры:
/// - "Merge requests · GitLab - Google Chrome - Andrey (Main)" → "Merge requests · GitLab"
/// - "DevBoy - Google Chrome" → "DevBoy"
/// - "Claude - Google Chrome - Andrey (Main)" → "Claude"
pub fn clean_browser_title(title: &str) -> String {
    // Паттерны браузеров, которые нужно удалить из конца заголовка
    let browser_markers = [
        " - Google Chrome",
        " - Chromium",
        " - Brave",
        " - Microsoft Edge",
        " - Mozilla Firefox",
        " - Safari",
        " - Arc",
        " - Opera",
        " - Vivaldi",
    ];

    let mut cleaned = title.to_string();

    for marker in &browser_markers {
        if let Some(idx) = cleaned.find(marker) {
            cleaned = cleaned[..idx].to_string();
            break;
        }
    }

    cleaned.trim().to_string()
}

/// Классифицировать заголовок браузерной вкладки по категориям
pub fn classify_browser_title(title: &str) -> BrowserCategory {
    let cleaned = clean_browser_title(title);
    let lower = cleaned.to_lowercase();

    // Порядок проверок: от конкретных к общим
    if lower.contains("gitlab") {
        return BrowserCategory::GitLab;
    }
    if lower.contains("github") {
        return BrowserCategory::GitHub;
    }
    if lower.contains("clickup") {
        return BrowserCategory::ClickUp;
    }
    if lower.contains("jira") || lower.contains("atlassian.net/browse") {
        return BrowserCategory::Jira;
    }
    if lower.contains("claude") && (lower.contains("claude.ai") || lower == "claude") {
        return BrowserCategory::Claude;
    }
    if lower.contains("chatgpt") || lower.contains("chat.openai") {
        return BrowserCategory::ChatGPT;
    }
    if lower.contains("stack overflow") || lower.contains("stackoverflow") {
        return BrowserCategory::StackOverflow;
    }
    // Техническая документация
    if lower.contains("docs.rs")
        || lower.contains("mdn web docs")
        || lower.contains("developer.mozilla")
        || lower.contains("devdocs.io")
        || lower.contains("rust-lang")
        || lower.contains("crates.io")
        || lower.contains("npmjs.com")
        || lower.contains("pkg.go.dev")
        || lower.contains("docs.python.org")
        || lower.contains("typescriptlang.org")
        || lower.contains("react.dev")
        || lower.contains("kubernetes.io/docs")
        || lower.contains("docker docs")
    {
        return BrowserCategory::DevDocs;
    }
    // Документы / заметки
    if lower.contains("google docs")
        || lower.contains("docs.google")
        || lower.contains("notion")
        || lower.contains("confluence")
        || lower.contains("google sheets")
        || lower.contains("google slides")
        || lower.contains("figma")
    {
        return BrowserCategory::Docs;
    }
    // Почта
    if lower.contains("gmail")
        || lower.contains("mail.google")
        || lower.contains("outlook")
        || lower.contains("mail.ru")
        || lower.contains("inbox")
    {
        return BrowserCategory::Email;
    }
    // Соцсети и развлечения
    if lower.contains("youtube")
        || lower.contains("twitter")
        || lower.contains("reddit")
        || lower.contains("facebook")
        || lower.contains("instagram")
        || lower.contains("tiktok")
        || lower.contains("linkedin")
        || lower.contains("habr")
        || lower.contains("pikabu")
        || lower.contains("twitch")
        || lower.contains("vk.com")
        || lower.contains("вконтакте")
        || lower.contains("x.com")
    {
        return BrowserCategory::Social;
    }
    // Кастомные web-приложения (короткие заголовки = скорее всего SPA)
    if lower == "devboy" || lower.contains("dev-boy") || lower.contains("dev boy") {
        return BrowserCategory::Custom("DevBoy".to_string());
    }

    BrowserCategory::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_browser_title() {
        assert_eq!(
            clean_browser_title(
                "Merge requests · Meteora / dev-boy · GitLab - Google Chrome - Andrey (Main)"
            ),
            "Merge requests · Meteora / dev-boy · GitLab"
        );
        assert_eq!(clean_browser_title("DevBoy - Google Chrome"), "DevBoy");
        assert_eq!(
            clean_browser_title("Claude - Google Chrome - Andrey (Main)"),
            "Claude"
        );
    }

    #[test]
    fn test_classify_browser_title_gitlab() {
        assert_eq!(
            classify_browser_title(
                "Merge requests · Meteora / dev-boy · GitLab - Google Chrome - Andrey (Main)"
            ),
            BrowserCategory::GitLab
        );
    }

    #[test]
    fn test_classify_browser_title_clickup() {
        assert_eq!(
            classify_browser_title(
                "OAuth per User — персональные токены | DEV-559 - ClickUp - Google Chrome"
            ),
            BrowserCategory::ClickUp
        );
    }

    #[test]
    fn test_classify_browser_title_devboy() {
        assert_eq!(
            classify_browser_title("DevBoy - Google Chrome"),
            BrowserCategory::Custom("DevBoy".to_string())
        );
    }

    #[test]
    fn test_classify_browser_title_social() {
        assert_eq!(
            classify_browser_title("YouTube - Google Chrome"),
            BrowserCategory::Social
        );
    }

    #[test]
    fn test_classify_browser_title_claude() {
        assert_eq!(
            classify_browser_title("Claude - Google Chrome - Andrey (Main)"),
            BrowserCategory::Claude
        );
    }
}
