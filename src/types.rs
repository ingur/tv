//! Message types for IPC communication between daemon, client, and session.

use serde::{Deserialize, Serialize};

/// Server sends this to any new connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    /// Generated 4-char hex ID.
    pub id: String,
}

/// Response to AuthRequest - identifies as client or session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthResponse {
    /// Client: acks id and sends request.
    Client {
        id: String,
        pid: u32,
        request: Request,
    },
    /// Session: acks or overrides id.
    Session {
        id: String,
        pid: u32,
        tag: Option<String>,
        visible: bool,
    },
}

/// Session selector for requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Selector {
    Id(String),
    Tag(String),
}

/// Request from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Request {
    Stop,

    Show {
        selector: Option<Selector>,
    },
    Hide {
        selector: Option<Selector>,
    },
    Tag {
        selector: Option<Selector>,
        new_tag: Option<String>,
        delete: bool,
    },
    Clear {
        selector: Option<Selector>,
    },

    List {
        selector: Option<Selector>,
        all: bool,
    },
    Peek {
        selector: Option<Selector>,
    },
    History {
        selector: Option<Selector>,
        count: Option<usize>,
        offset: Option<usize>,
    },
    Exec {
        selector: Option<Selector>,
        /// Pre-parsed key arguments as raw strings (validated at CLI time).
        keys: Vec<String>,
        /// Raw text from stdin, injected where {stdin} appears in keys.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        stdin: Option<String>,
    },
}

/// Response from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or(serde_json::Value::Null)),
            error: None,
            code: None,
        }
    }

    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
            code: None,
        }
    }

    pub fn err(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
            code: Some(code),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    SessionNotFound,
    AmbiguousSelector,
    PermissionDenied,
    AuthFailed,
    SessionHidden,
    Internal,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCode::SessionNotFound => write!(f, "session not found"),
            ErrorCode::AmbiguousSelector => write!(f, "selector matches multiple sessions"),
            ErrorCode::PermissionDenied => write!(f, "permission denied"),
            ErrorCode::AuthFailed => write!(f, "authentication failed"),
            ErrorCode::SessionHidden => write!(f, "session is hidden"),
            ErrorCode::Internal => write!(f, "internal server error"),
        }
    }
}

/// Request forwarded from daemon to session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    /// Unique request ID for matching responses.
    pub id: u64,
    /// Source session ID (who's making the request).
    pub source_id: String,
    /// Source session tag (if any).
    pub source_tag: Option<String>,
    /// The actual request.
    pub request: Request,
}

/// Message from daemon to session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMessage {
    /// Forward a client request.
    Request(SessionRequest),
    /// Cancel a pending request (client disconnected).
    Cancel { id: u64 },
}

/// Message from session to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionMessage {
    /// Response to a SessionRequest.
    Response { id: u64, result: Response },
    /// Unsolicited status push.
    Status { update: StatusUpdate },
}

/// Dynamic terminal state (pushed by session on PTY activity, debounced).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub cwd: Option<String>,
    pub in_alt_screen: bool,
}

/// Full session information for tv list command.
/// Server composes this from auth data + status updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    // From auth
    pub id: String,
    pub tag: Option<String>,
    pub pid: u32,
    pub visible: bool,
    // From status updates
    pub cwd: Option<String>,
    pub in_alt_screen: bool,
    // Timestamps (epoch seconds, set by daemon)
    pub created_at: u64,
    pub last_activity: u64,
}

impl SessionInfo {
    /// Create a new SessionInfo from auth data with default status.
    pub fn new(id: String, pid: u32, tag: Option<String>, visible: bool) -> Self {
        let now = epoch_secs();
        Self {
            id,
            tag,
            pid,
            visible,
            cwd: None,
            in_alt_screen: false,
            created_at: now,
            last_activity: now,
        }
    }

    /// Update from a StatusUpdate.
    pub fn update_status(&mut self, update: &StatusUpdate) {
        self.cwd = update.cwd.clone();
        self.in_alt_screen = update.in_alt_screen;
        self.last_activity = epoch_secs();
    }
}

/// Current time as Unix epoch seconds.
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// === Response Payload Types ===

/// Response payload for text content (history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    pub content: String,
}

/// Response payload for peek command — includes screen content and cursor info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekResponse {
    pub content: String,
    pub in_alt_screen: bool,
    pub rows: usize,
    pub cols: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

/// Response payload for exec command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResponse {
    pub id: String,
}

/// Response payload for session state changes (tag, show, hide, clear).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStateResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}
