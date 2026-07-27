//! Internal tool calls and parent message tracking.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::error::{ToolFailure, ToolFailureKind};
use crate::limits::{INTERNAL_TOOL_STEPS_MAX, TOOL_CALLS_PER_MESSAGE_MAX};

pub const EXACT_TOOL_NAMES: [&str; 5] = [
    "mcp__syntax-checker__validate",
    "mcp__knowledge-hub__Search_Documentation",
    "mcp__knowledge-hub__Search_ITS",
    "mcp__knowledge-hub__Fetch_ITS",
    "mcp__knowledge-hub__Diff_Documentation_Versions",
];

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    id: String,
    name: String,
    arguments: Map<String, Value>,
}

impl ToolCall {
    #[must_use]
    pub fn new(id: String, name: String, arguments: Map<String, Value>) -> Self {
        Self {
            id,
            name,
            arguments,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }
}

#[derive(Debug, Default)]
pub struct StepCounter {
    completed: usize,
}

impl StepCounter {
    pub fn begin_step(&mut self) -> Result<(), ToolFailure> {
        if self.completed >= INTERNAL_TOOL_STEPS_MAX {
            return Err(ToolFailure::new(
                ToolFailureKind::LimitExceeded,
                "the internal tool step limit was exceeded",
                false,
            ));
        }
        self.completed += 1;
        Ok(())
    }
}

pub fn build_standard_results(calls: &[ToolCall]) -> Result<Vec<Value>, ToolFailure> {
    validate_tool_calls(calls)?;
    Ok(calls
        .iter()
        .map(|call| {
            if is_supported_standard_tool(call.name()) {
                json!({
                    "status": "accepted",
                    "tool_call_id": call.id(),
                    "content": null
                })
            } else {
                json!({
                    "status": "rejected",
                    "tool_call_id": call.id(),
                    "content": {"error": "unsupported internal tool"}
                })
            }
        })
        .collect())
}

pub fn require_exact_tool<'a>(
    calls: &'a [ToolCall],
    expected_name: &str,
    expected_arguments: &Map<String, Value>,
) -> Result<&'a ToolCall, ToolFailure> {
    validate_tool_calls(calls)?;
    if !EXACT_TOOL_NAMES.contains(&expected_name)
        || calls.len() != 1
        || calls[0].name() != expected_name
        || calls[0].arguments() != expected_arguments
    {
        return Err(ToolFailure::new(
            ToolFailureKind::UpstreamProtocol,
            "the exact internal tool response is incompatible",
            false,
        ));
    }
    Ok(&calls[0])
}

pub fn validate_tool_calls(calls: &[ToolCall]) -> Result<(), ToolFailure> {
    if calls.len() > TOOL_CALLS_PER_MESSAGE_MAX {
        return Err(ToolFailure::new(
            ToolFailureKind::LimitExceeded,
            "the internal tool call count exceeds the limit",
            false,
        ));
    }

    let mut identifiers = HashSet::with_capacity(calls.len());
    for call in calls {
        if call.id().is_empty()
            || call.name().is_empty()
            || !identifiers.insert(call.id().to_owned())
        {
            return Err(ToolFailure::new(
                ToolFailureKind::UpstreamProtocol,
                "the internal tool call array is invalid",
                false,
            ));
        }
    }
    Ok(())
}

fn is_supported_standard_tool(name: &str) -> bool {
    name == "TaskResult" || EXACT_TOOL_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{
        EXACT_TOOL_NAMES, StepCounter, ToolCall, build_standard_results, require_exact_tool,
    };
    use crate::error::ToolFailureKind;
    use crate::limits::{INTERNAL_TOOL_STEPS_MAX, TOOL_CALLS_PER_MESSAGE_MAX};

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall::new(id.to_owned(), name.to_owned(), serde_json::Map::new())
    }

    #[test]
    fn standard_results_preserve_every_call_and_report_unknown_names() {
        let calls = vec![
            call("first", "TaskResult"),
            call("second", "unsupported"),
            call("third", EXACT_TOOL_NAMES[0]),
        ];

        let results = build_standard_results(&calls).unwrap();

        assert_eq!(
            results,
            vec![
                json!({"status":"accepted","tool_call_id":"first","content":null}),
                json!({
                    "status":"rejected",
                    "tool_call_id":"second",
                    "content":{"error":"unsupported internal tool"}
                }),
                json!({"status":"accepted","tool_call_id":"third","content":null}),
            ]
        );
    }

    #[test]
    fn direct_mode_requires_exactly_one_expected_pinned_tool() {
        let expected_arguments = Map::new();
        for expected in EXACT_TOOL_NAMES {
            let calls = vec![call("call-1", expected)];
            let selected = require_exact_tool(&calls, expected, &expected_arguments).unwrap();
            assert_eq!(selected.id(), "call-1");
        }

        for invalid in [
            Vec::new(),
            vec![
                call("a", EXACT_TOOL_NAMES[0]),
                call("b", EXACT_TOOL_NAMES[0]),
            ],
            vec![call("a", EXACT_TOOL_NAMES[1])],
        ] {
            let error =
                require_exact_tool(&invalid, EXACT_TOOL_NAMES[0], &expected_arguments).unwrap_err();
            assert_eq!(error.kind(), ToolFailureKind::UpstreamProtocol);
        }
    }

    #[test]
    fn direct_mode_requires_structurally_equal_arguments() {
        let Value::Object(expected_arguments) = json!({
            "query": "test",
            "extended": false
        }) else {
            unreachable!("object literal")
        };
        let Value::Object(reordered_arguments) = json!({
            "extended": false,
            "query": "test"
        }) else {
            unreachable!("object literal")
        };
        let Value::Object(different_arguments) = json!({
            "query": "other",
            "extended": false
        }) else {
            unreachable!("object literal")
        };

        let matching = ToolCall::new(
            "call-1".to_owned(),
            EXACT_TOOL_NAMES[2].to_owned(),
            reordered_arguments,
        );
        require_exact_tool(&[matching], EXACT_TOOL_NAMES[2], &expected_arguments)
            .expect("equal JSON objects are accepted");

        let different = ToolCall::new(
            "call-1".to_owned(),
            EXACT_TOOL_NAMES[2].to_owned(),
            different_arguments,
        );
        let error =
            require_exact_tool(&[different], EXACT_TOOL_NAMES[2], &expected_arguments).unwrap_err();
        assert_eq!(error.kind(), ToolFailureKind::UpstreamProtocol);
    }

    #[test]
    fn step_counter_rejects_an_eleventh_internal_tool_round() {
        let mut counter = StepCounter::default();
        for _ in 0..INTERNAL_TOOL_STEPS_MAX {
            counter.begin_step().unwrap();
        }

        let error = counter.begin_step().unwrap_err();
        assert_eq!(error.kind(), ToolFailureKind::LimitExceeded);
    }

    #[test]
    fn array_limit_is_checked_before_results_are_built() {
        let boundary_calls = (0..TOOL_CALLS_PER_MESSAGE_MAX)
            .map(|index| call(&format!("call-{index}"), "TaskResult"))
            .collect::<Vec<_>>();
        assert_eq!(
            build_standard_results(&boundary_calls)
                .expect("sixteen calls are accepted")
                .len(),
            TOOL_CALLS_PER_MESSAGE_MAX
        );

        let calls = (0..=TOOL_CALLS_PER_MESSAGE_MAX)
            .map(|index| call(&format!("call-{index}"), "TaskResult"))
            .collect::<Vec<_>>();

        let error = build_standard_results(&calls).unwrap_err();
        assert_eq!(error.kind(), ToolFailureKind::LimitExceeded);
    }

    #[test]
    fn arguments_remain_structured_json_objects() {
        let mut arguments = serde_json::Map::new();
        arguments.insert("query".to_owned(), Value::String("значение".to_owned()));
        let call = ToolCall::new("id".to_owned(), EXACT_TOOL_NAMES[1].to_owned(), arguments);

        assert_eq!(call.arguments()["query"], "значение");
    }
}
