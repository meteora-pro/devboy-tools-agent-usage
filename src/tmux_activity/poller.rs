//! Снятие snapshot tmux state через `tmux list-panes -a -F "..."`.
//!
//! Не зависит от tmux libraries — просто spawn child process и парсинг
//! tab-separated stdout. Если tmux сервер не запущен — возвращаем пустой
//! Vec без ошибки (нормальное состояние, не блокирует daemon).

use anyhow::{Context, Result};
use serde::Serialize;
use std::process::Command;

/// Состояние одной tmux pane на момент snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneSnapshot {
    pub session: String,
    pub window_idx: i64,
    pub window_name: String,
    pub pane_idx: i64,
    pub pane_active: bool,
    pub command: String,
    pub cwd: String,
}

/// Tab-separated формат для `tmux list-panes -F`. Tab — безопасный
/// разделитель: pane_current_command не содержит tabs, cwd — тоже
/// (rare-edge cases с tab в имени файла мы игнорируем).
const TMUX_FORMAT: &str = "#{session_name}\t#{window_index}\t#{window_name}\t#{pane_index}\t#{pane_active}\t#{pane_current_command}\t#{pane_current_path}";

/// Снять snapshot всех pane'ов через реальный tmux binary.
/// Возвращает пустой Vec если tmux сервер не запущен.
pub fn poll() -> Result<Vec<PaneSnapshot>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", TMUX_FORMAT])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // tmux не установлен — это не наша ошибка, возвращаем пустой набор
            return Ok(Vec::new());
        }
        Err(e) => return Err(e).context("запуск tmux"),
    };

    if !output.status.success() {
        // tmux есть, но сервер не запущен ("no server running on ...")
        // или другая ошибка stderr — для daemon'а это нормально, не блокируем.
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_tmux_output(&stdout)
}

/// Распарсить вывод `tmux list-panes`. Каждая строка — TAB-разделённый snapshot.
pub fn parse_tmux_output(text: &str) -> Result<Vec<PaneSnapshot>> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_pane_line(line) {
            Some(p) => out.push(p),
            None => {
                eprintln!("Warning: пропускаем malformed tmux line: {:?}", line);
            }
        }
    }
    Ok(out)
}

fn parse_pane_line(line: &str) -> Option<PaneSnapshot> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() != 7 {
        return None;
    }
    Some(PaneSnapshot {
        session: parts[0].to_string(),
        window_idx: parts[1].parse().ok()?,
        window_name: parts[2].to_string(),
        pane_idx: parts[3].parse().ok()?,
        pane_active: parts[4] == "1",
        command: parts[5].to_string(),
        cwd: parts[6].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_line() {
        let line = "main\t2\teditor\t0\t1\tvim\t/home/u/project";
        let p = parse_pane_line(line).unwrap();
        assert_eq!(p.session, "main");
        assert_eq!(p.window_idx, 2);
        assert_eq!(p.window_name, "editor");
        assert_eq!(p.pane_idx, 0);
        assert!(p.pane_active);
        assert_eq!(p.command, "vim");
        assert_eq!(p.cwd, "/home/u/project");
    }

    #[test]
    fn parse_inactive_pane() {
        let line = "work\t1\tbg\t0\t0\tbash\t/tmp";
        let p = parse_pane_line(line).unwrap();
        assert!(!p.pane_active);
    }

    #[test]
    fn malformed_line_returns_none() {
        assert!(parse_pane_line("not enough fields").is_none());
        assert!(parse_pane_line("a\tb\tc\td\te\tf").is_none());
        assert!(parse_pane_line("a\tNOT_AN_INT\tc\t0\t1\tcmd\t/").is_none());
    }

    #[test]
    fn parse_multiline() {
        let text = "main\t1\twin1\t0\t1\tclaude\t/p\n\
                    main\t2\twin2\t0\t1\tvim\t/p\n\
                    \n\
                    work\t1\twin1\t0\t0\tbash\t/q\n";
        let panes = parse_tmux_output(text).unwrap();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].window_name, "win1");
        assert_eq!(panes[2].session, "work");
        assert!(!panes[2].pane_active);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(parse_tmux_output("").unwrap().is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped_not_aborted() {
        let text = "main\t1\twin\t0\t1\tclaude\t/p\n\
                    broken line\n\
                    work\t2\tw\t0\t0\tbash\t/q\n";
        let panes = parse_tmux_output(text).unwrap();
        assert_eq!(panes.len(), 2, "1 хорошая + 1 пропущена + 1 хорошая");
    }

    #[test]
    fn paths_with_spaces_preserved() {
        let line = "s\t1\tw with spaces\t0\t1\tcmd\t/path with spaces/dir";
        let p = parse_pane_line(line).unwrap();
        assert_eq!(p.window_name, "w with spaces");
        assert_eq!(p.cwd, "/path with spaces/dir");
    }

    #[test]
    fn poll_does_not_panic_when_tmux_missing() {
        // Не можем легко emulate "tmux not installed", но проверим что poll()
        // в принципе возвращает Ok даже если result пустой.
        let result = poll();
        assert!(result.is_ok(), "poll() должен gracefully возвращать Ok");
    }
}
