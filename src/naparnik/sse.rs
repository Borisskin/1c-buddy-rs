//! Strict bounded server-sent event parsing.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::error::{ClientCallError, ToolFailure, ToolFailureKind};
use crate::limits::{
    SSE_EVENT_MAX_BYTES, SSE_EVENT_TIMEOUT, SSE_STREAM_MAX_BYTES, TOOL_CALLS_PER_MESSAGE_MAX,
};
use crate::naparnik::client::CallContext;
use crate::naparnik::tool_roundtrip::{ToolCall, validate_tool_calls};

#[derive(Debug, Default, PartialEq)]
pub struct AssistantResponse {
    text: String,
    final_text: String,
    reasoning: String,
    assistant_uuid: Option<String>,
    tool_calls: Vec<ToolCall>,
    tool_results: Vec<RenderedToolResult>,
    tool_followups: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RenderedToolResult {
    response_markdown: String,
    response_details: Vec<Value>,
}

impl RenderedToolResult {
    #[must_use]
    pub(crate) fn response_markdown(&self) -> &str {
        &self.response_markdown
    }

    #[must_use]
    pub(crate) fn response_details(&self) -> &[Value] {
        &self.response_details
    }
}

impl AssistantResponse {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub(crate) fn final_text(&self) -> &str {
        &self.final_text
    }

    #[must_use]
    pub(crate) fn tool_results(&self) -> &[RenderedToolResult] {
        &self.tool_results
    }

    #[must_use]
    pub(crate) fn tool_followups(&self) -> &[String] {
        &self.tool_followups
    }

    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "reasoning is intentionally parsed but not exposed as answer text"
        )
    )]
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    #[must_use]
    pub fn assistant_uuid(&self) -> Option<&str> {
        self.assistant_uuid.as_deref()
    }

    #[must_use]
    pub fn tool_calls(&self) -> &[ToolCall] {
        &self.tool_calls
    }

    pub(super) fn absorb_round(&mut self, round: &Self) {
        merge_text(&mut self.text, &round.text);
        merge_text(&mut self.reasoning, &round.reasoning);
        if let Some(uuid) = &round.assistant_uuid {
            self.assistant_uuid = Some(uuid.clone());
        }
        self.tool_calls.clone_from(&round.tool_calls);
        self.tool_results.extend(round.tool_results.iter().cloned());
        self.tool_followups
            .extend(round.tool_followups.iter().cloned());
        if !round.final_text.is_empty() {
            self.final_text.clone_from(&round.final_text);
        }
    }

    fn buffered_bytes(&self) -> usize {
        let base = self
            .text
            .len()
            .saturating_add(self.reasoning.len())
            .saturating_add(self.assistant_uuid.as_ref().map_or(0, String::len));
        let with_calls = self.tool_calls.iter().fold(base, |total, call| {
            total
                .saturating_add(call.id().len())
                .saturating_add(call.name().len())
                .saturating_add(
                    serde_json::to_vec(call.arguments()).map_or(usize::MAX, |value| value.len()),
                )
        });
        let with_results = self.tool_results.iter().fold(with_calls, |total, result| {
            result.response_details.iter().fold(
                total.saturating_add(result.response_markdown.len()),
                |total, detail| {
                    total.saturating_add(
                        serde_json::to_vec(detail).map_or(usize::MAX, |value| value.len()),
                    )
                },
            )
        });
        self.tool_followups
            .iter()
            .fold(with_results, |total, text| total.saturating_add(text.len()))
    }
}

fn merge_text(accumulated: &mut String, next: &str) {
    if next.is_empty() || accumulated == next || accumulated.contains(next) {
        return;
    }
    if accumulated.is_empty() || next.contains(accumulated.as_str()) {
        accumulated.clear();
        accumulated.push_str(next);
        return;
    }

    let overlap = suffix_prefix_overlap(accumulated, next);
    if overlap == 0 {
        accumulated.push_str("\n\n");
    }
    accumulated.push_str(&next[overlap..]);
}

fn suffix_prefix_overlap(accumulated: &str, next: &str) -> usize {
    let mut max_overlap = accumulated.len().min(next.len());
    while !next.is_char_boundary(max_overlap) {
        max_overlap -= 1;
    }
    if max_overlap == 0 {
        return 0;
    }

    let pattern = &next.as_bytes()[..max_overlap];
    let mut prefix = vec![0_u32; pattern.len()];
    for index in 1..pattern.len() {
        let mut matched = prefix[index - 1] as usize;
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = prefix[matched - 1] as usize;
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        let Ok(encoded_match) = u32::try_from(matched) else {
            return 0;
        };
        prefix[index] = encoded_match;
    }

    let mut start = accumulated.len() - max_overlap;
    while !accumulated.is_char_boundary(start) {
        start += 1;
    }

    let mut matched = 0;
    for &byte in &accumulated.as_bytes()[start..] {
        while matched > 0 && (matched == pattern.len() || pattern[matched] != byte) {
            matched = prefix[matched - 1] as usize;
        }
        if pattern[matched] == byte {
            matched += 1;
        }
    }

    debug_assert!(next.is_char_boundary(matched));
    matched
}

pub async fn parse_response_stream<S, B, E>(
    stream: S,
    context: &CallContext,
) -> Result<AssistantResponse, ClientCallError>
where
    S: Stream<Item = Result<B, E>> + 'static,
    B: AsRef<[u8]>,
    E: Error + Send + Sync + 'static,
{
    parse_stream_with_timeout(stream, context, SSE_EVENT_TIMEOUT).await
}

#[cfg(test)]
async fn parse_stream_for_test<S, B, E>(
    stream: S,
    context: &CallContext,
    event_timeout: Duration,
) -> Result<AssistantResponse, ClientCallError>
where
    S: Stream<Item = Result<B, E>> + 'static,
    B: AsRef<[u8]>,
    E: Error + Send + Sync + 'static,
{
    parse_stream_with_timeout(stream, context, event_timeout).await
}

async fn parse_stream_with_timeout<S, B, E>(
    stream: S,
    context: &CallContext,
    event_timeout: Duration,
) -> Result<AssistantResponse, ClientCallError>
where
    S: Stream<Item = Result<B, E>> + 'static,
    B: AsRef<[u8]>,
    E: Error + Send + Sync + 'static,
{
    let stream_bytes = Arc::new(AtomicUsize::new(0));
    let bounded = BoundedSseStream::new(stream, Arc::clone(&stream_bytes));
    let mut events = bounded.eventsource();
    let mut state = ResponseState::default();

    loop {
        let next = await_next_event(context, event_timeout, events.next()).await?;
        match next {
            Some(Ok(event)) => {
                if event.data.len() > SSE_EVENT_MAX_BYTES {
                    return Err(limit_failure("an SSE event exceeds the size limit"));
                }
                if event.data == "[DONE]" {
                    return state.finish();
                }
                let finished = state.apply_json_event(&event.data)?;
                if stream_bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(state.response.buffered_bytes())
                    > SSE_STREAM_MAX_BYTES
                {
                    return Err(limit_failure(
                        "the SSE stream and accumulated response exceed the size limit",
                    ));
                }
                if finished {
                    return state.finish();
                }
            }
            Some(Err(error)) => return Err(map_stream_error(error)),
            None => {
                return Err(protocol_failure(
                    "the SSE stream ended without a terminal event",
                ));
            }
        }
    }
}

async fn await_next_event<F, T>(
    context: &CallContext,
    event_timeout: Duration,
    future: F,
) -> Result<T, ClientCallError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        () = context.cancellation().cancelled() => Err(ClientCallError::Cancelled),
        () = tokio::time::sleep_until(context.deadline()) => Err(ToolFailure::new(
            ToolFailureKind::Timeout,
            "the external operation exceeded its deadline while reading SSE",
            true,
        ).into()),
        () = tokio::time::sleep(event_timeout) => Err(ToolFailure::new(
            ToolFailureKind::Timeout,
            "the service did not produce a complete SSE event in time",
            true,
        ).into()),
        output = future => Ok(output),
    }
}

#[derive(Default)]
struct ResponseState {
    response: AssistantResponse,
}

impl ResponseState {
    fn apply_json_event(&mut self, data: &str) -> Result<bool, ClientCallError> {
        let value: Value = serde_json::from_str(data).map_err(|error| {
            ToolFailure::with_cause(
                ToolFailureKind::UpstreamProtocol,
                "an SSE event contains invalid JSON",
                false,
                error,
            )
        })?;
        let object = value
            .as_object()
            .ok_or_else(|| protocol_tool_failure("an SSE event must contain a JSON object"))?;

        let role = optional_string(object, "role")?;
        let uuid = optional_string(object, "uuid")?;
        if role == Some("assistant")
            && let Some(uuid) = uuid
        {
            if uuid.is_empty() {
                return Err(protocol_failure(
                    "an assistant SSE event contains an empty uuid",
                ));
            }
            self.response.assistant_uuid = Some(uuid.to_owned());
        }

        let finished = match object.get("finished") {
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(protocol_failure(
                    "an SSE event contains an invalid finished field",
                ));
            }
            None => false,
        };
        if role == Some("tool") && finished {
            self.response
                .tool_results
                .extend(parse_rendered_tool_results(object.get("render_info"))?);
            self.enforce_accumulated_limit()?;
            return Ok(false);
        }

        let mut text_from_content = false;
        let mut reasoning_from_content = false;
        let mut tool_calls_from_content = false;

        if let Some(content) = optional_object(object, "content")? {
            if let Some(snapshot) = optional_string(content, "content")? {
                self.response.text.clear();
                self.response.text.push_str(snapshot);
                text_from_content = true;
            }
            if let Some(reasoning) = optional_string(content, "reasoning_content")? {
                self.response.reasoning.clear();
                self.response.reasoning.push_str(reasoning);
                reasoning_from_content = true;
            }
            if let Some(calls) = content.get("tool_calls").filter(|calls| !calls.is_null()) {
                self.response.tool_calls = parse_tool_calls(calls)?;
                tool_calls_from_content = true;
            }
        }

        if let Some(delta) = optional_object(object, "content_delta")? {
            if !text_from_content && let Some(text) = optional_string(delta, "content")? {
                self.response.text.push_str(text);
            }
            if !reasoning_from_content
                && let Some(reasoning) = optional_string(delta, "reasoning_content")?
            {
                self.response.reasoning.push_str(reasoning);
            }
        }

        if !reasoning_from_content
            && let Some(reasoning) = optional_string(object, "reasoning_content")?
        {
            self.response.reasoning.push_str(reasoning);
        }
        if !tool_calls_from_content
            && let Some(calls) = object.get("tool_calls").filter(|calls| !calls.is_null())
        {
            self.response.tool_calls = parse_tool_calls(calls)?;
        }

        self.enforce_accumulated_limit()?;

        if finished
            && role == Some("assistant")
            && !self.response.tool_calls.is_empty()
            && self.response.assistant_uuid.is_none()
        {
            return Err(protocol_failure(
                "internal tool calls require an assistant message uuid",
            ));
        }
        Ok(finished && role == Some("assistant"))
    }

    fn enforce_accumulated_limit(&self) -> Result<(), ClientCallError> {
        if self.response.buffered_bytes() > SSE_STREAM_MAX_BYTES {
            return Err(limit_failure(
                "the accumulated SSE result exceeds the size limit",
            ));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<AssistantResponse, ClientCallError> {
        if !self.response.tool_calls.is_empty() && self.response.assistant_uuid.is_none() {
            return Err(protocol_failure(
                "internal tool calls require an assistant message uuid",
            ));
        }
        self.response.final_text = self.response.text.trim().to_owned();
        Ok(self.response)
    }
}

fn parse_rendered_tool_results(
    render_info: Option<&Value>,
) -> Result<Vec<RenderedToolResult>, ClientCallError> {
    let Some(render_info) = render_info.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let Some(items) = render_info.as_array() else {
        return Err(protocol_failure(
            "a tool SSE event contains invalid render_info",
        ));
    };
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let Some(item) = item.as_object() else {
            continue;
        };
        let response_markdown = item
            .get("response_markdown")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let response_details = item
            .get("details")
            .and_then(Value::as_object)
            .and_then(|details| details.get("response_details"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        results.push(RenderedToolResult {
            response_markdown,
            response_details,
        });
    }
    Ok(results)
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<Option<&'a str>, ClientCallError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(protocol_failure("an SSE event has an invalid string field")),
    }
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<Option<&'a Map<String, Value>>, ClientCallError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(value)),
        Some(_) => Err(protocol_failure("an SSE event has an invalid object field")),
    }
}

fn parse_tool_calls(value: &Value) -> Result<Vec<ToolCall>, ClientCallError> {
    let array = value
        .as_array()
        .ok_or_else(|| protocol_tool_failure("the internal tool_calls field must be an array"))?;
    if array.len() > TOOL_CALLS_PER_MESSAGE_MAX {
        return Err(limit_failure(
            "the internal tool call count exceeds the limit",
        ));
    }

    let mut calls = Vec::with_capacity(array.len());
    let mut identifiers = HashSet::with_capacity(array.len());
    for value in array {
        let object = value
            .as_object()
            .ok_or_else(|| protocol_tool_failure("an internal tool call must be an object"))?;
        let id = required_nonempty_string(object, "id")?;
        if !identifiers.insert(id.to_owned()) {
            return Err(protocol_failure(
                "internal tool call identifiers must be unique",
            ));
        }
        let function = object
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                protocol_tool_failure("an internal tool call requires a function object")
            })?;
        let name = required_nonempty_string(function, "name")?;
        let arguments = parse_tool_arguments(function)?;
        calls.push(ToolCall::new(id.to_owned(), name.to_owned(), arguments));
    }
    validate_tool_calls(&calls)?;
    Ok(calls)
}

fn parse_tool_arguments(
    function: &Map<String, Value>,
) -> Result<Map<String, Value>, ClientCallError> {
    match function.get("arguments") {
        Some(Value::Object(arguments)) => Ok(arguments.clone()),
        Some(Value::String(encoded)) => {
            let parsed: Value = serde_json::from_str(encoded).map_err(|error| {
                ToolFailure::with_cause(
                    ToolFailureKind::UpstreamProtocol,
                    "internal tool arguments contain invalid JSON",
                    false,
                    error,
                )
            })?;
            Ok(parsed.as_object().cloned().ok_or_else(|| {
                protocol_tool_failure("internal tool arguments must decode to a JSON object")
            })?)
        }
        _ => Err(protocol_tool_failure(
            "internal tool arguments must be an object or a JSON string",
        )
        .into()),
    }
}

fn required_nonempty_string<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, ClientCallError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| protocol_tool_failure("an internal tool call has an invalid string field"))
        .map_err(Into::into)
}

struct BoundedSseStream<S> {
    inner: Pin<Box<S>>,
    total_bytes: usize,
    shared_total_bytes: Arc<AtomicUsize>,
    event_bytes: usize,
    boundary: BoundaryState,
    terminated: bool,
}

struct BoundaryState {
    at_line_start: bool,
    bom_prefix: Vec<u8>,
    prefix_complete: bool,
    pending_cr: bool,
}

impl<S> BoundedSseStream<S> {
    fn new(stream: S, shared_total_bytes: Arc<AtomicUsize>) -> Self {
        Self {
            inner: Box::pin(stream),
            total_bytes: 0,
            shared_total_bytes,
            event_bytes: 0,
            boundary: BoundaryState {
                at_line_start: true,
                bom_prefix: Vec::with_capacity(3),
                prefix_complete: false,
                pending_cr: false,
            },
            terminated: false,
        }
    }

    fn transform_chunk<B: AsRef<[u8]>>(
        &mut self,
        chunk: &B,
    ) -> Result<Vec<u8>, BoundedStreamError<()>> {
        let source = chunk.as_ref();
        self.total_bytes = self.total_bytes.saturating_add(source.len());
        if self.total_bytes > SSE_STREAM_MAX_BYTES {
            return Err(BoundedStreamError::StreamTooLarge);
        }
        self.shared_total_bytes
            .store(self.total_bytes, Ordering::Relaxed);

        let without_bom = self.strip_initial_bom(source);
        let normalized = self.normalize_line_endings(&without_bom);
        self.scan_event_bytes(&normalized)?;
        Ok(normalized)
    }

    fn strip_initial_bom(&mut self, source: &[u8]) -> Vec<u8> {
        const UTF8_BOM: [u8; 3] = [0xef, 0xbb, 0xbf];
        if self.boundary.prefix_complete {
            return source.to_vec();
        }

        let mut output =
            Vec::with_capacity(source.len().saturating_add(self.boundary.bom_prefix.len()));
        for (index, &byte) in source.iter().enumerate() {
            self.boundary.bom_prefix.push(byte);
            if UTF8_BOM.starts_with(&self.boundary.bom_prefix)
                && self.boundary.bom_prefix.len() < UTF8_BOM.len()
            {
                continue;
            }

            self.boundary.prefix_complete = true;
            if self.boundary.bom_prefix != UTF8_BOM {
                output.extend_from_slice(&self.boundary.bom_prefix);
            }
            self.boundary.bom_prefix.clear();
            output.extend_from_slice(&source[index + 1..]);
            break;
        }
        output
    }

    fn normalize_line_endings(&mut self, source: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(source.len().saturating_add(1));
        for &byte in source {
            if self.boundary.pending_cr {
                output.push(b'\n');
                self.boundary.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' {
                self.boundary.pending_cr = true;
            } else {
                output.push(byte);
            }
        }
        output
    }

    fn scan_event_bytes(&mut self, bytes: &[u8]) -> Result<(), BoundedStreamError<()>> {
        for &byte in bytes {
            if byte == b'\n' {
                if self.boundary.at_line_start {
                    self.event_bytes = 0;
                } else {
                    self.boundary.at_line_start = true;
                }
            } else {
                self.boundary.at_line_start = false;
                self.event_bytes = self.event_bytes.saturating_add(1);
                if self.event_bytes > SSE_EVENT_MAX_BYTES {
                    return Err(BoundedStreamError::EventTooLarge);
                }
            }
        }
        Ok(())
    }

    fn flush_end(&mut self) -> Result<Option<Vec<u8>>, BoundedStreamError<()>> {
        let mut output = Vec::new();
        if !self.boundary.prefix_complete && !self.boundary.bom_prefix.is_empty() {
            self.boundary.prefix_complete = true;
            output.append(&mut self.boundary.bom_prefix);
        }
        if self.boundary.pending_cr {
            self.boundary.pending_cr = false;
            output.push(b'\n');
        }
        if output.is_empty() {
            Ok(None)
        } else {
            self.scan_event_bytes(&output)?;
            Ok(Some(output))
        }
    }
}

impl<S, B, E> Stream for BoundedSseStream<S>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    type Item = Result<Vec<u8>, BoundedStreamError<E>>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(chunk))) => match self.as_mut().get_mut().transform_chunk(&chunk) {
                Ok(normalized) => Poll::Ready(Some(Ok(normalized))),
                Err(BoundedStreamError::EventTooLarge) => {
                    self.terminated = true;
                    Poll::Ready(Some(Err(BoundedStreamError::EventTooLarge)))
                }
                Err(BoundedStreamError::StreamTooLarge) => {
                    self.terminated = true;
                    Poll::Ready(Some(Err(BoundedStreamError::StreamTooLarge)))
                }
                Err(BoundedStreamError::Transport(())) => unreachable!(),
            },
            Poll::Ready(Some(Err(error))) => {
                self.terminated = true;
                Poll::Ready(Some(Err(BoundedStreamError::Transport(error))))
            }
            Poll::Ready(None) => match self.as_mut().get_mut().flush_end() {
                Ok(Some(normalized)) => Poll::Ready(Some(Ok(normalized))),
                Ok(None) => {
                    self.terminated = true;
                    Poll::Ready(None)
                }
                Err(BoundedStreamError::EventTooLarge) => {
                    self.terminated = true;
                    Poll::Ready(Some(Err(BoundedStreamError::EventTooLarge)))
                }
                Err(BoundedStreamError::StreamTooLarge | BoundedStreamError::Transport(())) => {
                    unreachable!()
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
enum BoundedStreamError<E> {
    Transport(E),
    EventTooLarge,
    StreamTooLarge,
}

impl<E: fmt::Display> fmt::Display for BoundedStreamError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "transport error: {error}"),
            Self::EventTooLarge => formatter.write_str("SSE event exceeds limit"),
            Self::StreamTooLarge => formatter.write_str("SSE stream exceeds limit"),
        }
    }
}

impl<E: Error + 'static> Error for BoundedStreamError<E> {}

fn map_stream_error<E>(error: EventStreamError<BoundedStreamError<E>>) -> ClientCallError
where
    E: Error + Send + Sync + 'static,
{
    match error {
        EventStreamError::Transport(BoundedStreamError::EventTooLarge) => {
            limit_failure("an SSE event exceeds the size limit")
        }
        EventStreamError::Transport(BoundedStreamError::StreamTooLarge) => {
            limit_failure("the SSE stream exceeds the size limit")
        }
        EventStreamError::Transport(BoundedStreamError::Transport(error)) => {
            ToolFailure::with_cause(
                ToolFailureKind::UpstreamTransport,
                "the SSE stream could not be read",
                true,
                error,
            )
            .into()
        }
        EventStreamError::Utf8(error) => ToolFailure::with_cause(
            ToolFailureKind::UpstreamProtocol,
            "the SSE stream contains invalid UTF-8",
            false,
            error,
        )
        .into(),
        EventStreamError::Parser(error) => ToolFailure::with_cause(
            ToolFailureKind::UpstreamProtocol,
            "the SSE stream is malformed",
            false,
            error,
        )
        .into(),
    }
}

fn protocol_tool_failure(message: &'static str) -> ToolFailure {
    ToolFailure::new(ToolFailureKind::UpstreamProtocol, message, false)
}

fn protocol_failure(message: &'static str) -> ClientCallError {
    protocol_tool_failure(message).into()
}

fn limit_failure(message: &'static str) -> ClientCallError {
    ToolFailure::new(ToolFailureKind::LimitExceeded, message, false).into()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    use futures_util::stream;
    use serde_json::Value;

    use super::{AssistantResponse, merge_text, parse_stream_for_test};
    use crate::error::{ClientCallError, ToolFailureKind};
    use crate::limits::{SSE_EVENT_MAX_BYTES, SSE_STREAM_MAX_BYTES};
    use crate::naparnik::client::CallContext;

    fn chunks(bytes: &[u8], chunk_size: usize) -> Vec<Result<Vec<u8>, Infallible>> {
        bytes
            .chunks(chunk_size)
            .map(|chunk| Ok(chunk.to_vec()))
            .collect()
    }

    async fn parse(bytes: &[u8], chunk_size: usize) -> Result<AssistantResponse, ClientCallError> {
        parse_stream_for_test(
            stream::iter(chunks(bytes, chunk_size)),
            &CallContext::with_timeout(Duration::from_secs(1)),
            Duration::from_millis(100),
        )
        .await
    }

    fn failure_kind(error: ClientCallError) -> ToolFailureKind {
        match error {
            ClientCallError::Failure(failure) => failure.kind(),
            ClientCallError::Cancelled => panic!("unexpected cancellation"),
        }
    }

    #[test]
    fn merge_text_preserves_an_overlap_larger_than_eight_kibibytes() {
        let overlap = "аб".repeat(5_000);
        let mut accumulated = format!("начало-{overlap}");
        let next = format!("{overlap}-конец");

        merge_text(&mut accumulated, &next);

        assert_eq!(accumulated, format!("начало-{overlap}-конец"));
    }

    #[test]
    fn merge_text_handles_a_byte_limit_inside_a_unicode_scalar() {
        let mut accumulated = "x".to_owned();

        merge_text(&mut accumulated, "аб");

        assert_eq!(accumulated, "x\n\nаб");
    }

    #[tokio::test]
    async fn finished_tool_events_preserve_rendered_results_before_empty_assistant_text() {
        let payload = concat!(
            "data: {\"role\":\"tool\",\"render_info\":[",
            "{\"response_markdown\":\"Ошибок не обнаружено\",",
            "\"details\":{\"response_details\":[\"подробность\",true]}}",
            "],\"finished\":true}\n\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"finished\":true}\n\n",
        );
        let response = parse(payload.as_bytes(), 5).await.unwrap();
        assert!(response.text().is_empty());
        assert!(response.final_text().is_empty());
        assert_eq!(response.tool_results().len(), 1);
        assert_eq!(
            response.tool_results()[0].response_markdown(),
            "Ошибок не обнаружено"
        );
        assert_eq!(
            response.tool_results()[0].response_details(),
            &[Value::String("подробность".to_owned()), Value::Bool(true)]
        );
    }

    #[tokio::test]
    async fn arbitrary_byte_boundaries_preserve_utf8_crlf_fields_and_multiline_data() {
        let payload = concat!(
            "\u{feff}: комментарий\r\n",
            "event: message\r\n",
            "id: event-1\r\n",
            "retry: 25\r\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\r\n",
            "data: \"content\":{\"content\":\"привет\"},\"finished\":false}\r\n",
            "\r\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
            "\"content_delta\":{\"content\":\"!\",\"reasoning_content\":\"скрыто\"},",
            "\"finished\":true}\r\n\r\n",
        );

        for chunk_size in 1..=payload.len() {
            let response = parse(payload.as_bytes(), chunk_size).await.unwrap();
            assert_eq!(response.text(), "привет!");
            assert_eq!(response.reasoning(), "скрыто");
            assert_eq!(response.assistant_uuid(), Some("assistant-1"));
            assert!(response.tool_calls().is_empty());
        }
    }

    #[tokio::test]
    async fn lf_crlf_and_cr_are_supported_and_snapshots_replace_prior_text() {
        for eol in ["\n", "\r\n", "\r"] {
            let events = [
                r#"{"uuid":"a","role":"assistant","content_delta":{"content":"old"},"finished":false}"#,
                r#"{"uuid":"a","role":"assistant","content":{"content":"new"},"finished":false}"#,
                r#"{"uuid":"a","role":"assistant","content_delta":{"content":"!"},"finished":true}"#,
            ];
            let mut payload = String::new();
            for event in events {
                payload.push_str("data: ");
                payload.push_str(event);
                payload.push_str(eol);
                payload.push_str(eol);
            }

            let response = parse(payload.as_bytes(), 1).await.unwrap();
            assert_eq!(response.text(), "new!");
        }
    }

    #[tokio::test]
    async fn a_snapshot_wins_over_a_delta_from_the_same_event() {
        let payload = concat!(
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
            "\"content\":{\"content\":\"abc\",\"reasoning_content\":\"reason\"},",
            "\"content_delta\":{\"content\":\"c\",\"reasoning_content\":\"reason\"},",
            "\"finished\":false}\n\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
            "\"content_delta\":{\"content\":\"d\",\"reasoning_content\":\"!\"},",
            "\"finished\":true}\n\n",
        );

        let response = parse(payload.as_bytes(), 5).await.unwrap();
        assert_eq!(response.text(), "abcd");
        assert_eq!(response.reasoning(), "reason!");
    }

    #[tokio::test]
    async fn a_tool_call_delta_does_not_replace_the_complete_snapshot() {
        let payload = concat!(
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
            "\"content\":{\"tool_calls\":[{\"id\":\"call-1\",\"function\":{",
            "\"name\":\"TaskResult\",\"arguments\":\"{\\\"result\\\":\\\"ok\\\"}\"}}]},",
            "\"finished\":false}\n\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
            "\"content_delta\":{\"tool_calls\":[{\"id\":\"call-1\",\"function\":{",
            "\"name\":\"TaskResult\",\"arguments\":\"{\"}}]},",
            "\"finished\":true}\n\n",
        );

        let response = parse(payload.as_bytes(), 5).await.unwrap();
        assert_eq!(response.tool_calls().len(), 1);
        assert_eq!(response.tool_calls()[0].name(), "TaskResult");
        assert_eq!(response.tool_calls()[0].arguments()["result"], "ok");
    }

    #[tokio::test]
    async fn done_is_exact_and_eof_without_a_terminal_state_is_an_error() {
        let done = parse(b"data: [DONE]\n\n", 1).await.unwrap();
        assert_eq!(done.text(), "");

        let not_exact = parse(b"data: [DONE] \n\n", 2).await.unwrap_err();
        assert_eq!(failure_kind(not_exact), ToolFailureKind::UpstreamProtocol);

        let implicit_eof = parse(
            b"data: {\"uuid\":\"a\",\"role\":\"assistant\",\"finished\":false}\n\n",
            3,
        )
        .await
        .unwrap_err();
        assert_eq!(
            failure_kind(implicit_eof),
            ToolFailureKind::UpstreamProtocol
        );
    }

    #[tokio::test]
    async fn malformed_utf8_and_json_are_not_silently_skipped() {
        let malformed_utf8 = parse(b"data: \xff\n\n", 1).await.unwrap_err();
        assert_eq!(
            failure_kind(malformed_utf8),
            ToolFailureKind::UpstreamProtocol
        );

        let malformed_json = parse(b"data: {not-json}\n\n", 2).await.unwrap_err();
        assert_eq!(
            failure_kind(malformed_json),
            ToolFailureKind::UpstreamProtocol
        );
    }

    #[tokio::test]
    async fn event_and_stream_limits_are_enforced_before_unbounded_accumulation() {
        let oversized_event = format!(
            "data: {}\n\n",
            "x".repeat(SSE_EVENT_MAX_BYTES.saturating_add(1))
        );
        let event_error = parse(oversized_event.as_bytes(), 4096).await.unwrap_err();
        assert_eq!(failure_kind(event_error), ToolFailureKind::LimitExceeded);

        let event = "data: {}\n\n";
        let oversized_stream = event.repeat(SSE_STREAM_MAX_BYTES / event.len() + 1);
        let stream_error = parse(oversized_stream.as_bytes(), 4096).await.unwrap_err();
        assert_eq!(failure_kind(stream_error), ToolFailureKind::LimitExceeded);
    }

    #[tokio::test]
    async fn exact_event_and_stream_byte_boundaries_are_accepted() {
        let event_prefix =
            r#"data: {"uuid":"assistant-1","role":"assistant","content":{"content":""#;
        let event_suffix = r#""},"finished":true}"#;
        let event_content_bytes = SSE_EVENT_MAX_BYTES
            .checked_sub(event_prefix.len() + event_suffix.len())
            .expect("event envelope fits the limit");
        let exact_event = format!(
            "{event_prefix}{}{event_suffix}\n\n",
            "x".repeat(event_content_bytes)
        );
        assert_eq!(exact_event.len() - 2, SSE_EVENT_MAX_BYTES);
        let response = parse(exact_event.as_bytes(), 4096)
            .await
            .expect("an event at the exact limit is accepted");
        assert_eq!(response.text().len(), event_content_bytes);

        let terminal = b"data: [DONE]\n\n";
        let mut exact_stream = Vec::with_capacity(SSE_STREAM_MAX_BYTES);
        let mut filler_bytes = SSE_STREAM_MAX_BYTES - terminal.len();
        while filler_bytes > 0 {
            let mut event_length = filler_bytes.min(SSE_EVENT_MAX_BYTES + 2);
            if filler_bytes > event_length && filler_bytes - event_length < 3 {
                event_length -= 3 - (filler_bytes - event_length);
            }
            assert!(event_length >= 3);
            exact_stream.push(b':');
            exact_stream.extend(std::iter::repeat_n(b'x', event_length - 3));
            exact_stream.extend_from_slice(b"\n\n");
            filler_bytes -= event_length;
        }
        exact_stream.extend_from_slice(terminal);
        assert_eq!(exact_stream.len(), SSE_STREAM_MAX_BYTES);
        parse_stream_for_test(
            stream::iter(chunks(&exact_stream, 64 * 1024)),
            &CallContext::with_timeout(Duration::from_secs(10)),
            Duration::from_secs(5),
        )
        .await
        .expect("an SSE stream at the exact limit is accepted");
    }

    #[tokio::test]
    async fn stream_and_accumulated_result_share_the_eight_mib_limit() {
        let mut payload = String::new();
        for (index, fill) in ['a', 'b', 'c', 'd', 'e', 'f'].into_iter().enumerate() {
            let event = serde_json::json!({
                "uuid": "assistant-1",
                "role": "assistant",
                "content_delta": {"content": fill.to_string().repeat(700_000)},
                "finished": index == 5,
            });
            payload.push_str("data: ");
            payload.push_str(&event.to_string());
            payload.push_str("\n\n");
        }
        assert!(payload.len() < SSE_STREAM_MAX_BYTES);

        let error = parse(payload.as_bytes(), 4096)
            .await
            .expect_err("stream bytes plus accumulated result exceed eight MiB");
        assert_eq!(failure_kind(error), ToolFailureKind::LimitExceeded);
    }

    #[tokio::test]
    async fn tool_calls_are_a_complete_validated_array_and_require_an_assistant_uuid() {
        let valid = concat!(
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
            "{\"id\":\"call-1\",\"function\":{\"name\":\"TaskResult\",\"arguments\":{\"result\":\"one\"}}},",
            "{\"id\":\"call-2\",\"function\":{\"name\":\"mcp__syntax-checker__validate\",\"arguments\":{}}}",
            "]},\"finished\":true}\n\n",
        );
        let response = parse(valid.as_bytes(), 7).await.unwrap();
        assert_eq!(response.tool_calls().len(), 2);
        assert_eq!(response.tool_calls()[0].id(), "call-1");
        assert_eq!(response.tool_calls()[1].id(), "call-2");

        let string_arguments = concat!(
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
            "{\"id\":\"call-1\",\"function\":{\"name\":\"TaskResult\",",
            "\"arguments\":\"{\\\"result\\\":\\\"one\\\"}\"}}",
            "]},\"finished\":true}\n\n",
        );
        let response = parse(string_arguments.as_bytes(), 3).await.unwrap();
        assert_eq!(response.tool_calls()[0].arguments()["result"], "one");

        for invalid in [
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"","function":{"name":"TaskResult","arguments":{}}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":7,"arguments":{}}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult","arguments":"{"}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult","arguments":"[]"}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult","arguments":7}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult"}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult","arguments":{}}},{"id":"x","function":{"name":"TaskResult","arguments":{}}}]},"finished":true}"#,
            r#"{"role":"assistant","content":{"tool_calls":[{"id":"x","function":{"name":"TaskResult","arguments":{}}}]},"finished":true}"#,
            r#"{"uuid":"a","role":"assistant","content":{"tool_calls":{}},"finished":true}"#,
        ] {
            let payload = format!("data: {invalid}\n\n");
            let error = parse(payload.as_bytes(), 5).await.unwrap_err();
            assert_eq!(failure_kind(error), ToolFailureKind::UpstreamProtocol);
        }

        let missing_uuid_before_done = concat!(
            "data: {\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
            "{\"id\":\"x\",\"function\":{\"name\":\"TaskResult\",\"arguments\":{}}}",
            "]},\"finished\":false}\n\n",
            "data: [DONE]\n\n",
        );
        let error = parse(missing_uuid_before_done.as_bytes(), 4)
            .await
            .unwrap_err();
        assert_eq!(failure_kind(error), ToolFailureKind::UpstreamProtocol);
    }

    #[tokio::test]
    async fn null_tool_calls_are_treated_as_absent_in_every_supported_location() {
        for event in [
            r#"{"uuid":"assistant-1","role":"assistant","content":{"content":"ok","tool_calls":null},"finished":true}"#,
            r#"{"uuid":"assistant-1","role":"assistant","content_delta":{"content":"ok","tool_calls":null},"finished":true}"#,
            r#"{"uuid":"assistant-1","role":"assistant","content":{"content":"ok"},"tool_calls":null,"finished":true}"#,
        ] {
            let payload = format!("data: {event}\n\n");
            let response = parse(payload.as_bytes(), 5).await.unwrap();
            assert_eq!(response.text(), "ok");
            assert!(response.tool_calls().is_empty());
        }

        let retained = concat!(
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
            "{\"id\":\"call-1\",\"function\":{\"name\":\"TaskResult\",\"arguments\":{}}}",
            "]},\"finished\":false}\n\n",
            "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"tool_calls\":null,\"finished\":true}\n\n",
        );
        let response = parse(retained.as_bytes(), 7).await.unwrap();
        assert_eq!(response.tool_calls().len(), 1);
        assert_eq!(response.tool_calls()[0].id(), "call-1");
    }

    #[tokio::test]
    async fn waiting_for_a_complete_event_obeys_timeout_and_cancellation() {
        let timeout_error = parse_stream_for_test(
            stream::pending::<Result<Vec<u8>, Infallible>>(),
            &CallContext::with_timeout(Duration::from_secs(1)),
            Duration::from_millis(5),
        )
        .await
        .unwrap_err();
        assert_eq!(failure_kind(timeout_error), ToolFailureKind::Timeout);

        let context = CallContext::with_timeout(Duration::from_secs(1));
        context.cancel();
        let cancelled = parse_stream_for_test(
            stream::pending::<Result<Vec<u8>, Infallible>>(),
            &context,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(cancelled, ClientCallError::Cancelled));
    }

    #[tokio::test]
    async fn transport_failure_is_returned_without_polling_for_a_reconnection() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&polls);
        let source = stream::poll_fn(move |_| {
            let poll = observed_polls.fetch_add(1, Ordering::SeqCst);
            if poll == 0 {
                Poll::Ready(Some(Err::<Vec<u8>, _>(std::io::Error::other(
                    "simulated transport failure",
                ))))
            } else {
                Poll::Ready(Some(Ok(b"data: [DONE]\n\n".to_vec())))
            }
        });

        let error = parse_stream_for_test(
            source,
            &CallContext::with_timeout(Duration::from_secs(1)),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();

        assert_eq!(failure_kind(error), ToolFailureKind::UpstreamTransport);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }
}
