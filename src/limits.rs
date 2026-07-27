//! Fixed limits for the first release.

use std::time::Duration;

pub const TOKEN_MAX_BYTES: usize = 8 * 1024;
pub const MCP_FRAME_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const TOOL_INPUT_MIN_DEFAULT: usize = 4;
pub const TOOL_INPUT_MAX_DEFAULT: usize = 100_000;
pub const TOOL_INPUT_MAX_HARD: usize = 100_000;
pub const UPSTREAM_REQUEST_MAX_BYTES: usize = 1024 * 1024;
pub const CONVERSATION_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const SSE_EVENT_MAX_BYTES: usize = 1024 * 1024;
pub const SSE_STREAM_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const ERROR_BODY_MAX_BYTES: usize = 16 * 1024;
pub const TOOL_CALLS_PER_MESSAGE_MAX: usize = 16;
pub const INTERNAL_TOOL_STEPS_MAX: usize = 10;
pub const WAITING_CALLS_MAX: usize = 8;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_mins(2);
pub const SSE_EVENT_TIMEOUT: Duration = Duration::from_mins(1);
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(30);
pub const SAFE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
pub const QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
pub const TOOL_CALL_TIMEOUT: Duration = Duration::from_mins(5);
pub const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CONNECT_TIMEOUT, CONVERSATION_RESPONSE_MAX_BYTES, ERROR_BODY_MAX_BYTES,
        HTTP_OPERATION_TIMEOUT, INTERNAL_TOOL_STEPS_MAX, MCP_FRAME_MAX_BYTES, QUEUE_WAIT_TIMEOUT,
        RETRY_AFTER_MAX, SAFE_RETRY_DELAYS, SHUTDOWN_GRACE_PERIOD, SSE_EVENT_MAX_BYTES,
        SSE_EVENT_TIMEOUT, SSE_STREAM_MAX_BYTES, TOKEN_MAX_BYTES, TOOL_CALL_TIMEOUT,
        TOOL_CALLS_PER_MESSAGE_MAX, TOOL_INPUT_MAX_DEFAULT, TOOL_INPUT_MAX_HARD,
        TOOL_INPUT_MIN_DEFAULT, UPSTREAM_REQUEST_MAX_BYTES, WAITING_CALLS_MAX,
    };

    #[test]
    fn fixed_resource_and_time_boundaries_match_the_release_contract() {
        assert_eq!(TOKEN_MAX_BYTES, 8 * 1024);
        assert_eq!(MCP_FRAME_MAX_BYTES, 2 * 1024 * 1024);
        assert_eq!(TOOL_INPUT_MIN_DEFAULT, 4);
        assert_eq!(TOOL_INPUT_MAX_DEFAULT, 100_000);
        assert_eq!(TOOL_INPUT_MAX_HARD, 100_000);
        assert_eq!(UPSTREAM_REQUEST_MAX_BYTES, 1024 * 1024);
        assert_eq!(CONVERSATION_RESPONSE_MAX_BYTES, 1024 * 1024);
        assert_eq!(SSE_EVENT_MAX_BYTES, 1024 * 1024);
        assert_eq!(SSE_STREAM_MAX_BYTES, 8 * 1024 * 1024);
        assert_eq!(ERROR_BODY_MAX_BYTES, 16 * 1024);
        assert_eq!(TOOL_CALLS_PER_MESSAGE_MAX, 16);
        assert_eq!(INTERNAL_TOOL_STEPS_MAX, 10);
        assert_eq!(WAITING_CALLS_MAX, 8);
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(HTTP_OPERATION_TIMEOUT, Duration::from_mins(2));
        assert_eq!(SSE_EVENT_TIMEOUT, Duration::from_mins(1));
        assert_eq!(RETRY_AFTER_MAX, Duration::from_secs(30));
        assert_eq!(
            SAFE_RETRY_DELAYS,
            [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ]
        );
        assert_eq!(QUEUE_WAIT_TIMEOUT, Duration::from_secs(30));
        assert_eq!(TOOL_CALL_TIMEOUT, Duration::from_mins(5));
        assert_eq!(SHUTDOWN_GRACE_PERIOD, Duration::from_secs(5));
    }
}
