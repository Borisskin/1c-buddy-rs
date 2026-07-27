//! Bounded JSON-RPC message framing.

use std::{marker::PhantomData, sync::Arc};

use crate::error::{ProtocolFailure, ProtocolFailureKind};
use futures_util::SinkExt;
use rmcp::{
    RoleServer,
    model::{ClientJsonRpcMessage, RequestId, ServerJsonRpcMessage},
    transport::{
        Transport,
        async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    sync::Mutex,
};
use tokio_util::{
    bytes::{BufMut, BytesMut},
    codec::{Decoder, Encoder, FramedWrite},
    sync::CancellationToken,
};

pub const MAX_INBOUND_MESSAGE_BYTES: usize = crate::limits::MCP_FRAME_MAX_BYTES;
pub const MAX_OUTBOUND_MESSAGE_BYTES: usize = crate::limits::MCP_FRAME_MAX_BYTES;

struct BoundedJsonRpcMessageCodec<T> {
    max_length: usize,
    marker: PhantomData<fn() -> T>,
}

impl<T> BoundedJsonRpcMessageCodec<T> {
    const fn new(max_length: usize) -> Self {
        Self {
            max_length,
            marker: PhantomData,
        }
    }
}

impl<T> Encoder<T> for BoundedJsonRpcMessageCodec<T>
where
    T: serde::Serialize,
{
    type Error = JsonRpcMessageCodecError;

    fn encode(&mut self, item: T, output: &mut BytesMut) -> Result<(), Self::Error> {
        let serialized = serde_json::to_vec(&item)?;
        if serialized.len() > self.max_length {
            return Err(JsonRpcMessageCodecError::MaxLineLengthExceeded);
        }
        output.reserve(serialized.len() + 1);
        output.extend_from_slice(&serialized);
        output.put_u8(b'\n');
        Ok(())
    }
}

struct BoundedFrameReader<R> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
    max_length: usize,
}

enum FrameRead {
    Complete(Vec<u8>),
    Oversized(Vec<u8>),
    Eof,
}

impl<R> BoundedFrameReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R, max_length: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: Vec::new(),
            max_length,
        }
    }

    async fn read_frame(&mut self) -> Result<FrameRead, std::io::Error> {
        self.buffer.clear();
        let mut oversized = false;

        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                self.buffer.clear();
                return Ok(FrameRead::Eof);
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_length = newline.unwrap_or(available.len());
            if !oversized {
                let remaining = self.max_length.saturating_sub(self.buffer.len());
                let copied = content_length.min(remaining);
                self.buffer.extend_from_slice(&available[..copied]);
                oversized = copied < content_length;
            }
            let consumed = newline.map_or(available.len(), |offset| offset + 1);
            self.reader.consume(consumed);

            if newline.is_some() {
                let mut frame = std::mem::take(&mut self.buffer);
                if !oversized && frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                return Ok(if oversized {
                    FrameRead::Oversized(frame)
                } else {
                    FrameRead::Complete(frame)
                });
            }
        }
    }
}

pub struct BoundedStdioTransport<R, W> {
    reader: BoundedFrameReader<R>,
    decoder: JsonRpcMessageCodec<ClientJsonRpcMessage>,
    writer: Arc<Mutex<FramedWrite<W, BoundedJsonRpcMessageCodec<ServerJsonRpcMessage>>>>,
    shutdown: CancellationToken,
}

#[cfg(test)]
pub fn bounded_stdio_transport<R, W>(read: R, write: W) -> BoundedStdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite,
{
    bounded_stdio_transport_with_cancellation(read, write, CancellationToken::new())
}

pub fn bounded_stdio_transport_with_cancellation<R, W>(
    read: R,
    write: W,
    shutdown: CancellationToken,
) -> BoundedStdioTransport<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite,
{
    BoundedStdioTransport {
        reader: BoundedFrameReader::new(read, MAX_INBOUND_MESSAGE_BYTES),
        decoder: JsonRpcMessageCodec::new_with_max_length(MAX_INBOUND_MESSAGE_BYTES),
        writer: Arc::new(Mutex::new(FramedWrite::new(
            write,
            BoundedJsonRpcMessageCodec::new(MAX_OUTBOUND_MESSAGE_BYTES),
        ))),
        shutdown,
    }
}

impl<R, W> BoundedStdioTransport<R, W> {
    #[must_use]
    #[cfg(test)]
    pub fn max_inbound_message_bytes(&self) -> usize {
        self.reader.max_length
    }
}

impl<R, W> Transport<RoleServer> for BoundedStdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = JsonRpcMessageCodecError;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let writer = Arc::clone(&self.writer);

        async move { writer.lock().await.send(item).await }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        loop {
            match self.reader.read_frame().await {
                Ok(FrameRead::Complete(frame)) => {
                    if frame.is_empty() {
                        continue;
                    }
                    if let Some(id) = malformed_tool_call_id(&frame) {
                        let response = ServerJsonRpcMessage::error(
                            ProtocolFailure::new(
                                ProtocolFailureKind::InvalidParams,
                                "tools/call parameters are invalid",
                            )
                            .into_mcp_error(),
                            Some(id),
                        );
                        if let Err(send_error) = self.writer.lock().await.send(response).await {
                            tracing::warn!(
                                %send_error,
                                "failed to report invalid tools/call parameters"
                            );
                            self.shutdown.cancel();
                            return None;
                        }
                        continue;
                    }
                    let mut framed = BytesMut::from(frame.as_slice());
                    framed.put_u8(b'\n');
                    match self.decoder.decode(&mut framed) {
                        Ok(Some(message)) => return Some(message),
                        Ok(None) => {}
                        Err(error) => {
                            let Some((id, kind, message)) = complete_frame_protocol_error(&frame)
                            else {
                                tracing::warn!(%error, "discarding an invalid MCP input frame");
                                continue;
                            };
                            let response = ServerJsonRpcMessage::error(
                                ProtocolFailure::new(kind, message).into_mcp_error(),
                                Some(id),
                            );
                            if let Err(send_error) = self.writer.lock().await.send(response).await {
                                tracing::warn!(
                                    %send_error,
                                    "failed to report an invalid MCP frame"
                                );
                                self.shutdown.cancel();
                                return None;
                            }
                        }
                    }
                }
                Ok(FrameRead::Oversized(prefix)) => {
                    let Some(id) = extract_safe_request_id(&prefix) else {
                        tracing::warn!(
                            limit_bytes = MAX_INBOUND_MESSAGE_BYTES,
                            "closing MCP input after an oversized frame without a safe id"
                        );
                        self.shutdown.cancel();
                        return None;
                    };
                    let error = ServerJsonRpcMessage::error(
                        ProtocolFailure::new(
                            ProtocolFailureKind::InvalidRequest,
                            "MCP input frame exceeds the size limit",
                        )
                        .into_mcp_error(),
                        Some(id),
                    );
                    if let Err(send_error) = self.writer.lock().await.send(error).await {
                        tracing::warn!(%send_error, "failed to report an oversized MCP frame");
                        self.shutdown.cancel();
                        return None;
                    }
                }
                Ok(FrameRead::Eof) => {
                    self.shutdown.cancel();
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to read an MCP input frame");
                    self.shutdown.cancel();
                    return None;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.writer.lock().await.close().await
    }
}

fn malformed_tool_call_id(frame: &[u8]) -> Option<RequestId> {
    let value = serde_json::from_slice::<serde_json::Value>(frame).ok()?;
    let object = value.as_object()?;
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0")
        || object.get("method").and_then(serde_json::Value::as_str) != Some("tools/call")
    {
        return None;
    }
    let id = serde_json::from_value::<RequestId>(object.get("id")?.clone()).ok()?;
    let valid = object
        .get("params")
        .cloned()
        .and_then(|params| {
            serde_json::from_value::<rmcp::model::CallToolRequestParams>(params).ok()
        })
        .is_some();
    (!valid).then_some(id)
}

fn complete_frame_protocol_error(
    frame: &[u8],
) -> Option<(RequestId, ProtocolFailureKind, &'static str)> {
    let value = serde_json::from_slice::<serde_json::Value>(frame).ok()?;
    let object = value.as_object()?;
    let id = serde_json::from_value::<RequestId>(object.get("id")?.clone()).ok()?;
    if object.get("method").and_then(serde_json::Value::as_str) == Some("tools/call") {
        Some((
            id,
            ProtocolFailureKind::InvalidParams,
            "tools/call parameters are invalid",
        ))
    } else {
        Some((
            id,
            ProtocolFailureKind::InvalidRequest,
            "MCP request is invalid",
        ))
    }
}

fn extract_safe_request_id(prefix: &[u8]) -> Option<RequestId> {
    let mut position = skip_json_whitespace(prefix, 0);
    if prefix.get(position) != Some(&b'{') {
        return None;
    }
    position += 1;
    let mut valid_jsonrpc = false;

    loop {
        position = skip_json_whitespace(prefix, position);
        let key_end = json_string_end(prefix, position)?;
        let key = serde_json::from_slice::<String>(&prefix[position..key_end]).ok()?;
        position = skip_json_whitespace(prefix, key_end);
        if prefix.get(position) != Some(&b':') {
            return None;
        }
        position = skip_json_whitespace(prefix, position + 1);
        let value_end = simple_json_value_end(prefix, position)?;
        let value = &prefix[position..value_end];

        if key == "jsonrpc" {
            valid_jsonrpc = serde_json::from_slice::<String>(value).ok().as_deref() == Some("2.0");
        } else if key == "id" && valid_jsonrpc {
            let id = serde_json::from_slice::<RequestId>(value).ok()?;
            let delimiter = prefix.get(skip_json_whitespace(prefix, value_end));
            return matches!(delimiter, Some(b',' | b'}')).then_some(id);
        }

        position = skip_json_whitespace(prefix, value_end);
        match prefix.get(position) {
            Some(b',') => position += 1,
            _ => return None,
        }
    }
}

fn skip_json_whitespace(input: &[u8], mut position: usize) -> usize {
    while input
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        position += 1;
    }
    position
}

fn json_string_end(input: &[u8], start: usize) -> Option<usize> {
    if input.get(start) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (offset, byte) in input[start + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(start + offset + 2);
        }
    }
    None
}

fn simple_json_value_end(input: &[u8], start: usize) -> Option<usize> {
    if input.get(start) == Some(&b'"') {
        return json_string_end(input, start);
    }
    if matches!(input.get(start), Some(b'{' | b'[') | None) {
        return None;
    }
    let length = input[start..]
        .iter()
        .position(|byte| matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(input.len() - start);
    (length > 0).then_some(start + length)
}

#[cfg(test)]
mod tests {
    use super::{BoundedJsonRpcMessageCodec, MAX_INBOUND_MESSAGE_BYTES, bounded_stdio_transport};
    use rmcp::{
        RoleServer,
        model::{CallToolResult, ContentBlock, RequestId, ServerJsonRpcMessage, ServerResult},
        transport::{Transport, async_rw::JsonRpcMessageCodecError},
    };
    use tokio::io::{AsyncWriteExt, duplex, sink};
    use tokio_util::{bytes::BytesMut, codec::Encoder};

    #[tokio::test]
    async fn transport_reports_the_two_mib_inbound_limit() {
        let transport = bounded_stdio_transport(tokio::io::empty(), sink());

        assert_eq!(transport.max_inbound_message_bytes(), 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn valid_frame_at_the_two_mib_boundary_is_accepted() {
        let valid_message = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let mut input = valid_message.to_vec();
        input.resize(MAX_INBOUND_MESSAGE_BYTES, b' ');
        input.push(b'\n');

        let (mut writer, reader) = duplex(input.len());
        writer
            .write_all(&input)
            .await
            .expect("test input must fit the duplex buffer");
        drop(writer);

        let mut transport = bounded_stdio_transport(reader, sink());
        let message = Transport::<RoleServer>::receive(&mut transport).await;

        assert!(message.is_some(), "a valid frame at the limit must pass");
    }

    #[tokio::test]
    async fn oversized_corrupted_frame_closes_before_the_next_message() {
        let valid_message = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let mut input = vec![b' '; MAX_INBOUND_MESSAGE_BYTES + 1];
        input.push(b'\n');
        input.extend_from_slice(valid_message);
        input.push(b'\n');

        let (mut writer, reader) = duplex(input.len());
        writer
            .write_all(&input)
            .await
            .expect("test input must fit the duplex buffer");
        drop(writer);

        let mut transport = bounded_stdio_transport(reader, sink());
        let message = Transport::<RoleServer>::receive(&mut transport).await;

        assert!(
            message.is_none(),
            "an oversized frame without a safely extracted id must close the input"
        );
    }

    #[tokio::test]
    async fn oversized_frame_with_safe_id_gets_protocol_error_and_is_discarded() {
        let prefix = br#"{"jsonrpc":"2.0","id":41,"method":"tools/call","params":{"name":"ask_1c_ai","arguments":{"question":""#;
        let suffix = br#""}}}"#;
        let mut oversized = prefix.to_vec();
        oversized.resize(MAX_INBOUND_MESSAGE_BYTES + 1 - suffix.len(), b'x');
        oversized.extend_from_slice(suffix);
        oversized.push(b'\n');
        oversized.extend_from_slice(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        oversized.push(b'\n');

        let (mut input_writer, input_reader) = duplex(oversized.len());
        let write_input = tokio::spawn(async move {
            input_writer
                .write_all(&oversized)
                .await
                .expect("write oversized input");
        });
        let (output_writer, output_reader) = duplex(1024);
        let mut output_reader = tokio::io::BufReader::new(output_reader);
        let mut transport = bounded_stdio_transport(input_reader, output_writer);

        let next = Transport::<RoleServer>::receive(&mut transport).await;
        write_input.await.expect("input writer task");
        assert!(
            next.is_some(),
            "the frame after an oversized request with a safe id must be processed"
        );

        let mut error_line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut output_reader, &mut error_line)
            .await
            .expect("read protocol error");
        let error: serde_json::Value =
            serde_json::from_str(&error_line).expect("protocol error JSON");
        assert_eq!(error["id"], 41);
        assert_eq!(error["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn malformed_tool_call_with_safe_id_gets_invalid_params_error() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":51,\"method\":\"tools/call\",",
            "\"params\":{\"name\":\"ask_1c_ai\",\"arguments\":[]}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"
        );
        let (mut input_writer, input_reader) = duplex(input.len());
        input_writer
            .write_all(input.as_bytes())
            .await
            .expect("write malformed call");
        drop(input_writer);
        let (output_writer, output_reader) = duplex(1024);
        let mut output_reader = tokio::io::BufReader::new(output_reader);
        let mut transport = bounded_stdio_transport(input_reader, output_writer);

        let next = Transport::<RoleServer>::receive(&mut transport).await;
        assert!(
            next.is_some(),
            "the frame after a malformed request with a safe id must be processed"
        );

        let mut error_line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut output_reader, &mut error_line)
            .await
            .expect("read invalid params error");
        let error: serde_json::Value =
            serde_json::from_str(&error_line).expect("protocol error JSON");
        assert_eq!(error["id"], 51);
        assert_eq!(error["error"]["code"], -32602);
    }

    #[test]
    fn outbound_codec_rejects_oversized_message_without_partial_bytes() {
        let message = ServerJsonRpcMessage::response(
            ServerResult::CallToolResult(CallToolResult::success(vec![ContentBlock::text(
                "x".repeat(MAX_INBOUND_MESSAGE_BYTES),
            )])),
            RequestId::Number(1),
        );
        let mut codec = BoundedJsonRpcMessageCodec::new(MAX_INBOUND_MESSAGE_BYTES);
        let mut output = BytesMut::new();

        let error = codec
            .encode(message, &mut output)
            .expect_err("an oversized response must be rejected");

        assert!(matches!(
            error,
            JsonRpcMessageCodecError::MaxLineLengthExceeded
        ));
        assert!(
            output.is_empty(),
            "no partial JSON-RPC response may reach the writer buffer"
        );
    }

    #[test]
    fn outbound_codec_accepts_a_message_at_the_exact_limit() {
        let empty = ServerJsonRpcMessage::response(
            ServerResult::CallToolResult(CallToolResult::success(vec![ContentBlock::text("")])),
            RequestId::Number(2),
        );
        let base_length = serde_json::to_vec(&empty)
            .expect("base response serializes")
            .len();
        let message = ServerJsonRpcMessage::response(
            ServerResult::CallToolResult(CallToolResult::success(vec![ContentBlock::text(
                "x".repeat(MAX_INBOUND_MESSAGE_BYTES - base_length),
            )])),
            RequestId::Number(2),
        );
        assert_eq!(
            serde_json::to_vec(&message)
                .expect("boundary response serializes")
                .len(),
            MAX_INBOUND_MESSAGE_BYTES
        );
        let mut codec = BoundedJsonRpcMessageCodec::new(MAX_INBOUND_MESSAGE_BYTES);
        let mut output = BytesMut::new();

        codec
            .encode(message, &mut output)
            .expect("a response at the exact limit must pass");

        assert_eq!(output.len(), MAX_INBOUND_MESSAGE_BYTES + 1);
        assert_eq!(output.last(), Some(&b'\n'));
    }
}
