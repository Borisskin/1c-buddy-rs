//! HTTPS client construction and operations.

use std::future::Future;
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE,
    HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Client, Method, Response, StatusCode, Url, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::{ClientCallError, ToolFailure, ToolFailureKind};
use crate::limits::{
    CONNECT_TIMEOUT, CONVERSATION_RESPONSE_MAX_BYTES, ERROR_BODY_MAX_BYTES, HTTP_OPERATION_TIMEOUT,
    RETRY_AFTER_MAX, SAFE_RETRY_DELAYS, TOOL_CALL_TIMEOUT, UPSTREAM_REQUEST_MAX_BYTES,
};
use crate::naparnik::compat::{
    CREATE_CONVERSATION_PATH, CreateConversationRequest, CreateConversationResponse,
    MESSAGE_PATH_SUFFIX, PRODUCTION_BASE_URL, ToolMessageRequest, UserMessageRequest,
};
use crate::naparnik::sse::{AssistantResponse, parse_response_stream};
use crate::naparnik::tool_roundtrip::{StepCounter, build_standard_results, require_exact_tool};
use crate::naparnik::types::Conversation;

#[cfg(test)]
const TEST_AUTHORIZATION: &str = "test-placeholder-not-a-real-token";
const ACCEPT_LANGUAGE_VALUE: &str = "ru-ru,en-us;q=0.8,en;q=0.7";
const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/620.1 (KHTML, like Gecko) JavaFX/22 Safari/620.1";

#[derive(Clone)]
pub struct CallContext {
    cancellation: CancellationToken,
    deadline: Instant,
}

impl CallContext {
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "production MCP calls use externally linked cancellation"
        )
    )]
    pub fn for_tool_call() -> Self {
        Self::with_deadline_after(TOOL_CALL_TIMEOUT)
    }

    #[must_use]
    pub(crate) fn for_tool_call_with_cancellation(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            deadline: Instant::now() + TOOL_CALL_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_timeout(timeout: Duration) -> Self {
        Self::with_deadline_after(timeout)
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "production MCP calls construct the linked context directly"
        )
    )]
    fn with_deadline_after(timeout: Duration) -> Self {
        Self {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + timeout,
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "production cancellation is driven by RequestContext.ct"
        )
    )]
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
}

pub struct NaparnikClient {
    client: Client,
    base_url: Url,
    authorization: HeaderValue,
}

impl NaparnikClient {
    pub fn new(token: &SecretString) -> Result<Self, ToolFailure> {
        let base_url = Url::parse(PRODUCTION_BASE_URL).map_err(|error| {
            ToolFailure::with_cause(
                ToolFailureKind::Internal,
                "the fixed service address is invalid",
                false,
                error,
            )
        })?;
        let authorization = HeaderValue::from_str(token.expose_secret()).map_err(|error| {
            ToolFailure::with_cause(
                ToolFailureKind::InvalidArguments,
                "the configured token cannot be used as an HTTP authorization value",
                false,
                error,
            )
        })?;
        Self::build(base_url, authorization, true)
    }

    #[cfg(test)]
    fn for_test(base_url: Url) -> Result<Self, ToolFailure> {
        Self::build(
            base_url,
            HeaderValue::from_static(TEST_AUTHORIZATION),
            false,
        )
    }

    fn build(
        base_url: Url,
        authorization: HeaderValue,
        https_only: bool,
    ) -> Result<Self, ToolFailure> {
        let mut builder = Client::builder()
            .use_rustls_tls()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .gzip(true)
            .deflate(true)
            .brotli(true);
        if https_only {
            builder = builder.https_only(true);
        }
        let client = builder.build().map_err(|error| {
            ToolFailure::with_cause(
                ToolFailureKind::Internal,
                "the HTTP client could not be initialized",
                false,
                error,
            )
        })?;

        Ok(Self {
            client,
            base_url,
            authorization,
        })
    }

    pub async fn create_conversation(
        &self,
        context: &CallContext,
        ui_language: &str,
        programming_language: &str,
    ) -> Result<Conversation, ClientCallError> {
        let body = serialize_bounded(&CreateConversationRequest::new(
            ui_language,
            programming_language,
        ))?;
        let url = self
            .base_url
            .join(CREATE_CONVERSATION_PATH)
            .map_err(|error| {
                ToolFailure::with_cause(
                    ToolFailureKind::Internal,
                    "the fixed service path is invalid",
                    false,
                    error,
                )
            })?;
        let response = self
            .send_json(
                context,
                url,
                self.headers("*/*"),
                body,
                Operation::CreateConversation,
            )
            .await?;
        let body = read_bounded_body(response, CONVERSATION_RESPONSE_MAX_BYTES, context).await?;
        let parsed = CreateConversationResponse::parse(&body).map_err(|error| {
            ToolFailure::with_cause(
                ToolFailureKind::UpstreamProtocol,
                "the conversation response is incompatible",
                false,
                error,
            )
        })?;
        let (id, parent_uuid) = parsed.into_parts();
        Ok(Conversation::new(id, parent_uuid))
    }

    pub async fn send_user_message(
        &self,
        context: &CallContext,
        conversation: &Conversation,
        instruction: &str,
        tools: &[Value],
    ) -> Result<Response, ClientCallError> {
        let body = serialize_bounded(&UserMessageRequest::new(
            instruction,
            tools,
            conversation.parent_uuid(),
        ))?;
        self.send_message_body(context, conversation, body).await
    }

    pub async fn send_tool_results(
        &self,
        context: &CallContext,
        conversation: &Conversation,
        results: &[Value],
    ) -> Result<Response, ClientCallError> {
        let Some(parent_uuid) = conversation.parent_uuid() else {
            return Err(ToolFailure::new(
                ToolFailureKind::UpstreamProtocol,
                "tool results require an assistant message parent",
                false,
            )
            .into());
        };
        let body = serialize_bounded(&ToolMessageRequest::new(results, parent_uuid))?;
        self.send_message_body(context, conversation, body).await
    }

    pub async fn read_message_response(
        &self,
        context: &CallContext,
        response: Response,
    ) -> Result<AssistantResponse, ClientCallError> {
        parse_response_stream(response.bytes_stream(), context).await
    }

    pub async fn execute_standard_message(
        &self,
        context: &CallContext,
        conversation: &mut Conversation,
        instruction: &str,
        tools: &[Value],
    ) -> Result<AssistantResponse, ClientCallError> {
        let response = self
            .send_user_message(context, conversation, instruction, tools)
            .await?;
        let mut round = self.read_message_response(context, response).await?;
        let mut combined = AssistantResponse::default();
        let mut steps = StepCounter::default();

        loop {
            update_parent_from_response(conversation, &round)?;
            combined.absorb_round(&round);
            if round.tool_calls().is_empty() {
                return Ok(combined);
            }

            steps.begin_step()?;
            let results = build_standard_results(round.tool_calls())?;
            let response = self
                .send_tool_results(context, conversation, &results)
                .await?;
            round = self.read_message_response(context, response).await?;
        }
    }

    pub async fn execute_exact_message(
        &self,
        context: &CallContext,
        conversation: &mut Conversation,
        instruction: &str,
        tools: &[Value],
        expected_tool_name: &str,
        expected_arguments: &Map<String, Value>,
    ) -> Result<AssistantResponse, ClientCallError> {
        let response = self
            .send_user_message(context, conversation, instruction, tools)
            .await?;
        let first = self.read_message_response(context, response).await?;
        require_exact_tool(first.tool_calls(), expected_tool_name, expected_arguments)?;
        update_parent_from_response(conversation, &first)?;

        let results = build_standard_results(first.tool_calls())?;
        let response = self
            .send_tool_results(context, conversation, &results)
            .await?;
        let final_response = self.read_message_response(context, response).await?;
        if !final_response.tool_calls().is_empty() {
            return Err(ToolFailure::new(
                ToolFailureKind::UpstreamProtocol,
                "the exact internal tool produced an unexpected follow-up call",
                false,
            )
            .into());
        }
        update_parent_from_response(conversation, &final_response)?;

        let mut combined = AssistantResponse::default();
        combined.absorb_round(&first);
        combined.absorb_round(&final_response);
        Ok(combined)
    }

    async fn send_message_body(
        &self,
        context: &CallContext,
        conversation: &Conversation,
        body: Vec<u8>,
    ) -> Result<Response, ClientCallError> {
        let url = self.message_url(conversation.id())?;
        let response = self
            .send_json(
                context,
                url,
                self.headers("text/event-stream"),
                body,
                Operation::Message,
            )
            .await?;

        if !has_event_stream_content_type(&response) {
            return Err(ToolFailure::new(
                ToolFailureKind::UpstreamProtocol,
                "the service returned a non-SSE response",
                false,
            )
            .into());
        }
        Ok(response)
    }

    fn message_url(&self, conversation_id: &str) -> Result<Url, ClientCallError> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|()| {
                ToolFailure::new(
                    ToolFailureKind::Internal,
                    "the service address cannot contain path segments",
                    false,
                )
            })?;
            segments.clear();
            segments.extend(["chat_api", "v1", "conversations"]);
            segments.push(conversation_id);
            segments.push(MESSAGE_PATH_SUFFIX);
        }
        Ok(url)
    }

    fn headers(&self, accept: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        headers.insert(ACCEPT_CHARSET, HeaderValue::from_static("utf-8"));
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, deflate, br"),
        );
        headers.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_static(ACCEPT_LANGUAGE_VALUE),
        );
        headers.insert(AUTHORIZATION, self.authorization.clone());
        headers.insert(ORIGIN, HeaderValue::from_static("https://code.1c.ai"));
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://code.1c.ai/chat/"),
        );
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        headers.insert(
            HeaderName::from_static("session-id"),
            HeaderValue::from_static(""),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers
    }

    async fn send_json(
        &self,
        context: &CallContext,
        url: Url,
        headers: HeaderMap,
        body: Vec<u8>,
        operation: Operation,
    ) -> Result<Response, ClientCallError> {
        let attempts = SAFE_RETRY_DELAYS
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None));
        for (attempt, connect_retry_delay) in attempts.enumerate() {
            let request = self
                .client
                .request(Method::POST, url.clone())
                .headers(headers.clone())
                .timeout(HTTP_OPERATION_TIMEOUT)
                .body(body.clone());
            let result = await_in_context(context, request.send()).await?;

            match result {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let retry_headers = response.headers().clone();
                    discard_bounded_error_body(response, context).await?;

                    if connect_retry_delay.is_some() && operation.can_retry_status(status) {
                        let delay = retry_delay(&retry_headers, attempt, SystemTime::now());
                        wait_in_context(context, delay).await?;
                        continue;
                    }
                    return Err(ToolFailure::new(
                        ToolFailureKind::UpstreamStatus,
                        "the external service rejected the request",
                        false,
                    )
                    .into());
                }
                Err(error) => {
                    let retryable = error.is_connect();
                    if let (true, Some(delay)) = (retryable, connect_retry_delay) {
                        wait_in_context(context, delay).await?;
                        continue;
                    }

                    let kind = if error.is_timeout() {
                        ToolFailureKind::Timeout
                    } else {
                        ToolFailureKind::UpstreamTransport
                    };
                    return Err(ToolFailure::with_cause(
                        kind,
                        "the external service request failed",
                        !retryable,
                        error,
                    )
                    .into());
                }
            }
        }

        Err(ToolFailure::new(
            ToolFailureKind::Internal,
            "the retry controller reached an invalid state",
            false,
        )
        .into())
    }
}

fn update_parent_from_response(
    conversation: &mut Conversation,
    response: &AssistantResponse,
) -> Result<(), ClientCallError> {
    if let Some(uuid) = response.assistant_uuid() {
        conversation.advance_parent(uuid.to_owned());
        return Ok(());
    }
    if !response.tool_calls().is_empty() {
        return Err(ToolFailure::new(
            ToolFailureKind::UpstreamProtocol,
            "internal tool calls require an assistant message uuid",
            false,
        )
        .into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Operation {
    CreateConversation,
    Message,
}

impl Operation {
    fn can_retry_status(self, status: StatusCode) -> bool {
        status == StatusCode::LOCKED
            || status == StatusCode::TOO_MANY_REQUESTS
            || matches!(
                (self, status),
                (
                    Self::CreateConversation,
                    StatusCode::BAD_GATEWAY
                        | StatusCode::SERVICE_UNAVAILABLE
                        | StatusCode::GATEWAY_TIMEOUT
                )
            )
    }
}

fn serialize_bounded<T>(value: &T) -> Result<Vec<u8>, ClientCallError>
where
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(|error| {
        ToolFailure::with_cause(
            ToolFailureKind::Internal,
            "the external request could not be serialized",
            false,
            error,
        )
    })?;
    if body.len() > UPSTREAM_REQUEST_MAX_BYTES {
        return Err(ToolFailure::new(
            ToolFailureKind::LimitExceeded,
            "the external request exceeds the size limit",
            false,
        )
        .into());
    }
    Ok(body)
}

async fn read_bounded_body(
    response: Response,
    limit: usize,
    context: &CallContext,
) -> Result<Vec<u8>, ClientCallError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = await_in_context(context, stream.next()).await?;
        match next {
            Some(Ok(chunk)) => {
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err(ToolFailure::new(
                        ToolFailureKind::LimitExceeded,
                        "the external response exceeds the size limit",
                        false,
                    )
                    .into());
                }
                body.extend_from_slice(&chunk);
            }
            Some(Err(error)) => {
                return Err(ToolFailure::with_cause(
                    ToolFailureKind::UpstreamTransport,
                    "the external response could not be read",
                    true,
                    error,
                )
                .into());
            }
            None => return Ok(body),
        }
    }
}

async fn discard_bounded_error_body(
    response: Response,
    context: &CallContext,
) -> Result<(), ClientCallError> {
    let mut stream = response.bytes_stream();
    let mut remaining = ERROR_BODY_MAX_BYTES;
    while remaining > 0 {
        let next = await_in_context(context, stream.next()).await?;
        match next {
            Some(Ok(chunk)) => {
                if chunk.len() >= remaining {
                    break;
                }
                remaining -= chunk.len();
            }
            Some(Err(_)) | None => break,
        }
    }
    Ok(())
}

async fn await_in_context<F, T>(context: &CallContext, future: F) -> Result<T, ClientCallError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        () = context.cancellation.cancelled() => Err(ClientCallError::Cancelled),
        () = tokio::time::sleep_until(context.deadline) => Err(timeout_failure()),
        output = future => Ok(output),
    }
}

async fn wait_in_context(context: &CallContext, duration: Duration) -> Result<(), ClientCallError> {
    tokio::select! {
        biased;
        () = context.cancellation.cancelled() => Err(ClientCallError::Cancelled),
        () = tokio::time::sleep_until(context.deadline) => Err(ToolFailure::new(
            ToolFailureKind::Timeout,
            "the external operation exceeded its deadline while waiting to retry",
            false,
        ).into()),
        () = tokio::time::sleep(duration) => Ok(()),
    }
}

fn timeout_failure() -> ClientCallError {
    ToolFailure::new(
        ToolFailureKind::Timeout,
        "the external operation exceeded its deadline",
        true,
    )
    .into()
}

fn has_event_stream_content_type(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn retry_delay(headers: &HeaderMap, retry_index: usize, now: SystemTime) -> Duration {
    let fallback = SAFE_RETRY_DELAYS
        .get(retry_index)
        .copied()
        .unwrap_or(RETRY_AFTER_MAX);
    let mut values = headers.get_all(RETRY_AFTER).iter();
    let Some(value) = values.next() else {
        return fallback;
    };
    if values.next().is_some() {
        return fallback;
    }
    let Ok(value) = value.to_str() else {
        return fallback;
    };
    let value = value.trim();

    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_or(fallback, |delay| delay.min(RETRY_AFTER_MAX));
    }

    httpdate::parse_http_date(value).map_or(fallback, |date| {
        date.duration_since(now)
            .unwrap_or(Duration::ZERO)
            .min(RETRY_AFTER_MAX)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use serde_json::{Map, Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::{CallContext, NaparnikClient, Operation, retry_delay, serialize_bounded};
    use crate::error::{ClientCallError, ToolFailure, ToolFailureKind};
    use crate::limits::{
        CONVERSATION_RESPONSE_MAX_BYTES, ERROR_BODY_MAX_BYTES, TOOL_CALL_TIMEOUT,
        UPSTREAM_REQUEST_MAX_BYTES,
    };
    use crate::naparnik::types::Conversation;

    const TEST_AUTHORIZATION: &str = "test-placeholder-not-a-real-token";

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct ResponseSpec {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        delay: Duration,
    }

    impl ResponseSpec {
        fn json(status: &'static str, body: &Value) -> Self {
            Self {
                status,
                headers: vec![("Content-Type", "application/json".to_owned())],
                body: serde_json::to_vec(body).expect("test JSON must serialize"),
                delay: Duration::ZERO,
            }
        }

        fn sse(status: &'static str) -> Self {
            Self {
                status,
                headers: vec![(
                    "Content-Type",
                    "text/event-stream; charset=utf-8".to_owned(),
                )],
                body: Vec::new(),
                delay: Duration::ZERO,
            }
        }

        fn sse_with_body(body: impl Into<Vec<u8>>) -> Self {
            let mut response = Self::sse("200 OK");
            response.body = body.into();
            response
        }

        fn with_header(mut self, name: &'static str, value: &str) -> Self {
            self.headers.push((name, value.to_owned()));
            self
        }
    }

    async fn spawn_server(
        responses: Vec<ResponseSpec>,
    ) -> (reqwest::Url, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener must bind");
        let address = listener.local_addr().expect("listener has an address");
        let (sender, receiver) = mpsc::channel(responses.len().max(1));

        let _server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.expect("request must connect");
                let request = read_request(&mut stream).await;
                let _capture_was_received = sender.send(request).await.is_ok();
                if !response.delay.is_zero() {
                    tokio::time::sleep(response.delay).await;
                }
                write_response(&mut stream, &response).await;
            }
        });

        (
            reqwest::Url::parse(&format!("http://{address}")).expect("test URL is valid"),
            receiver,
        )
    }

    async fn spawn_disconnect_server() -> (reqwest::Url, mpsc::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener must bind");
        let address = listener.local_addr().expect("listener has an address");
        let (sender, receiver) = mpsc::channel(1);

        let _server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request must connect");
            let request = read_request(&mut stream).await;
            let _capture_was_received = sender.send(request).await.is_ok();
        });

        (
            reqwest::Url::parse(&format!("http://{address}")).expect("test URL is valid"),
            receiver,
        )
    }

    async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.expect("request must read");
            assert!(count > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
            assert!(bytes.len() <= 64 * 1024, "test request headers are bounded");
        };

        let header_text =
            std::str::from_utf8(&bytes[..header_end]).expect("request headers are UTF-8");
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().expect("request line exists");
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts.next().expect("method exists").to_owned();
        let path = request_parts.next().expect("path exists").to_owned();
        let mut headers = BTreeMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').expect("header has a colon");
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }

        let content_length = headers
            .get("content-length")
            .map_or(0, |value| value.parse::<usize>().expect("valid length"));
        while bytes.len() - header_end < content_length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.expect("body must read");
            assert!(count > 0, "request ended before body");
            bytes.extend_from_slice(&chunk[..count]);
        }

        CapturedRequest {
            method,
            path,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    async fn write_response(stream: &mut TcpStream, response: &ResponseSpec) {
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");

        stream
            .write_all(head.as_bytes())
            .await
            .expect("response headers must write");
        stream
            .write_all(&response.body)
            .await
            .expect("response body must write");
    }

    fn context() -> CallContext {
        CallContext::with_timeout(Duration::from_secs(5))
    }

    fn failure(error: ClientCallError) -> ToolFailure {
        match error {
            ClientCallError::Failure(failure) => failure,
            ClientCallError::Cancelled => panic!("unexpected cancellation"),
        }
    }

    #[tokio::test]
    async fn create_conversation_sends_the_exact_pinned_request() {
        let (base_url, mut requests) = spawn_server(vec![ResponseSpec::json(
            "201 Created",
            &json!({
                "uuid": "conversation-1",
                "root_message_uuid": "root-1",
                "unknown": true
            }),
        )])
        .await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");

        let conversation = client
            .create_conversation(&context(), "russian", "bsl")
            .await
            .expect("conversation must be created");
        assert_eq!(conversation.id(), "conversation-1");
        assert_eq!(conversation.parent_uuid(), Some("root-1"));

        let request = requests.recv().await.expect("request was captured");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/chat_api/v1/conversations/");
        for (name, expected) in [
            ("accept", "*/*"),
            ("accept-charset", "utf-8"),
            ("accept-encoding", "gzip, deflate, br"),
            ("accept-language", "ru-ru,en-us;q=0.8,en;q=0.7"),
            ("authorization", TEST_AUTHORIZATION),
            ("origin", "https://code.1c.ai"),
            ("referer", "https://code.1c.ai/chat/"),
            ("content-type", "application/json; charset=utf-8"),
            ("session-id", ""),
            (
                "user-agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/620.1 (KHTML, like Gecko) JavaFX/22 Safari/620.1",
            ),
        ] {
            assert_eq!(
                request.headers.get(name).map(String::as_str),
                Some(expected)
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).expect("request body is JSON"),
            json!({
                "is_chat": true,
                "skill_name": "custom",
                "ui_language": "russian",
                "programming_language": "bsl"
            })
        );
    }

    #[tokio::test]
    async fn message_uses_sse_accept_and_current_parent_without_retrying_503() {
        let (base_url, mut requests) = spawn_server(vec![ResponseSpec::json(
            "503 Service Unavailable",
            &json!({}),
        )])
        .await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));

        let error = client
            .send_user_message(
                &context(),
                &conversation,
                "question",
                &[json!({"name": "tool-1"})],
            )
            .await
            .expect_err("message 503 must not retry");
        let failure = failure(error);
        assert_eq!(failure.kind(), ToolFailureKind::UpstreamStatus);
        assert!(!failure.ambiguous_outcome());

        let request = requests.recv().await.expect("request was captured");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.path,
            "/chat_api/v1/conversations/conversation-1/messages"
        );
        assert_eq!(
            request.headers.get("accept").map(String::as_str),
            Some("text/event-stream")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).expect("message body is JSON"),
            json!({
                "role": "user",
                "content": {
                    "content": {"instruction": "question"},
                    "tools": [{"name": "tool-1"}]
                },
                "parent_uuid": "root-1"
            })
        );
        assert!(requests.try_recv().is_err(), "message was sent only once");
    }

    #[tokio::test]
    async fn accepted_message_response_is_parsed_directly_from_reqwest_byte_stream() {
        let sse = ResponseSpec::sse_with_body(
            concat!(
                "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",",
                "\"content_delta\":{\"content\":\"answer\"},\"finished\":true}\r\n\r\n"
            )
            .as_bytes()
            .to_vec(),
        );
        let (base_url, mut requests) = spawn_server(vec![sse]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));
        let context = context();

        let response = client
            .send_user_message(&context, &conversation, "question", &[])
            .await
            .expect("SSE response must be accepted");
        let parsed = client
            .read_message_response(&context, response)
            .await
            .expect("SSE response must parse");

        assert_eq!(parsed.text(), "answer");
        assert_eq!(parsed.assistant_uuid(), Some("assistant-1"));
        requests.recv().await.expect("request was captured");
    }

    #[tokio::test]
    async fn standard_mode_processes_every_tool_call_in_order_and_continues_to_final_text() {
        let first = ResponseSpec::sse_with_body(
            concat!(
                "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
                "{\"id\":\"first\",\"function\":{\"name\":\"TaskResult\",",
                "\"arguments\":\"{\\\"result\\\":\\\"done\\\"}\"}},",
                "{\"id\":\"second\",\"function\":{\"name\":\"unsupported\",\"arguments\":{}}}",
                "]},\"finished\":true}\n\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let final_response = ResponseSpec::sse_with_body(
            concat!(
                "data: {\"uuid\":\"assistant-2\",\"role\":\"assistant\",",
                "\"content\":{\"content\":\"final answer\"},\"finished\":true}\n\n"
            )
            .as_bytes()
            .to_vec(),
        );
        let (base_url, mut requests) = spawn_server(vec![first, final_response]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));

        let response = client
            .execute_standard_message(&context(), &mut conversation, "question", &[])
            .await
            .expect("standard tool round-trip succeeds");

        assert_eq!(response.text(), "final answer");
        assert_eq!(conversation.parent_uuid(), Some("assistant-2"));
        requests.recv().await.expect("initial request was captured");
        let tool_request = requests.recv().await.expect("tool request was captured");
        assert_eq!(
            serde_json::from_slice::<Value>(&tool_request.body).expect("tool body is JSON"),
            json!({
                "role": "tool",
                "content": [
                    {"status":"accepted","tool_call_id":"first","content":null},
                    {
                        "status":"rejected",
                        "tool_call_id":"second",
                        "content":{"error":"unsupported internal tool"}
                    }
                ],
                "parent_uuid": "assistant-1"
            })
        );
    }

    #[tokio::test]
    async fn direct_mode_rejects_extra_or_different_calls_without_a_fallback_request() {
        let response = ResponseSpec::sse_with_body(
            concat!(
                "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
                "{\"id\":\"first\",\"function\":{\"name\":\"mcp__syntax-checker__validate\",\"arguments\":{}}},",
                "{\"id\":\"second\",\"function\":{\"name\":\"mcp__knowledge-hub__Search_ITS\",\"arguments\":{}}}",
                "]},\"finished\":true}\n\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let (base_url, mut requests) = spawn_server(vec![response]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));
        let expected_arguments = Map::new();

        let error = client
            .execute_exact_message(
                &context(),
                &mut conversation,
                "request exact tool",
                &[],
                "mcp__syntax-checker__validate",
                &expected_arguments,
            )
            .await
            .expect_err("extra exact call must fail");

        assert_eq!(failure(error).kind(), ToolFailureKind::UpstreamProtocol);
        requests.recv().await.expect("initial request was captured");
        assert!(
            requests.try_recv().is_err(),
            "no fallback or guessed acknowledgement was sent"
        );
    }

    #[tokio::test]
    async fn direct_mode_rejects_different_arguments_before_acknowledgement() {
        let response = ResponseSpec::sse_with_body(
            concat!(
                "data: {\"uuid\":\"assistant-1\",\"role\":\"assistant\",\"content\":{\"tool_calls\":[",
                "{\"id\":\"first\",\"function\":{\"name\":\"mcp__syntax-checker__validate\",",
                "\"arguments\":\"{\\\"code\\\":\\\"other\\\",\\\"extended\\\":false}\"}}",
                "]},\"finished\":true}\n\n",
            )
            .as_bytes()
            .to_vec(),
        );
        let (base_url, mut requests) = spawn_server(vec![response]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));
        let Value::Object(expected_arguments) = json!({"code":"expected","extended":false}) else {
            unreachable!("object literal")
        };

        let error = client
            .execute_exact_message(
                &context(),
                &mut conversation,
                "request exact tool",
                &[],
                "mcp__syntax-checker__validate",
                &expected_arguments,
            )
            .await
            .expect_err("different exact arguments must fail");

        assert_eq!(failure(error).kind(), ToolFailureKind::UpstreamProtocol);
        requests.recv().await.expect("initial request was captured");
        assert!(
            requests.try_recv().is_err(),
            "mismatched arguments must not be acknowledged"
        );
    }

    #[tokio::test]
    async fn standard_mode_stops_before_acknowledging_an_eleventh_tool_round() {
        let responses = (0..=crate::limits::INTERNAL_TOOL_STEPS_MAX)
            .map(|index| {
                ResponseSpec::sse_with_body(
                    format!(
                        concat!(
                            "data: {{\"uuid\":\"assistant-{0}\",\"role\":\"assistant\",",
                            "\"content\":{{\"tool_calls\":[{{\"id\":\"call-{0}\",",
                            "\"function\":{{\"name\":\"TaskResult\",\"arguments\":{{}}}}}}]}},",
                            "\"finished\":true}}\n\n"
                        ),
                        index
                    )
                    .into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let (base_url, mut requests) = spawn_server(responses).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));

        let error = client
            .execute_standard_message(&context(), &mut conversation, "question", &[])
            .await
            .expect_err("eleventh internal tool round must fail");

        assert_eq!(failure(error).kind(), ToolFailureKind::LimitExceeded);
        for _ in 0..=crate::limits::INTERNAL_TOOL_STEPS_MAX {
            requests.recv().await.expect("bounded request was captured");
        }
        assert!(
            requests.try_recv().is_err(),
            "the eleventh tool result was not sent"
        );
    }

    #[tokio::test]
    async fn create_conversation_retries_503_three_times_then_succeeds() {
        let retry = ResponseSpec::json("503 Service Unavailable", &json!({}))
            .with_header("Retry-After", "0");
        let (base_url, mut requests) = spawn_server(vec![
            retry.clone(),
            retry.clone(),
            retry,
            ResponseSpec::json("201 Created", &json!({"uuid": "conversation-after-retry"})),
        ])
        .await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");

        let conversation = client
            .create_conversation(&context(), "russian", "")
            .await
            .expect("fourth attempt succeeds");
        assert_eq!(conversation.id(), "conversation-after-retry");

        for _ in 0..4 {
            requests.recv().await.expect("all attempts were captured");
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn every_create_call_receives_fresh_conversation_state() {
        let (base_url, mut requests) = spawn_server(vec![
            ResponseSpec::json("201 Created", &json!({"uuid": "conversation-1"})),
            ResponseSpec::json("201 Created", &json!({"uuid": "conversation-2"})),
        ])
        .await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");

        let first = client
            .create_conversation(&context(), "russian", "bsl")
            .await
            .expect("first conversation");
        let second = client
            .create_conversation(&context(), "russian", "bsl")
            .await
            .expect("second conversation");

        assert_eq!(first.id(), "conversation-1");
        assert_eq!(second.id(), "conversation-2");
        requests.recv().await.expect("first request");
        requests.recv().await.expect("second request");
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn parallel_create_calls_keep_conversation_state_isolated() {
        let (base_url, mut requests) = spawn_server(vec![
            ResponseSpec::json(
                "201 Created",
                &json!({"uuid": "conversation-1", "root_message_uuid": "root-1"}),
            ),
            ResponseSpec::json(
                "201 Created",
                &json!({"uuid": "conversation-2", "root_message_uuid": "root-2"}),
            ),
        ])
        .await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let first_context = context();
        let second_context = context();

        let (first, second) = tokio::join!(
            client.create_conversation(&first_context, "russian", "bsl"),
            client.create_conversation(&second_context, "russian", "bsl"),
        );
        let mut states = [first.unwrap(), second.unwrap()]
            .map(|conversation| {
                (
                    conversation.id().to_owned(),
                    conversation.parent_uuid().map(str::to_owned),
                )
            })
            .to_vec();
        states.sort();

        assert_eq!(
            states,
            vec![
                ("conversation-1".to_owned(), Some("root-1".to_owned())),
                ("conversation-2".to_owned(), Some("root-2".to_owned())),
            ]
        );
        requests.recv().await.expect("first request");
        requests.recv().await.expect("second request");
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn redirect_is_not_followed_and_missing_uuid_is_a_protocol_failure() {
        let (redirect_url, mut redirect_requests) = spawn_server(vec![
            ResponseSpec::json("302 Found", &json!({})).with_header("Location", "/redirected"),
        ])
        .await;
        let redirect_client =
            NaparnikClient::for_test(redirect_url).expect("test client must build");
        let redirect_error = failure(
            redirect_client
                .create_conversation(&context(), "russian", "bsl")
                .await
                .expect_err("redirect must not be followed"),
        );
        assert_eq!(redirect_error.kind(), ToolFailureKind::UpstreamStatus);
        redirect_requests
            .recv()
            .await
            .expect("only original request was captured");
        assert!(redirect_requests.try_recv().is_err());

        let (invalid_url, _) =
            spawn_server(vec![ResponseSpec::json("200 OK", &json!({"future": true}))]).await;
        let invalid_client = NaparnikClient::for_test(invalid_url).expect("test client must build");
        let invalid_error = failure(
            invalid_client
                .create_conversation(&context(), "russian", "bsl")
                .await
                .expect_err("missing uuid violates compatibility"),
        );
        assert_eq!(invalid_error.kind(), ToolFailureKind::UpstreamProtocol);
    }

    #[tokio::test]
    async fn message_requires_event_stream_content_type() {
        let (base_url, _) =
            spawn_server(vec![ResponseSpec::json("200 OK", &json!({"not": "sse"}))]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let conversation = Conversation::new("conversation-1".to_owned(), None);

        let error = failure(
            client
                .send_user_message(&context(), &conversation, "question", &[])
                .await
                .expect_err("wrong content type must fail"),
        );
        assert_eq!(error.kind(), ToolFailureKind::UpstreamProtocol);
    }

    #[tokio::test]
    async fn tool_results_use_the_latest_assistant_uuid_as_parent() {
        let (base_url, mut requests) = spawn_server(vec![ResponseSpec::sse("200 OK")]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let mut conversation =
            Conversation::new("conversation-1".to_owned(), Some("root-1".to_owned()));
        conversation.advance_parent("assistant-1".to_owned());
        let results = vec![
            json!({"tool_call_id": "call-1", "content": "first"}),
            json!({"tool_call_id": "call-2", "content": "second"}),
        ];

        client
            .send_tool_results(&context(), &conversation, &results)
            .await
            .expect("tool results must be sent");

        let request = requests.recv().await.expect("request was captured");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).expect("message body is JSON"),
            json!({
                "role": "tool",
                "content": results,
                "parent_uuid": "assistant-1"
            })
        );
    }

    #[tokio::test]
    async fn ambiguous_disconnect_after_message_body_is_not_retried() {
        let (base_url, mut requests) = spawn_disconnect_server().await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let conversation = Conversation::new("conversation-1".to_owned(), None);

        let error = failure(
            client
                .send_user_message(&context(), &conversation, "question", &[])
                .await
                .expect_err("disconnect after request is ambiguous"),
        );

        assert_eq!(error.kind(), ToolFailureKind::UpstreamTransport);
        assert!(error.ambiguous_outcome());
        requests.recv().await.expect("one request was captured");
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn serialized_request_accepts_exactly_one_mib_and_rejects_one_more_byte() {
        let envelope_bytes = serde_json::to_vec(&json!({"value": ""}))
            .expect("test JSON")
            .len();
        let exact = json!({"value": "x".repeat(UPSTREAM_REQUEST_MAX_BYTES - envelope_bytes)});
        let exact_body = serialize_bounded(&exact).expect("exactly one MiB is accepted");
        assert_eq!(exact_body.len(), UPSTREAM_REQUEST_MAX_BYTES);

        let oversized =
            json!({"value": "x".repeat(UPSTREAM_REQUEST_MAX_BYTES - envelope_bytes + 1)});
        let error = failure(serialize_bounded(&oversized).expect_err("one extra byte is rejected"));
        assert_eq!(error.kind(), ToolFailureKind::LimitExceeded);
    }

    #[tokio::test]
    async fn request_and_conversation_response_limits_are_enforced() {
        let (request_url, mut request_captures) = spawn_server(vec![ResponseSpec::json(
            "201 Created",
            &json!({"uuid": "must-not-be-used"}),
        )])
        .await;
        let request_client = NaparnikClient::for_test(request_url).expect("test client must build");
        let conversation = Conversation::new("conversation-1".to_owned(), None);
        let request_error = failure(
            request_client
                .send_user_message(
                    &context(),
                    &conversation,
                    &"x".repeat(UPSTREAM_REQUEST_MAX_BYTES),
                    &[],
                )
                .await
                .expect_err("serialized request exceeds one MiB"),
        );
        assert_eq!(request_error.kind(), ToolFailureKind::LimitExceeded);
        assert!(request_captures.try_recv().is_err());

        let response_envelope_bytes = br#"{"uuid":""}"#.len();
        let exact_body = format!(
            "{{\"uuid\":\"{}\"}}",
            "x".repeat(CONVERSATION_RESPONSE_MAX_BYTES - response_envelope_bytes)
        )
        .into_bytes();
        assert_eq!(exact_body.len(), CONVERSATION_RESPONSE_MAX_BYTES);
        let (exact_url, _) = spawn_server(vec![ResponseSpec {
            status: "200 OK",
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: exact_body,
            delay: Duration::ZERO,
        }])
        .await;
        let exact_client = NaparnikClient::for_test(exact_url).expect("test client must build");
        let exact_conversation = exact_client
            .create_conversation(&context(), "russian", "bsl")
            .await
            .expect("a conversation response at the exact limit is accepted");
        assert_eq!(
            exact_conversation.id().len(),
            CONVERSATION_RESPONSE_MAX_BYTES - response_envelope_bytes
        );

        let oversized_body = format!(
            "{{\"uuid\":\"{}\"}}",
            "x".repeat(CONVERSATION_RESPONSE_MAX_BYTES)
        )
        .into_bytes();
        let oversized_response = ResponseSpec {
            status: "200 OK",
            headers: vec![("Content-Type", "application/json".to_owned())],
            body: oversized_body,
            delay: Duration::ZERO,
        };
        let (response_url, _) = spawn_server(vec![oversized_response]).await;
        let response_client =
            NaparnikClient::for_test(response_url).expect("test client must build");
        let response_error = failure(
            response_client
                .create_conversation(&context(), "russian", "bsl")
                .await
                .expect_err("conversation response exceeds one MiB"),
        );
        assert_eq!(response_error.kind(), ToolFailureKind::LimitExceeded);
    }

    #[tokio::test]
    async fn error_body_boundary_is_not_exposed_or_retried() {
        const SECRET: &str =
            "Authorization: Bearer DO_NOT_LEAK; DO_NOT_LEAK_USER_CODE; DO_NOT_LEAK_RESPONSE;";

        for body_size in [ERROR_BODY_MAX_BYTES, ERROR_BODY_MAX_BYTES + 1] {
            let mut body = SECRET.repeat(body_size.div_ceil(SECRET.len())).into_bytes();
            body.truncate(body_size);
            let response = ResponseSpec {
                status: "400 Bad Request",
                headers: vec![("Content-Type", "text/plain".to_owned())],
                body,
                delay: Duration::ZERO,
            };
            let (base_url, mut requests) = spawn_server(vec![response]).await;
            let client = NaparnikClient::for_test(base_url).expect("test client must build");

            let error = failure(
                client
                    .create_conversation(&context(), "russian", "bsl")
                    .await
                    .expect_err("HTTP 400 is an application failure"),
            );
            let rendered = format!("{error:?} {error}");
            assert_eq!(error.kind(), ToolFailureKind::UpstreamStatus);
            assert!(!rendered.contains("Authorization"));
            assert!(!rendered.contains("DO_NOT_LEAK"));
            requests.recv().await.expect("one request was captured");
            assert!(requests.try_recv().is_err(), "HTTP 400 was not retried");
        }
    }

    #[test]
    fn retry_after_supports_seconds_dates_caps_and_ambiguity() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);

        let empty = HeaderMap::new();
        assert_eq!(retry_delay(&empty, 0, now), Duration::from_secs(1));
        assert_eq!(retry_delay(&empty, 1, now), Duration::from_secs(2));
        assert_eq!(retry_delay(&empty, 2, now), Duration::from_secs(4));

        let mut seconds = HeaderMap::new();
        seconds.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(retry_delay(&seconds, 0, now), Duration::from_secs(30));

        let mut past = HeaderMap::new();
        past.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(now - Duration::from_secs(1)))
                .expect("date is a header"),
        );
        assert_eq!(retry_delay(&past, 0, now), Duration::ZERO);

        let mut future = HeaderMap::new();
        future.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(now + Duration::from_secs(12)))
                .expect("date is a header"),
        );
        assert_eq!(retry_delay(&future, 0, now), Duration::from_secs(12));

        let mut invalid = HeaderMap::new();
        invalid.insert(RETRY_AFTER, HeaderValue::from_static("-1"));
        assert_eq!(retry_delay(&invalid, 1, now), Duration::from_secs(2));

        let mut overflow = HeaderMap::new();
        overflow.insert(
            RETRY_AFTER,
            HeaderValue::from_static("184467440737095516160"),
        );
        assert_eq!(retry_delay(&overflow, 1, now), Duration::from_secs(2));

        let mut ambiguous = HeaderMap::new();
        ambiguous.append(RETRY_AFTER, HeaderValue::from_static("1"));
        ambiguous.append(RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(retry_delay(&ambiguous, 2, now), Duration::from_secs(4));
    }

    #[test]
    fn retry_matrix_is_closed_by_operation_and_status() {
        for operation in [Operation::CreateConversation, Operation::Message] {
            assert!(operation.can_retry_status(reqwest::StatusCode::LOCKED));
            assert!(operation.can_retry_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
            assert!(!operation.can_retry_status(reqwest::StatusCode::BAD_REQUEST));
            assert!(!operation.can_retry_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        }

        for status in [
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(Operation::CreateConversation.can_retry_status(status));
            assert!(!Operation::Message.can_retry_status(status));
        }
    }

    #[test]
    fn production_call_context_has_the_fixed_five_minute_deadline() {
        let started = Instant::now();
        let context = CallContext::for_tool_call();
        let elapsed_to_deadline = context.deadline.duration_since(started);
        let upper_bound = TOOL_CALL_TIMEOUT
            .checked_add(Duration::from_secs(1))
            .expect("tool timeout fits in Duration");

        assert!(elapsed_to_deadline >= TOOL_CALL_TIMEOUT);
        assert!(elapsed_to_deadline < upper_bound);
    }

    #[tokio::test]
    async fn deadline_and_cancellation_stop_the_request() {
        let mut delayed = ResponseSpec::sse("200 OK");
        delayed.delay = Duration::from_millis(100);
        let (base_url, _) = spawn_server(vec![delayed]).await;
        let client = NaparnikClient::for_test(base_url).expect("test client must build");
        let conversation = Conversation::new("conversation-1".to_owned(), None);
        let deadline = CallContext::with_timeout(Duration::from_millis(10));

        let timeout = failure(
            client
                .send_user_message(&deadline, &conversation, "question", &[])
                .await
                .expect_err("deadline must stop the request"),
        );
        assert_eq!(timeout.kind(), ToolFailureKind::Timeout);

        let cancelled = CallContext::with_timeout(Duration::from_secs(1));
        cancelled.cancel();
        assert!(matches!(
            client
                .send_user_message(&cancelled, &conversation, "question", &[])
                .await,
            Err(ClientCallError::Cancelled)
        ));
    }
}
