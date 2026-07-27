//! Safe tool and protocol error boundaries.

use std::error::Error;
use std::fmt;

use rmcp::{
    ErrorData as McpError,
    model::{ErrorCode, ErrorData},
};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("exactly one of ONEC_AI_TOKEN or ONEC_AI_TOKEN_FILE must be set")]
    TokenSourceCount,
    #[error("the token is invalid")]
    InvalidToken,
    #[error("the token file path must be absolute and local")]
    InvalidTokenFilePath,
    #[error("the token file must be a regular file and not a link")]
    InvalidTokenFileKind,
    #[error("the token file exceeds the 8192-byte limit")]
    TokenFileTooLarge,
    #[error("the token file could not be read")]
    TokenFileUnreadable,
    #[error("{name} is not valid UTF-8")]
    NonUnicodeValue { name: &'static str },
    #[error("{name} must be an integer")]
    InvalidInteger { name: &'static str },
    #[error("{name} has an invalid value")]
    InvalidSetting { name: &'static str },
    #[error("MCP_TOOL_INPUT_MIN_LENGTH must not exceed MCP_TOOL_INPUT_MAX_LENGTH")]
    InvertedToolInputLimits,
    #[error("RUST_LOG is not a valid tracing filter")]
    InvalidLogFilter,
    #[error("ONEC_AI_BASE_URL is not supported")]
    UnsupportedBaseUrl,
    #[error("technical logging could not be initialized")]
    TracingInitialization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolFailureKind {
    InvalidArguments,
    UpstreamTransport,
    UpstreamStatus,
    UpstreamProtocol,
    LimitExceeded,
    Timeout,
    Internal,
}

pub struct ToolFailure {
    kind: ToolFailureKind,
    safe_message: &'static str,
    ambiguous_outcome: bool,
    cause: Option<Box<dyn Error + Send + Sync>>,
}

impl ToolFailure {
    #[must_use]
    pub fn new(kind: ToolFailureKind, safe_message: &'static str, ambiguous_outcome: bool) -> Self {
        Self {
            kind,
            safe_message,
            ambiguous_outcome,
            cause: None,
        }
    }

    pub fn with_cause<E>(
        kind: ToolFailureKind,
        safe_message: &'static str,
        ambiguous_outcome: bool,
        cause: E,
    ) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            kind,
            safe_message,
            ambiguous_outcome,
            cause: Some(Box::new(cause)),
        }
    }

    #[must_use]
    pub fn kind(&self) -> ToolFailureKind {
        self.kind
    }

    #[must_use]
    pub fn ambiguous_outcome(&self) -> bool {
        self.ambiguous_outcome
    }
}

impl fmt::Debug for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolFailure")
            .field("kind", &self.kind)
            .field("safe_message", &self.safe_message)
            .field("ambiguous_outcome", &self.ambiguous_outcome)
            .field(
                "cause",
                &self.cause.as_ref().map(|_| "[REDACTED LOWER CAUSE]"),
            )
            .finish()
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.safe_message)
    }
}

impl Error for ToolFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the protocol boundary keeps the complete fixed failure taxonomy"
    )
)]
pub enum ProtocolFailureKind {
    UnknownTool,
    InvalidRequest,
    InvalidParams,
    ResponseTooLarge,
    Internal,
}

#[derive(Debug, Error)]
#[error("{safe_message}")]
pub struct ProtocolFailure {
    kind: ProtocolFailureKind,
    safe_message: &'static str,
}

impl ProtocolFailure {
    #[must_use]
    pub fn new(kind: ProtocolFailureKind, safe_message: &'static str) -> Self {
        Self { kind, safe_message }
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "kind inspection is used by boundary tests")
    )]
    pub fn kind(&self) -> ProtocolFailureKind {
        self.kind
    }

    #[must_use]
    pub fn into_mcp_error(self) -> McpError {
        let code = match self.kind {
            ProtocolFailureKind::UnknownTool => ErrorCode::METHOD_NOT_FOUND,
            ProtocolFailureKind::InvalidRequest => ErrorCode::INVALID_REQUEST,
            ProtocolFailureKind::InvalidParams => ErrorCode::INVALID_PARAMS,
            ProtocolFailureKind::ResponseTooLarge | ProtocolFailureKind::Internal => {
                ErrorCode::INTERNAL_ERROR
            }
        };
        ErrorData::new(code, self.safe_message, None)
    }
}

#[derive(Debug, Error)]
pub enum ClientCallError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Failure(#[from] ToolFailure),
}

#[cfg(test)]
mod tests {
    use rmcp::model::ErrorCode;

    use super::{ProtocolFailure, ProtocolFailureKind, ToolFailure, ToolFailureKind};

    #[test]
    fn lower_level_failure_cause_is_redacted() {
        let secret = "Authorization: DO_NOT_LEAK";
        let failure = ToolFailure::with_cause(
            ToolFailureKind::UpstreamTransport,
            "the request failed safely",
            true,
            std::io::Error::other(secret),
        );

        let rendered = format!("{failure:?} {failure}");
        assert!(rendered.contains("the request failed safely"));
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("DO_NOT_LEAK"));
    }

    #[test]
    fn protocol_failures_map_to_json_rpc_errors_only() {
        let cases = [
            (
                ProtocolFailureKind::UnknownTool,
                ErrorCode::METHOD_NOT_FOUND,
            ),
            (
                ProtocolFailureKind::InvalidRequest,
                ErrorCode::INVALID_REQUEST,
            ),
            (
                ProtocolFailureKind::InvalidParams,
                ErrorCode::INVALID_PARAMS,
            ),
            (
                ProtocolFailureKind::ResponseTooLarge,
                ErrorCode::INTERNAL_ERROR,
            ),
            (ProtocolFailureKind::Internal, ErrorCode::INTERNAL_ERROR),
        ];

        for (kind, expected_code) in cases {
            let failure = ProtocolFailure::new(kind, "safe protocol error");
            assert_eq!(failure.kind(), kind);
            assert_eq!(failure.into_mcp_error().code, expected_code);
        }
    }
}
