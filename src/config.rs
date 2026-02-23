use anyhow::{Context, Result};
use std::path::PathBuf;

/// Пути к источникам данных
pub struct Config {
    /// Директория с JSONL логами Claude Code
    pub claude_projects_dir: PathBuf,
    /// Путь к SQLite БД ActivityWatch
    pub activitywatch_db_path: PathBuf,
}

impl Config {
    pub fn detect() -> Result<Self> {
        let home = dirs::home_dir().context("Не удалось определить домашнюю директорию")?;

        let claude_projects_dir = home.join(".claude").join("projects");
        if !claude_projects_dir.exists() {
            anyhow::bail!(
                "Директория Claude Code логов не найдена: {}",
                claude_projects_dir.display()
            );
        }

        let activitywatch_db_path = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/activitywatch/aw-server/peewee-sqlite.v2.db")
        } else {
            home.join(".local/share/activitywatch/aw-server/peewee-sqlite.v2.db")
        };

        Ok(Config {
            claude_projects_dir,
            activitywatch_db_path,
        })
    }

    pub fn has_activitywatch(&self) -> bool {
        self.activitywatch_db_path.exists()
    }
}
