use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;

use super::client::TaskSummary;

/// SQLite кеш для результатов LLM классификации и суммаризации
pub struct ClassificationCache {
    db: Connection,
}

impl ClassificationCache {
    /// Открыть (или создать) кеш БД
    pub fn open() -> Result<Self> {
        let db_path = Self::db_path()?;

        // Создаём директорию если не существует
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Не удалось создать {}", parent.display()))?;
        }

        let db = Connection::open(&db_path)
            .with_context(|| format!("Не удалось открыть {}", db_path.display()))?;

        // Создаём таблицы если не существуют
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS turn_classifications (
                session_id TEXT NOT NULL,
                turn_timestamp TEXT NOT NULL,
                activity_label TEXT NOT NULL,
                confidence REAL,
                classified_at TEXT NOT NULL,
                model TEXT,
                PRIMARY KEY (session_id, turn_timestamp)
            );
            CREATE INDEX IF NOT EXISTS idx_session
                ON turn_classifications(session_id);

            CREATE TABLE IF NOT EXISTS task_summaries (
                task_id TEXT NOT NULL,
                turn_count INTEGER NOT NULL,
                last_turn_ts TEXT NOT NULL,
                summary TEXT NOT NULL,
                status TEXT,
                classified_at TEXT NOT NULL,
                model TEXT,
                PRIMARY KEY (task_id, turn_count, last_turn_ts)
            );

            CREATE TABLE IF NOT EXISTS chunk_summaries (
                task_id TEXT NOT NULL,
                level INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                chunk_hash TEXT NOT NULL,
                summary TEXT NOT NULL,
                status TEXT,
                classified_at TEXT NOT NULL,
                model TEXT,
                PRIMARY KEY (task_id, level, chunk_index)
            );

            CREATE TABLE IF NOT EXISTS manual_titles (
                task_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                set_at TEXT NOT NULL
            );",
        )?;

        // Миграция: добавляем колонку title в task_summaries и chunk_summaries
        Self::migrate_title_column(&db)?;

        Ok(ClassificationCache { db })
    }

    /// Миграция: добавить колонку title если отсутствует
    fn migrate_title_column(db: &Connection) -> Result<()> {
        // Проверяем наличие колонки title в task_summaries
        let has_title: bool = db
            .prepare("SELECT title FROM task_summaries LIMIT 0")
            .is_ok();

        if !has_title {
            db.execute_batch(
                "ALTER TABLE task_summaries ADD COLUMN title TEXT;
                 ALTER TABLE chunk_summaries ADD COLUMN title TEXT;",
            )
            .ok(); // Игнорируем ошибку если колонка уже есть
        }

        Ok(())
    }

    /// Путь к файлу кеша
    fn db_path() -> Result<PathBuf> {
        let cache_dir = dirs::cache_dir().context("Не удалось определить cache директорию")?;
        Ok(cache_dir
            .join("devboy-agent-usage")
            .join("classifications.db"))
    }

    // ==================== Turn Classifications ====================

    /// Получить классификацию для одного turn
    pub fn get(&self, session_id: &str, turn_ts: &DateTime<Utc>) -> Option<String> {
        let ts_str = turn_ts.to_rfc3339();
        self.db
            .query_row(
                "SELECT activity_label FROM turn_classifications
                 WHERE session_id = ?1 AND turn_timestamp = ?2",
                rusqlite::params![session_id, ts_str],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }

    /// Получить классификации для батча ключей
    pub fn get_batch(&self, keys: &[(String, DateTime<Utc>)]) -> HashMap<(String, String), String> {
        let mut result = HashMap::new();
        for (session_id, turn_ts) in keys {
            let ts_str = turn_ts.to_rfc3339();
            if let Ok(label) = self.db.query_row(
                "SELECT activity_label FROM turn_classifications
                 WHERE session_id = ?1 AND turn_timestamp = ?2",
                rusqlite::params![session_id, ts_str],
                |row| row.get::<_, String>(0),
            ) {
                result.insert((session_id.clone(), ts_str), label);
            }
        }
        result
    }

    /// Сохранить результат классификации
    pub fn store(
        &self,
        session_id: &str,
        turn_ts: &DateTime<Utc>,
        label: &str,
        model: &str,
    ) -> Result<()> {
        let ts_str = turn_ts.to_rfc3339();
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT OR REPLACE INTO turn_classifications
             (session_id, turn_timestamp, activity_label, classified_at, model)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, ts_str, label, now, model],
        )?;
        Ok(())
    }

    /// Сохранить батч результатов
    pub fn store_batch(
        &self,
        items: &[(String, DateTime<Utc>, String)],
        model: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let tx = self.db.unchecked_transaction()?;
        for (session_id, turn_ts, label) in items {
            let ts_str = turn_ts.to_rfc3339();
            tx.execute(
                "INSERT OR REPLACE INTO turn_classifications
                 (session_id, turn_timestamp, activity_label, classified_at, model)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![session_id, ts_str, label, now, model],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ==================== Task Summaries ====================

    /// Получить кешированную суммаризацию задачи
    ///
    /// Ключ: (task_id, turn_count, last_turn_ts) — если новые turns добавились,
    /// кеш инвалидируется автоматически
    pub fn get_summary(
        &self,
        task_id: &str,
        turn_count: usize,
        last_ts: &str,
    ) -> Option<TaskSummary> {
        self.db
            .query_row(
                "SELECT summary, status, title FROM task_summaries
                 WHERE task_id = ?1 AND turn_count = ?2 AND last_turn_ts = ?3",
                rusqlite::params![task_id, turn_count as i64, last_ts],
                |row| {
                    Ok(TaskSummary {
                        summary: row.get(0)?,
                        status: row.get(1)?,
                        title: row.get(2)?,
                    })
                },
            )
            .ok()
    }

    /// Сохранить суммаризацию задачи
    pub fn store_summary(
        &self,
        task_id: &str,
        turn_count: usize,
        last_ts: &str,
        summary: &TaskSummary,
        model: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT OR REPLACE INTO task_summaries
             (task_id, turn_count, last_turn_ts, summary, status, title, classified_at, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                task_id,
                turn_count as i64,
                last_ts,
                summary.summary,
                summary.status,
                summary.title,
                now,
                model,
            ],
        )?;
        Ok(())
    }

    // ==================== Chunk Summaries ====================

    /// Получить кешированную суммаризацию чанка
    ///
    /// Возвращает Some только если chunk_hash совпадает (содержимое не изменилось)
    pub fn get_chunk_summary(
        &self,
        task_id: &str,
        level: usize,
        chunk_index: usize,
        expected_hash: &str,
    ) -> Option<TaskSummary> {
        self.db
            .query_row(
                "SELECT summary, status, title FROM chunk_summaries
                 WHERE task_id = ?1 AND level = ?2 AND chunk_index = ?3 AND chunk_hash = ?4",
                rusqlite::params![task_id, level as i64, chunk_index as i64, expected_hash],
                |row| {
                    Ok(TaskSummary {
                        summary: row.get(0)?,
                        status: row.get(1)?,
                        title: row.get(2)?,
                    })
                },
            )
            .ok()
    }

    /// Получить все chunk summaries для задачи (level=0), отсортированные по chunk_index
    ///
    /// Возвращает Vec<(chunk_index, summary, status)>
    pub fn get_all_chunk_summaries(&self, task_id: &str) -> Vec<(usize, String, Option<String>)> {
        let mut stmt = match self.db.prepare(
            "SELECT chunk_index, summary, status FROM chunk_summaries
             WHERE task_id = ?1 AND level = 0
             ORDER BY chunk_index ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = stmt
            .query_map(rusqlite::params![task_id], |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .ok();

        match rows {
            Some(iter) => iter.filter_map(|r| r.ok()).collect(),
            None => Vec::new(),
        }
    }

    /// Сохранить суммаризацию чанка
    pub fn store_chunk_summary(
        &self,
        task_id: &str,
        level: usize,
        chunk_index: usize,
        chunk_hash: &str,
        summary: &TaskSummary,
        model: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT OR REPLACE INTO chunk_summaries
             (task_id, level, chunk_index, chunk_hash, summary, status, title, classified_at, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                task_id,
                level as i64,
                chunk_index as i64,
                chunk_hash,
                summary.summary,
                summary.status,
                summary.title,
                now,
                model,
            ],
        )?;
        Ok(())
    }

    // ==================== Manual Titles ====================

    /// Установить ручной заголовок задачи
    pub fn set_manual_title(&self, task_id: &str, title: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT OR REPLACE INTO manual_titles (task_id, title, set_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![task_id, title, now],
        )?;
        Ok(())
    }

    /// Получить ручной заголовок задачи
    pub fn get_manual_title(&self, task_id: &str) -> Option<String> {
        self.db
            .query_row(
                "SELECT title FROM manual_titles WHERE task_id = ?1",
                rusqlite::params![task_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
    }

    /// Получить ручные заголовки для списка задач
    pub fn get_manual_titles(&self, task_ids: &[String]) -> HashMap<String, String> {
        let mut result = HashMap::new();
        for task_id in task_ids {
            if let Some(title) = self.get_manual_title(task_id) {
                result.insert(task_id.clone(), title);
            }
        }
        result
    }

    // ==================== Reclassify ====================

    /// Удалить суммаризации для указанных задач (task_summaries + chunk_summaries)
    ///
    /// Возвращает количество удалённых записей
    pub fn clear_summaries_for_tasks(&self, task_ids: &[String]) -> Result<usize> {
        let tx = self.db.unchecked_transaction()?;
        let mut total_deleted = 0usize;

        for task_id in task_ids {
            let d1 = tx.execute(
                "DELETE FROM task_summaries WHERE task_id = ?1",
                rusqlite::params![task_id],
            )?;
            let d2 = tx.execute(
                "DELETE FROM chunk_summaries WHERE task_id = ?1",
                rusqlite::params![task_id],
            )?;
            total_deleted += d1 + d2;
        }

        tx.commit()?;
        Ok(total_deleted)
    }
}
