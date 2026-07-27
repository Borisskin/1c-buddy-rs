//! Admission, validation, cancellation, and execution.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, RequestId, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use super::catalog::{CatalogError, CatalogErrorKind, PreparedToolCall, ToolCatalog};
use super::text::{
    DirectToolText, ToolTextResult, extract_standard_text, extract_tool_text, sanitize_text,
};
use crate::config::Config;
use crate::error::{
    ClientCallError, ProtocolFailure, ProtocolFailureKind, ToolFailure, ToolFailureKind,
};
use crate::limits::{MCP_FRAME_MAX_BYTES, QUEUE_WAIT_TIMEOUT, WAITING_CALLS_MAX};
use crate::naparnik::{CallContext, NaparnikClient};

const RESPONSE_TOO_LARGE_MESSAGE: &str = "the tool response exceeds the MCP frame limit";

struct AdmissionController {
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
}

impl AdmissionController {
    fn new(max_concurrent_calls: usize) -> Self {
        Self {
            admission: Arc::new(Semaphore::new(max_concurrent_calls + WAITING_CALLS_MAX)),
            execution: Arc::new(Semaphore::new(max_concurrent_calls)),
        }
    }

    fn try_admit(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.admission).try_acquire_owned().ok()
    }

    async fn acquire_execution(&self) -> Result<OwnedSemaphorePermit, AcquireError> {
        Arc::clone(&self.execution).acquire_owned().await
    }

    #[cfg(test)]
    fn try_acquire_execution(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.execution).try_acquire_owned().ok()
    }
}

pub(crate) trait ToolExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        call: PreparedToolCall,
        context: CallContext,
    ) -> impl Future<Output = Result<String, ClientCallError>> + Send;
}

pub(crate) struct NaparnikExecutor {
    client: NaparnikClient,
    ui_language: String,
}

impl NaparnikExecutor {
    #[must_use]
    pub(crate) fn new(client: NaparnikClient, ui_language: &str) -> Self {
        Self {
            client,
            ui_language: ui_language.to_owned(),
        }
    }
}

impl ToolExecutor for NaparnikExecutor {
    async fn execute(
        &self,
        call: PreparedToolCall,
        context: CallContext,
    ) -> Result<String, ClientCallError> {
        let mut conversation = self
            .client
            .create_conversation(&context, &self.ui_language, call.programming_language())
            .await?;
        let direct = call.route().exact_name().is_some();
        let response = if let Some(name) = call.route().exact_name() {
            let Some(expected_arguments) = call.route().exact_arguments() else {
                return Err(ToolFailure::new(
                    ToolFailureKind::Internal,
                    "an exact route is missing its arguments",
                    false,
                )
                .into());
            };
            self.client
                .execute_exact_message(
                    &context,
                    &mut conversation,
                    call.instruction(),
                    &[],
                    name,
                    expected_arguments,
                )
                .await?
        } else {
            self.client
                .execute_standard_message(&context, &mut conversation, call.instruction(), &[])
                .await?
        };
        let text = if direct {
            let tool_results = response
                .tool_results()
                .iter()
                .map(|result| ToolTextResult {
                    response_markdown: result.response_markdown().to_owned(),
                    response_details: result.response_details().to_vec(),
                })
                .collect::<Vec<_>>();
            extract_tool_text(&DirectToolText {
                tool_results: &tool_results,
                full_text: response.text(),
                tool_followups: response.tool_followups(),
                final_text: response.final_text(),
            })
        } else {
            extract_standard_text(
                response.text(),
                response.final_text(),
                response.tool_followups(),
            )
        };
        if text.is_empty() {
            return Err(ToolFailure::new(
                ToolFailureKind::UpstreamProtocol,
                "the assistant response did not contain text",
                false,
            )
            .into());
        }
        Ok(text)
    }
}

pub(crate) struct BuddyServer<E> {
    catalog: ToolCatalog,
    executor: Arc<E>,
    admission: AdmissionController,
    queue_wait_timeout: Duration,
    shutdown: tokio_util::sync::CancellationToken,
    next_operation_id: AtomicU64,
}

impl<E> BuddyServer<E>
where
    E: ToolExecutor,
{
    #[must_use]
    pub(crate) fn new(
        config: &Config,
        executor: E,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            catalog: ToolCatalog::from_config(config),
            executor: Arc::new(executor),
            admission: AdmissionController::new(config.max_concurrent_calls()),
            queue_wait_timeout: QUEUE_WAIT_TIMEOUT,
            shutdown,
            next_operation_id: AtomicU64::new(1),
        }
    }

    #[cfg(test)]
    fn for_test(
        catalog: ToolCatalog,
        executor: E,
        max_concurrent_calls: usize,
        queue_wait_timeout: Duration,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            catalog,
            executor: Arc::new(executor),
            admission: AdmissionController::new(max_concurrent_calls),
            queue_wait_timeout,
            shutdown,
            next_operation_id: AtomicU64::new(1),
        }
    }

    async fn execute_admitted(
        &self,
        prepared: PreparedToolCall,
        cancellation: tokio_util::sync::CancellationToken,
        _admission_permit: OwnedSemaphorePermit,
    ) -> ExecutionOutcome {
        let call_context = CallContext::for_tool_call_with_cancellation(cancellation.clone());
        let execution_permit = tokio::select! {
            () = cancellation.cancelled() => return ExecutionOutcome::Cancelled,
            acquired = timeout(self.queue_wait_timeout, self.admission.acquire_execution()) => {
                match acquired {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_closed)) => {
                        return ExecutionOutcome::Failure(ToolFailure::new(
                            ToolFailureKind::Internal,
                            "the execution queue is unavailable",
                            false,
                        ));
                    }
                    Err(_elapsed) => {
                        return ExecutionOutcome::Failure(ToolFailure::new(
                            ToolFailureKind::Timeout,
                            "the execution queue wait timed out",
                            false,
                        ));
                    }
                }
            }
        };

        let result = tokio::select! {
            () = cancellation.cancelled() => {
                drop(execution_permit);
                return ExecutionOutcome::Cancelled;
            }
            result = self.executor.execute(prepared, call_context) => result,
        };
        drop(execution_permit);

        match result {
            Ok(text) => ExecutionOutcome::Success(text),
            Err(ClientCallError::Cancelled) => ExecutionOutcome::Cancelled,
            Err(ClientCallError::Failure(failure)) => ExecutionOutcome::Failure(failure),
        }
    }
}

enum ExecutionOutcome {
    Success(String),
    Failure(ToolFailure),
    Cancelled,
}

impl<E> ServerHandler for BuddyServer<E>
where
    E: ToolExecutor,
{
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            self.catalog.tools().to_vec(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let prepared = match self.catalog.prepare(request.name.as_ref(), &arguments) {
            Ok(prepared) => prepared,
            Err(error) => return map_catalog_error(&error),
        };
        let operation_id = self.next_operation_id.fetch_add(1, Ordering::Relaxed);
        let canonical_name = prepared.canonical_name().to_owned();
        tracing::debug!(
            operation_id,
            tool = canonical_name,
            "accepted MCP tool call"
        );
        let Some(admission_permit) = self.admission.try_admit() else {
            return Ok(bounded_tool_result(
                tool_error("the server is busy; retry later"),
                &context.id,
            ));
        };

        let result = match self
            .execute_admitted(prepared, context.ct.clone(), admission_permit)
            .await
        {
            ExecutionOutcome::Success(text) => {
                CallToolResult::success(vec![ContentBlock::text(sanitize_text(&text))])
            }
            ExecutionOutcome::Failure(failure) => {
                log_tool_failure(operation_id, &canonical_name, &failure);
                tool_failure_result(&failure)
            }
            ExecutionOutcome::Cancelled => {
                if self.shutdown.is_cancelled() {
                    // During stdin EOF shutdown rmcp drains handler responses
                    // directly. Staying pending prevents a cancelled response from
                    // crossing that drain path; rmcp drops the task after its
                    // bounded shutdown grace period.
                    return std::future::pending().await;
                }
                // rmcp removes the request id before this future completes, so this
                // placeholder is deliberately dropped and never reaches stdout.
                return Err(McpError::internal_error("request cancelled", None));
            }
        };
        Ok(bounded_tool_result(result, &context.id))
    }
}

fn map_catalog_error(error: &CatalogError) -> Result<CallToolResult, McpError> {
    match error.kind() {
        CatalogErrorKind::UnknownTool => Err(ProtocolFailure::new(
            ProtocolFailureKind::UnknownTool,
            "unknown tool",
        )
        .into_mcp_error()),
        CatalogErrorKind::InvalidArguments | CatalogErrorKind::LimitExceeded => {
            Ok(tool_error(error.to_string()))
        }
    }
}

fn tool_failure_result(failure: &ToolFailure) -> CallToolResult {
    tool_error(failure.to_string())
}

fn log_tool_failure(operation_id: u64, tool: &str, failure: &ToolFailure) {
    tracing::warn!(
        operation_id,
        tool,
        failure_kind = ?failure.kind(),
        ambiguous_outcome = failure.ambiguous_outcome(),
        "MCP tool call failed"
    );
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl std::io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct ToolResponseFrame<'a> {
    jsonrpc: &'static str,
    id: &'a RequestId,
    result: &'a CallToolResult,
}

fn bounded_tool_result(result: CallToolResult, request_id: &RequestId) -> CallToolResult {
    let mut counter = ByteCounter::default();
    let serialized = serde_json::to_writer(
        &mut counter,
        &ToolResponseFrame {
            jsonrpc: "2.0",
            id: request_id,
            result: &result,
        },
    );
    if serialized.is_ok() && counter.bytes <= MCP_FRAME_MAX_BYTES {
        return result;
    }

    CallToolResult::error(vec![ContentBlock::text(RESPONSE_TOO_LARGE_MESSAGE)])
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        process::Stdio,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use rmcp::{
        ServiceExt,
        model::{CallToolResult, ContentBlock, RequestId},
    };
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
        process::{Child, Command},
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        AdmissionController, BuddyServer, ToolExecutor, bounded_tool_result, log_tool_failure,
    };
    use crate::config::CallMode;
    use crate::error::{ClientCallError, ToolFailure, ToolFailureKind};
    use crate::limits::MCP_FRAME_MAX_BYTES;
    use crate::mcp::catalog::{PreparedToolCall, ToolCatalog};
    use crate::mcp::codec::bounded_stdio_transport_with_cancellation;
    use crate::naparnik::CallContext;

    const CHILD_HELPER_ENV: &str = "ONEC_BUDDY_MCP_STDIO_TEST_HELPER";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(8);

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("captured log lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeExecutor;

    impl ToolExecutor for FakeExecutor {
        async fn execute(
            &self,
            call: PreparedToolCall,
            context: CallContext,
        ) -> Result<String, ClientCallError> {
            let question = call
                .arguments()
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match question {
                "fail" => Err(ToolFailure::new(
                    ToolFailureKind::UpstreamStatus,
                    "synthetic application failure",
                    false,
                )
                .into()),
                "wait" => {
                    context.cancellation().cancelled().await;
                    Err(ClientCallError::Cancelled)
                }
                "huge" => Ok("x".repeat(MCP_FRAME_MAX_BYTES)),
                _ => Ok("```1C\nСообщить(\"Готово\");\n```".to_owned()),
            }
        }
    }

    fn test_server(shutdown: CancellationToken) -> BuddyServer<FakeExecutor> {
        BuddyServer::for_test(
            ToolCatalog::for_test(1, 100_000, CallMode::Standard, "bsl", "", ""),
            FakeExecutor,
            2,
            Duration::from_secs(30),
            shutdown,
        )
    }

    #[tokio::test]
    async fn admission_capacity_is_execution_capacity_plus_eight() {
        let controller = AdmissionController::new(2);
        let mut permits = Vec::new();

        for _ in 0..10 {
            permits.push(
                controller
                    .try_admit()
                    .expect("the configured execution capacity plus eight must be admitted"),
            );
        }

        assert!(
            controller.try_admit().is_none(),
            "the next request must be rejected immediately"
        );
        drop(permits);
    }

    #[tokio::test]
    async fn execution_capacity_is_limited_independently_from_admission() {
        let controller = Arc::new(AdmissionController::new(2));
        let first = controller
            .acquire_execution()
            .await
            .expect("first execution slot");
        let second = controller
            .acquire_execution()
            .await
            .expect("second execution slot");

        assert!(
            controller.try_acquire_execution().is_none(),
            "a third execution must wait even though admission has spare capacity"
        );

        drop((first, second));
        assert!(controller.try_acquire_execution().is_some());
    }

    #[tokio::test]
    async fn execution_queue_wait_has_a_bounded_timeout() {
        let server = BuddyServer::for_test(
            ToolCatalog::for_test(1, 100_000, CallMode::Standard, "bsl", "", ""),
            FakeExecutor,
            2,
            Duration::from_millis(20),
            CancellationToken::new(),
        );
        let _first = server
            .admission
            .acquire_execution()
            .await
            .expect("first execution slot");
        let _second = server
            .admission
            .acquire_execution()
            .await
            .expect("second execution slot");
        let admission = server.admission.try_admit().expect("admission slot");
        let Value::Object(arguments) = json!({"question":"queued"}) else {
            unreachable!("object literal")
        };
        let prepared = server
            .catalog
            .prepare("ask_1c_ai", &arguments)
            .expect("valid prepared call");

        let outcome = server
            .execute_admitted(prepared, CancellationToken::new(), admission)
            .await;

        assert!(matches!(
            outcome,
            super::ExecutionOutcome::Failure(ref failure)
                if failure.kind() == ToolFailureKind::Timeout
        ));
    }

    #[tokio::test]
    async fn cancellation_while_queued_releases_the_admission_permit() {
        let server = Arc::new(BuddyServer::for_test(
            ToolCatalog::for_test(1, 100_000, CallMode::Standard, "bsl", "", ""),
            FakeExecutor,
            1,
            Duration::from_secs(30),
            CancellationToken::new(),
        ));
        let _held_execution = server
            .admission
            .acquire_execution()
            .await
            .expect("held execution slot");
        let admission = server.admission.try_admit().expect("admission slot");
        let Value::Object(arguments) = json!({"question":"queued"}) else {
            unreachable!("object literal")
        };
        let prepared = server
            .catalog
            .prepare("ask_1c_ai", &arguments)
            .expect("valid prepared call");
        let cancellation = CancellationToken::new();
        let task = {
            let server = Arc::clone(&server);
            let cancellation = cancellation.clone();
            tokio::spawn(async move {
                server
                    .execute_admitted(prepared, cancellation, admission)
                    .await
            })
        };
        tokio::task::yield_now().await;
        cancellation.cancel();

        assert!(matches!(
            task.await.expect("queued task"),
            super::ExecutionOutcome::Cancelled
        ));
        assert_eq!(
            server.admission.admission.available_permits(),
            1 + crate::limits::WAITING_CALLS_MAX
        );
    }

    #[test]
    fn oversized_known_tool_result_is_replaced_before_transport_framing() {
        let oversized =
            CallToolResult::success(vec![ContentBlock::text("x".repeat(MCP_FRAME_MAX_BYTES))]);

        let bounded = bounded_tool_result(oversized, &RequestId::Number(7));
        let frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": bounded,
        }))
        .expect("bounded result must serialize");

        assert!(frame.len() <= MCP_FRAME_MAX_BYTES);
        assert_eq!(bounded.is_error, Some(true));
        assert!(
            bounded.content[0]
                .as_text()
                .expect("fallback must be text")
                .text
                .contains("response")
        );
    }

    #[test]
    fn failure_log_contains_only_safe_operation_metadata() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer_capture = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || CapturedLogWriter(Arc::clone(&writer_capture)))
            .finish();
        let dispatcher = tracing::Dispatch::new(subscriber);
        let secrets = concat!(
            "Authorization: Bearer DO_NOT_LEAK_TOKEN ",
            "C:\\secrets\\token.txt ",
            "DO_NOT_LEAK_USER_CODE ",
            "DO_NOT_LEAK_UPSTREAM_RESPONSE"
        );
        let failure = ToolFailure::with_cause(
            ToolFailureKind::UpstreamProtocol,
            "the upstream response is invalid",
            false,
            std::io::Error::other(secrets),
        );

        tracing::dispatcher::with_default(&dispatcher, || {
            log_tool_failure(42, "check_1c_code", &failure);
        });

        let rendered = String::from_utf8(captured.lock().expect("captured log lock").clone())
            .expect("log must be UTF-8");
        assert!(rendered.contains("operation_id=42"));
        assert!(rendered.contains("tool=\"check_1c_code\""));
        assert!(rendered.contains("failure_kind=UpstreamProtocol"));
        for secret in [
            "Authorization",
            "DO_NOT_LEAK_TOKEN",
            r"C:\secrets\token.txt",
            "DO_NOT_LEAK_USER_CODE",
            "DO_NOT_LEAK_UPSTREAM_RESPONSE",
        ] {
            assert!(!rendered.contains(secret), "log exposed {secret}");
        }
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "one sequential child-process scenario verifies the complete stdio lifecycle"
    )]
    async fn true_child_process_handles_lifecycle_errors_success_and_cancellation() {
        if std::env::var_os(CHILD_HELPER_ENV).is_some() {
            return;
        }

        let mut child = spawn_helper();
        let mut writer = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let mut reader = BufReader::new(stdout);

        send_json(
            &mut writer,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "test-client", "version": "1.0.0"}
                }
            }),
        )
        .await;
        let initialized = read_for_id(&mut reader, 1).await;
        assert_eq!(
            initialized["result"]["serverInfo"]["name"],
            "onec-buddy-mcp"
        );
        send_json(
            &mut writer,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
        )
        .await;

        let mut boundary = br#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#.to_vec();
        boundary.resize(MCP_FRAME_MAX_BYTES, b' ');
        boundary.push(b'\n');
        writer
            .write_all(&boundary)
            .await
            .expect("write boundary frame");
        assert_eq!(read_for_id(&mut reader, 10).await["result"], json!({}));

        let prefix = br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"ask_1c_ai","arguments":{"question":""#;
        let suffix = br#""}}}"#;
        let mut oversized_frame = prefix.to_vec();
        oversized_frame.resize(MCP_FRAME_MAX_BYTES + 1 - suffix.len(), b'x');
        oversized_frame.extend_from_slice(suffix);
        oversized_frame.push(b'\n');
        writer
            .write_all(&oversized_frame)
            .await
            .expect("write oversized frame");
        let oversized_input = read_for_id(&mut reader, 11).await;
        assert_eq!(oversized_input["error"]["code"], -32600);

        send_json(
            &mut writer,
            &json!({
                "jsonrpc":"2.0",
                "id":12,
                "method":"tools/call",
                "params":{"name":"ask_1c_ai","arguments":{"question":1}}
            }),
        )
        .await;
        let invalid_known_call = read_for_id(&mut reader, 12).await;
        assert_eq!(invalid_known_call["result"]["isError"], true);

        send_json(
            &mut writer,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        let listed = read_for_id(&mut reader, 2).await;
        assert_eq!(
            listed["result"]["tools"]
                .as_array()
                .expect("tools array")
                .len(),
            8
        );

        send_tool_call(&mut writer, 3, "ask_1c_ai", "fail").await;
        let application_error = read_for_id(&mut reader, 3).await;
        assert_eq!(application_error["result"]["isError"], true);

        send_tool_call(&mut writer, 4, "ask_1c_ai", "success").await;
        let success = read_for_id(&mut reader, 4).await;
        assert_eq!(success["result"]["isError"], false);
        let success_text = success["result"]["content"][0]["text"]
            .as_str()
            .expect("text result");
        assert!(
            success_text.contains("```1C"),
            "unexpected successful text: {success_text:?}"
        );

        send_json(
            &mut writer,
            &json!({
                "jsonrpc":"2.0",
                "id":5,
                "method":"tools/call",
                "params":{"name":"unknown","arguments":{}}
            }),
        )
        .await;
        let unknown = read_for_id(&mut reader, 5).await;
        assert_eq!(unknown["error"]["code"], -32601);

        send_tool_call(&mut writer, 6, "ask_1c_ai", "huge").await;
        let oversized = read_for_id(&mut reader, 6).await;
        assert_eq!(oversized["result"]["isError"], true);

        send_tool_call(&mut writer, 7, "ask_1c_ai", "wait").await;
        send_json(
            &mut writer,
            &json!({
                "jsonrpc":"2.0",
                "method":"notifications/cancelled",
                "params":{"requestId":7,"reason":"test cancellation"}
            }),
        )
        .await;
        send_json(
            &mut writer,
            &json!({"jsonrpc":"2.0","id":8,"method":"ping"}),
        )
        .await;
        let ping = read_for_id_rejecting(&mut reader, 8, 7).await;
        assert_eq!(ping["result"], json!({}));

        send_tool_call(&mut writer, 9, "ask_1c_ai", "wait").await;
        drop(writer);
        assert_no_response_until_eof(&mut reader, 9).await;
        wait_for_clean_child_exit(&mut child).await;
    }

    #[tokio::test]
    async fn stdio_e2e_helper() {
        if std::env::var_os(CHILD_HELPER_ENV).is_none() {
            return;
        }

        let shutdown = CancellationToken::new();
        let transport = bounded_stdio_transport_with_cancellation(
            tokio::io::stdin(),
            tokio::io::stdout(),
            shutdown.clone(),
        );
        let running = test_server(shutdown.clone())
            .serve_with_ct(transport, shutdown)
            .await
            .expect("helper must initialize");
        running.waiting().await.expect("helper service task");
    }

    fn spawn_helper() -> Child {
        Command::new(std::env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("mcp::service::tests::stdio_e2e_helper")
            .arg("--quiet")
            .arg("--nocapture")
            .arg("--test-threads")
            .arg("1")
            .env(CHILD_HELPER_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stdio helper")
    }

    async fn send_tool_call<W>(writer: &mut W, id: i64, name: &str, question: &str)
    where
        W: AsyncWrite + Unpin,
    {
        send_json(
            writer,
            &json!({
                "jsonrpc":"2.0",
                "id":id,
                "method":"tools/call",
                "params":{"name":name,"arguments":{"question":question}}
            }),
        )
        .await;
    }

    async fn send_json<W>(writer: &mut W, message: &Value)
    where
        W: AsyncWrite + Unpin,
    {
        let mut serialized = serde_json::to_vec(message).expect("test message serializes");
        serialized.push(b'\n');
        writer
            .write_all(&serialized)
            .await
            .expect("write test message");
        writer.flush().await.expect("flush test message");
    }

    async fn read_for_id<R>(reader: &mut R, expected_id: i64) -> Value
    where
        R: AsyncBufReadExt + Unpin,
    {
        read_for_id_rejecting(reader, expected_id, i64::MIN).await
    }

    async fn read_for_id_rejecting<R>(reader: &mut R, expected_id: i64, rejected_id: i64) -> Value
    where
        R: AsyncBufReadExt + Unpin,
    {
        tokio::time::timeout(CHILD_TIMEOUT, async {
            loop {
                let mut line = String::new();
                let count = reader
                    .read_line(&mut line)
                    .await
                    .expect("read helper response");
                assert_ne!(count, 0, "helper closed before id {expected_id}");
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    // The Rust test harness owns this helper process and writes its
                    // own progress lines around the MCP server.
                    continue;
                };
                assert_ne!(
                    message.get("id").and_then(Value::as_i64),
                    Some(rejected_id),
                    "cancelled request must not receive a response"
                );
                if message.get("id").and_then(Value::as_i64) == Some(expected_id) {
                    return message;
                }
            }
        })
        .await
        .expect("helper response timeout")
    }

    async fn wait_for_clean_child_exit(child: &mut Child) {
        let status = tokio::time::timeout(CHILD_TIMEOUT, child.wait())
            .await
            .expect("helper did not stop after stdin EOF")
            .expect("wait for helper");
        assert!(status.success(), "helper exited unsuccessfully: {status}");
    }

    async fn assert_no_response_until_eof<R>(reader: &mut R, rejected_id: i64)
    where
        R: AsyncBufReadExt + Unpin,
    {
        tokio::time::timeout(CHILD_TIMEOUT, async {
            loop {
                let mut line = String::new();
                let count = reader
                    .read_line(&mut line)
                    .await
                    .expect("read helper shutdown output");
                if count == 0 {
                    return;
                }
                if let Ok(message) = serde_json::from_str::<Value>(&line) {
                    assert_ne!(
                        message.get("id").and_then(Value::as_i64),
                        Some(rejected_id),
                        "stdin EOF cancellation must not emit a response"
                    );
                }
            }
        })
        .await
        .expect("helper stdout did not close");
    }
}
