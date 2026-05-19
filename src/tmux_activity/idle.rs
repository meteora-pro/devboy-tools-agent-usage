//! Best-effort AFK detection через `loginctl`.
//!
//! Из SSH/tmux pane мы не можем дотянуться до GNOME Mutter (нет linked
//! session bus). Поэтому используем systemd-logind, который доступен
//! глобально:
//!
//! 1. `loginctl list-sessions` → ищем GUI session (seat0).
//! 2. `loginctl show-session <id> -p IdleSinceHint -p IdleHint`.
//! 3. `IdleSinceHint` — UNIX timestamp в микросекундах последнего момента
//!    когда session НЕ была idle.
//! 4. idle_ms = (now_us - IdleSinceHint) / 1000, либо 0 если IdleHint=no.
//!
//! Если нет GUI session, нет seat0, или сервер не отдаёт данные —
//! возвращаем None и поле `idle_ms` остаётся NULL в таблице. Это не
//! блокирует сбор остальной активности.

use anyhow::Result;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Текущее значение idle в миллисекундах. None если best-effort не сработал.
pub fn current_idle_ms() -> Option<i64> {
    let session_id = find_gui_session_id()?;
    let raw = run_show_session(&session_id).ok()?;
    let now_us = now_unix_us();
    parse_idle_ms(&raw, now_us)
}

/// Найти id GUI session (та что прикреплена к seat, обычно seat0).
fn find_gui_session_id() -> Option<String> {
    let out = Command::new("loginctl")
        .args(["--no-legend", "list-sessions"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_gui_session_from_list(&text)
}

fn parse_gui_session_from_list(text: &str) -> Option<String> {
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // Формат: session uid user seat tty state idle ...
        // Берём первую active row с непустым seat (seat0).
        if cols.len() >= 6 && cols[3].starts_with("seat") && cols[5] == "active" {
            return Some(cols[0].to_string());
        }
    }
    None
}

fn run_show_session(session_id: &str) -> Result<String> {
    let out = Command::new("loginctl")
        .args([
            "show-session",
            session_id,
            "-p",
            "IdleHint",
            "-p",
            "IdleSinceHint",
        ])
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Распарсить вывод loginctl: KEY=VALUE строки.
/// Возвращает idle_ms, либо None если данные не доступны.
pub fn parse_idle_ms(text: &str, now_us: u64) -> Option<i64> {
    let mut idle_hint: Option<bool> = None;
    let mut idle_since: Option<u64> = None;

    for line in text.lines() {
        if let Some(v) = line.strip_prefix("IdleHint=") {
            match v.trim() {
                "yes" => idle_hint = Some(true),
                "no" => idle_hint = Some(false),
                _ => {} // unknown — оставляем None, считается malformed
            }
        } else if let Some(v) = line.strip_prefix("IdleSinceHint=") {
            idle_since = v.trim().parse().ok();
        }
    }

    match (idle_hint, idle_since) {
        // Не idle и hint=no → idle 0
        (Some(false), _) => Some(0),
        // Idle + есть anchor: считаем разницу
        (Some(true), Some(since_us)) if since_us > 0 => {
            let diff_us = now_us.saturating_sub(since_us);
            Some((diff_us / 1000) as i64)
        }
        // Idle, но since=0 — нет точного значения (например, idle с момента boot).
        // Чтобы не врать — None, пусть будет NULL в БД.
        _ => None,
    }
}

fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_idle_hint_no_returns_zero() {
        let text = "IdleHint=no\nIdleSinceHint=0\n";
        assert_eq!(parse_idle_ms(text, 1_000_000_000_000_000), Some(0));
    }

    #[test]
    fn parse_idle_with_anchor() {
        // since = 1 sec до now → idle 1000 ms
        let now_us: u64 = 1_500_000_000_000;
        let since_us: u64 = 1_500_000_000_000 - 1_000_000;
        let text = format!("IdleHint=yes\nIdleSinceHint={}\n", since_us);
        assert_eq!(parse_idle_ms(&text, now_us), Some(1000));
    }

    #[test]
    fn parse_idle_yes_but_since_zero_returns_none() {
        let text = "IdleHint=yes\nIdleSinceHint=0\n";
        assert_eq!(parse_idle_ms(text, 1_000), None);
    }

    #[test]
    fn parse_handles_extra_props() {
        // Реальный output может иметь extra строки — должны проигнорироваться
        let text = "Active=yes\nIdleHint=yes\nState=active\nIdleSinceHint=1000000\nClass=user\n";
        assert_eq!(parse_idle_ms(text, 1_500_000), Some(500));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert_eq!(parse_idle_ms("", 1_000), None);
    }

    #[test]
    fn parse_malformed_returns_none() {
        let text = "garbage\nIdleHint=maybe\nIdleSinceHint=foo\n";
        assert_eq!(parse_idle_ms(text, 1_000), None);
    }

    #[test]
    fn find_gui_session_picks_seat0_active() {
        let text = "\
388 1000 titan -     -     closing no -
514 1000 titan -     pts/1 active  no -
 c2  120 gdm   seat0 tty1  active  yes 2h 41min ago
 c4 1000 titan -     pts/3 active  no -";
        assert_eq!(parse_gui_session_from_list(text), Some("c2".into()));
    }

    #[test]
    fn find_gui_session_skips_inactive_seat() {
        let text = "\
 c1  120 gdm   seat0 tty1  closing yes -
 c2 1000 user  seat0 tty2  active  no -";
        assert_eq!(parse_gui_session_from_list(text), Some("c2".into()));
    }

    #[test]
    fn find_gui_session_returns_none_when_no_seat() {
        let text = "\
388 1000 titan -     -     closing no -
514 1000 titan -     pts/1 active  no -";
        assert_eq!(parse_gui_session_from_list(text), None);
    }

    #[test]
    fn idle_capped_by_saturating_sub() {
        // since в будущем (clock skew) → diff = 0 через saturating_sub
        let now_us = 1_000_000;
        let since_us = 2_000_000;
        let text = format!("IdleHint=yes\nIdleSinceHint={}\n", since_us);
        assert_eq!(parse_idle_ms(&text, now_us), Some(0));
    }
}
