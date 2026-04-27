use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorData {
    pub tool: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument: Option<String>,
    pub constraint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<serde_json::Value>,
    pub next_action: String,
}

pub mod codes {
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

pub type Result<T> = std::result::Result<T, SutraError>;

#[derive(Debug, Error)]
pub enum SutraError {
    #[error("missing config: {0}")]
    MissingConfig(String),

    #[error("[{tool}] invalid argument `{argument}`: {constraint}")]
    InvalidArgument {
        tool: &'static str,
        argument: &'static str,
        constraint: String,
        received: Option<String>,
        next_action: String,
    },

    #[error("[{tool}] not found: {kind}")]
    NotFound {
        tool: &'static str,
        kind: String,
        next_action: String,
    },

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal: {0}")]
    Internal(String),
}

impl SutraError {
    pub fn code(&self) -> i32 {
        match self {
            Self::MissingConfig(_)
            | Self::InvalidArgument { .. }
            | Self::NotFound { .. } => codes::INVALID_PARAMS,
            Self::Db(_) | Self::Parse(_) | Self::Io(_) | Self::Internal(_) => {
                codes::INTERNAL_ERROR
            }
        }
    }

    pub fn message(&self) -> String {
        self.to_string()
    }

    pub fn data(&self) -> ErrorData {
        match self {
            Self::MissingConfig(name) => ErrorData {
                tool: "startup",
                argument: Some(name.clone()),
                constraint: format!("environment variable `{name}` must be set"),
                received: None,
                next_action: format!("Set {name} and restart."),
            },
            Self::InvalidArgument {
                tool,
                argument,
                constraint,
                received,
                next_action,
            } => ErrorData {
                tool,
                argument: Some((*argument).to_string()),
                constraint: constraint.clone(),
                received: received
                    .as_deref()
                    .map(|s| serde_json::Value::String(s.to_string())),
                next_action: next_action.clone(),
            },
            Self::NotFound {
                tool,
                kind,
                next_action,
            } => ErrorData {
                tool,
                argument: None,
                constraint: format!("{kind} exists"),
                received: None,
                next_action: next_action.clone(),
            },
            Self::Db(e) => ErrorData {
                tool: "database",
                argument: None,
                constraint: "database query succeeds".to_string(),
                received: Some(serde_json::json!({ "message": e.to_string() })),
                next_action: db_next_action(e),
            },
            Self::Parse(msg) => ErrorData {
                tool: "parser",
                argument: None,
                constraint: "source file parses without fatal errors".to_string(),
                received: Some(serde_json::json!({ "message": msg })),
                next_action: "Check the source file for syntax errors.".to_string(),
            },
            Self::Io(e) => ErrorData {
                tool: "io",
                argument: None,
                constraint: "filesystem operation succeeds".to_string(),
                received: Some(serde_json::json!({ "message": e.to_string() })),
                next_action: "Check file permissions and paths.".to_string(),
            },
            Self::Internal(msg) => ErrorData {
                tool: "server",
                argument: None,
                constraint: "server completes the request without an internal fault"
                    .to_string(),
                received: Some(serde_json::json!({ "message": msg })),
                next_action: "Report this as a bug; include server logs.".to_string(),
            },
        }
    }
}

fn db_next_action(e: &rusqlite::Error) -> String {
    match e {
        rusqlite::Error::SqliteFailure(_, _) => {
            "The database rejected the query. Inspect the message, correct the input \
             or schema, and retry."
                .to_string()
        }
        rusqlite::Error::QueryReturnedNoRows => {
            "The expected row was absent. Verify the id or name, then retry.".to_string()
        }
        _ => {
            "Retry the request. If the error repeats, check server logs and report as a \
             bug."
                .to_string()
        }
    }
}
