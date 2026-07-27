//! Paths, headers, and the upstream compatibility contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const PRODUCTION_BASE_URL: &str = "https://code.1c.ai";
pub const CREATE_CONVERSATION_PATH: &str = "/chat_api/v1/conversations/";
pub const MESSAGE_PATH_SUFFIX: &str = "messages";

#[derive(Debug, Error)]
#[error("the upstream response violates the pinned compatibility contract")]
pub struct CompatibilityError;

#[derive(Debug, Serialize)]
pub struct CreateConversationRequest<'a> {
    is_chat: bool,
    skill_name: &'static str,
    ui_language: &'a str,
    programming_language: &'a str,
}

impl<'a> CreateConversationRequest<'a> {
    #[must_use]
    pub fn new(ui_language: &'a str, programming_language: &'a str) -> Self {
        Self {
            is_chat: true,
            skill_name: "custom",
            ui_language,
            programming_language,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateConversationResponse {
    uuid: String,
    root_message_uuid: Option<String>,
}

impl CreateConversationResponse {
    pub fn parse(bytes: &[u8]) -> Result<Self, CompatibilityError> {
        let response: Self = serde_json::from_slice(bytes).map_err(|_| CompatibilityError)?;
        if response.uuid.is_empty()
            || response
                .root_message_uuid
                .as_ref()
                .is_some_and(String::is_empty)
        {
            return Err(CompatibilityError);
        }
        Ok(response)
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "identifier accessors support compatibility tests")
    )]
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "identifier accessors support compatibility tests")
    )]
    pub fn root_message_uuid(&self) -> Option<&str> {
        self.root_message_uuid.as_deref()
    }

    pub fn into_parts(self) -> (String, Option<String>) {
        (self.uuid, self.root_message_uuid)
    }
}

#[derive(Debug, Serialize)]
pub struct UserMessageRequest<'a> {
    role: &'static str,
    content: UserMessageContent<'a>,
    parent_uuid: Option<&'a str>,
}

impl<'a> UserMessageRequest<'a> {
    #[must_use]
    pub fn new(instruction: &'a str, tools: &'a [Value], parent_uuid: Option<&'a str>) -> Self {
        Self {
            role: "user",
            content: UserMessageContent {
                content: UserInstruction { instruction },
                tools,
            },
            parent_uuid,
        }
    }
}

#[derive(Debug, Serialize)]
struct UserMessageContent<'a> {
    content: UserInstruction<'a>,
    tools: &'a [Value],
}

#[derive(Debug, Serialize)]
struct UserInstruction<'a> {
    instruction: &'a str,
}

#[derive(Debug, Serialize)]
pub struct ToolMessageRequest<'a> {
    role: &'static str,
    content: &'a [Value],
    parent_uuid: &'a str,
}

impl<'a> ToolMessageRequest<'a> {
    #[must_use]
    pub fn new(content: &'a [Value], parent_uuid: &'a str) -> Self {
        Self {
            role: "tool",
            content,
            parent_uuid,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CreateConversationRequest, CreateConversationResponse, ToolMessageRequest,
        UserMessageRequest,
    };

    #[test]
    fn conversation_request_has_the_pinned_shape() {
        let request = CreateConversationRequest::new("russian", "bsl");

        assert_eq!(
            serde_json::to_value(request).expect("request must serialize"),
            json!({
                "is_chat": true,
                "skill_name": "custom",
                "ui_language": "russian",
                "programming_language": "bsl"
            })
        );
    }

    #[test]
    fn conversation_response_requires_string_identifiers_and_ignores_unknown_fields() {
        let parsed = CreateConversationResponse::parse(
            br#"{"uuid":"conversation-1","root_message_uuid":"root-1","future":true}"#,
        )
        .expect("known fields are valid");

        assert_eq!(parsed.uuid(), "conversation-1");
        assert_eq!(parsed.root_message_uuid(), Some("root-1"));

        for invalid in [
            br"{}".as_slice(),
            br#"{"uuid":1}"#.as_slice(),
            br#"{"uuid":""}"#.as_slice(),
            br#"{"uuid":"conversation-1","root_message_uuid":1}"#.as_slice(),
            br#"{"uuid":"conversation-1","root_message_uuid":""}"#.as_slice(),
        ] {
            assert!(
                CreateConversationResponse::parse(invalid).is_err(),
                "{:?} must violate the pinned contract",
                String::from_utf8_lossy(invalid)
            );
        }
    }

    #[test]
    fn user_message_contains_instruction_tools_and_parent() {
        let tools = vec![json!({"name": "tool-1"})];
        let request = UserMessageRequest::new("explain this", &tools, Some("root-message"));

        assert_eq!(
            serde_json::to_value(request).expect("request must serialize"),
            json!({
                "role": "user",
                "content": {
                    "content": {
                        "instruction": "explain this"
                    },
                    "tools": [{"name": "tool-1"}]
                },
                "parent_uuid": "root-message"
            })
        );
    }

    #[test]
    fn absent_parent_is_serialized_as_json_null() {
        let request = UserMessageRequest::new("question", &[], None);
        let value = serde_json::to_value(request).expect("request must serialize");

        assert_eq!(value["parent_uuid"], serde_json::Value::Null);
    }

    #[test]
    fn tool_message_preserves_all_results_and_assistant_parent() {
        let results = vec![
            json!({"tool_call_id": "call-1", "content": "first"}),
            json!({"tool_call_id": "call-2", "content": "second"}),
        ];
        let request = ToolMessageRequest::new(&results, "assistant-message");

        assert_eq!(
            serde_json::to_value(request).expect("request must serialize"),
            json!({
                "role": "tool",
                "content": results,
                "parent_uuid": "assistant-message"
            })
        );
    }
}
