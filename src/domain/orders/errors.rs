use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    OrderAlreadyExists,
    OrderNotFound,
    InvalidStateTransition,
    InvalidQuantity,
    InvalidPrice,
    CommandNotAllowed,
    InvalidExecutionReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandRejection {
    pub code: RejectionCode,
    pub message: String,
}

impl CommandRejection {
    pub fn new(code: RejectionCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
