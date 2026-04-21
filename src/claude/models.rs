use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// Обёртка для десериализации — serde выбирает вариант по полю "type"
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClaudeEvent {
    User(UserEvent),
    Assistant(AssistantEvent),
    Progress(ProgressEvent),
    System(SystemEvent),
    Summary(SummaryEvent),
    FileHistorySnapshot(FileHistorySnapshot),
    QueueOperation(QueueOperationEvent),
    Attachment(AttachmentEvent),
    #[serde(other)]
    Unknown,
}

impl ClaudeEvent {
    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        match self {
            ClaudeEvent::User(e) => Some(e.base.timestamp),
            ClaudeEvent::Assistant(e) => Some(e.base.timestamp),
            ClaudeEvent::Progress(e) => Some(e.base.timestamp),
            ClaudeEvent::System(e) => Some(e.base.timestamp),
            ClaudeEvent::Summary(e) => e.timestamp,
            ClaudeEvent::FileHistorySnapshot(e) => e.timestamp,
            ClaudeEvent::QueueOperation(e) => e.timestamp,
            ClaudeEvent::Attachment(e) => Some(e.base.timestamp),
            ClaudeEvent::Unknown => None,
        }
    }

    pub fn session_id(&self) -> Option<Uuid> {
        match self {
            ClaudeEvent::User(e) => Some(e.base.session_id),
            ClaudeEvent::Assistant(e) => Some(e.base.session_id),
            ClaudeEvent::Progress(e) => Some(e.base.session_id),
            ClaudeEvent::System(e) => Some(e.base.session_id),
            ClaudeEvent::Summary(_) => None,
            ClaudeEvent::FileHistorySnapshot(_) => None,
            ClaudeEvent::QueueOperation(e) => e.session_id,
            ClaudeEvent::Attachment(e) => Some(e.base.session_id),
            ClaudeEvent::Unknown => None,
        }
    }

    pub fn is_sidechain(&self) -> bool {
        match self {
            ClaudeEvent::User(e) => e.base.is_sidechain.unwrap_or(false),
            ClaudeEvent::Assistant(e) => e.base.is_sidechain.unwrap_or(false),
            ClaudeEvent::Progress(e) => e.base.is_sidechain.unwrap_or(false),
            ClaudeEvent::System(e) => e.base.is_sidechain.unwrap_or(false),
            ClaudeEvent::Attachment(e) => e.base.is_sidechain.unwrap_or(false),
            _ => false,
        }
    }
}

/// Базовые поля, общие для user/assistant/progress/system
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventBase {
    pub uuid: Uuid,
    pub timestamp: DateTime<Utc>,
    pub session_id: Uuid,
    pub parent_uuid: Option<Uuid>,
    pub is_sidechain: Option<bool>,
    pub cwd: Option<String>,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserEvent {
    #[serde(flatten)]
    pub base: EventBase,
    pub message: Option<UserMessage>,
    pub user_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    pub role: Option<String>,
    pub content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantEvent {
    #[serde(flatten)]
    pub base: EventBase,
    pub message: AssistantMessage,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub model: Option<String>,
    pub id: Option<String>,
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Thinking {
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    #[serde(flatten)]
    pub base: EventBase,
    pub data: Option<serde_json::Value>,
    pub tool_use_id: Option<String>,
    #[serde(alias = "toolUseID")]
    pub tool_use_id_alt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemEvent {
    #[serde(flatten)]
    pub base: EventBase,
    pub subtype: Option<String>,
    pub content: Option<String>,
    pub level: Option<String>,
    pub duration_ms: Option<u64>,
    pub compact_metadata: Option<CompactMetadata>,
}

/// Метаданные context compaction (из system event subtype: "compact_boundary")
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactMetadata {
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryEvent {
    pub timestamp: Option<DateTime<Utc>>,
    pub summary: Option<String>,
    pub leaf_uuid: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshot {
    pub timestamp: Option<DateTime<Utc>>,
    pub message_id: Option<Uuid>,
    pub is_snapshot_update: Option<bool>,
}

/// Attachment event (mcp_instructions_delta, deferred_tools_delta, skill_listing, command_permissions)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentEvent {
    #[serde(flatten)]
    pub base: EventBase,
    pub attachment: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueOperationEvent {
    pub timestamp: Option<DateTime<Utc>>,
    pub session_id: Option<Uuid>,
    pub operation: Option<String>,
    pub content: Option<String>,
}
