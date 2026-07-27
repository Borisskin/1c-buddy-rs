use std::fmt::Write as _;

use icu_normalizer::ComposingNormalizerBorrowed;
use icu_properties::{CodePointMapData, props::GeneralCategory};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolTextResult {
    pub(crate) response_markdown: String,
    pub(crate) response_details: Vec<Value>,
}

pub(crate) struct DirectToolText<'a> {
    pub(crate) tool_results: &'a [ToolTextResult],
    pub(crate) full_text: &'a str,
    pub(crate) tool_followups: &'a [String],
    pub(crate) final_text: &'a str,
}

pub(crate) fn sanitize_text(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let normalized = ComposingNormalizerBorrowed::new_nfkc().normalize(input);
    let categories = CodePointMapData::<GeneralCategory>::new();
    normalized
        .chars()
        .filter(|character| {
            matches!(*character, '\n' | '\r' | '\t')
                || !matches!(
                    categories.get(*character),
                    GeneralCategory::Control | GeneralCategory::Format
                )
        })
        .collect()
}

pub(crate) fn extract_tool_text(input: &DirectToolText<'_>) -> String {
    let mut blocks = Vec::new();
    for result in input.tool_results {
        let markdown = result.response_markdown.trim();
        if !markdown.is_empty() && markdown != "✓ Инструмент выполнен" {
            blocks.push(markdown.to_owned());
        }
        for detail in &result.response_details {
            if python_truthy(detail) {
                blocks.push(python_str(detail));
            }
        }
    }

    let full_text = input.full_text.trim();
    if full_text.is_empty() {
        let followups = input
            .tool_followups
            .iter()
            .map(|text| text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        if let Some(followup) = followups.last() {
            blocks.push((*followup).to_owned());
        } else {
            let final_text = input.final_text.trim();
            if !final_text.is_empty() {
                blocks.push(final_text.to_owned());
            }
        }
    } else {
        blocks.push(full_text.to_owned());
    }

    blocks
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

pub(crate) fn extract_standard_text(
    full_text: &str,
    final_text: &str,
    tool_followups: &[String],
) -> String {
    let full_text = full_text.trim();
    if !full_text.is_empty() {
        return full_text.to_owned();
    }
    let final_text = final_text.trim();
    if !final_text.is_empty() {
        return final_text.to_owned();
    }
    tool_followups
        .iter()
        .filter(|text| !text.is_empty())
        .map(|text| text.trim())
        .next_back()
        .unwrap_or_default()
        .to_owned()
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => {
            value.as_i64().is_some_and(|number| number != 0)
                || value.as_u64().is_some_and(|number| number != 0)
                || value.as_f64().is_some_and(|number| number != 0.0)
        }
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_str(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => python_repr(value),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => python_repr_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| {
                    format!("{}: {}", python_repr_string(key), python_repr(value))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn python_repr_string(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::with_capacity(value.len().saturating_add(2));
    result.push(quote);
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if character == quote => {
                result.push('\\');
                result.push(character);
            }
            character if character.is_control() => {
                let code = u32::from(character);
                if code <= 0xff {
                    write!(&mut result, "\\x{code:02x}")
                        .expect("writing a formatted code point to String cannot fail");
                } else if code <= 0xffff {
                    write!(&mut result, "\\u{code:04x}")
                        .expect("writing a formatted code point to String cannot fail");
                } else {
                    write!(&mut result, "\\U{code:08x}")
                        .expect("writing a formatted code point to String cannot fail");
                }
            }
            character => result.push(character),
        }
    }
    result.push(quote);
    result
}

#[cfg(test)]
mod tests {
    use super::{DirectToolText, ToolTextResult, extract_tool_text, sanitize_text};
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/python_buddy_1_4_0_observed_parity.json"
        ))
        .expect("the Python parity fixture must be valid JSON")
    }

    #[test]
    fn sanitization_matches_the_python_buddy_1_4_0_fixture() {
        for case in fixture()["sanitization_cases"].as_array().unwrap() {
            assert_eq!(
                sanitize_text(case["input"].as_str().unwrap()),
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn extraction_matches_the_python_buddy_1_4_0_fixture() {
        for case in fixture()["extraction_cases"].as_array().unwrap() {
            let result = &case["result"];
            let tool_results = result["tool_results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| ToolTextResult {
                    response_markdown: item["response_markdown"].as_str().unwrap().to_owned(),
                    response_details: item["response_details"].as_array().unwrap().clone(),
                })
                .collect::<Vec<_>>();
            let tool_followups = result["tool_followups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            let input = DirectToolText {
                tool_results: &tool_results,
                full_text: result["full_text"].as_str().unwrap(),
                tool_followups: &tool_followups,
                final_text: result["final_text"].as_str().unwrap(),
            };
            assert_eq!(
                extract_tool_text(&input),
                case["expected"].as_str().unwrap(),
                "{}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn whitespace_only_followup_does_not_hide_final_text() {
        let input = DirectToolText {
            tool_results: &[],
            full_text: "",
            tool_followups: &["   ".to_owned()],
            final_text: "итог",
        };

        assert_eq!(extract_tool_text(&input), "итог");
    }
}
