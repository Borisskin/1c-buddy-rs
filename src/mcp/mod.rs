//! MCP stdio transport and request handling.

mod catalog;
mod codec;
mod service;
mod text;

use rmcp::{ServiceExt, service::ServerInitializeError};
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use self::codec::bounded_stdio_transport_with_cancellation;
use self::service::{BuddyServer, NaparnikExecutor};
use crate::config::Config;
use crate::naparnik::NaparnikClient;

#[derive(Debug, Error)]
pub(crate) enum McpRuntimeError {
    #[error("MCP initialization failed")]
    Initialization,
    #[error("MCP service task failed")]
    ServiceTask,
}

pub(crate) async fn run_stdio(
    config: &Config,
    client: NaparnikClient,
) -> Result<(), McpRuntimeError> {
    debug_assert_eq!(
        crate::limits::SHUTDOWN_GRACE_PERIOD,
        Duration::from_secs(5),
        "rmcp 2.2.0 is pinned to the same five-second EOF drain"
    );
    let shutdown = CancellationToken::new();
    let transport = bounded_stdio_transport_with_cancellation(
        tokio::io::stdin(),
        tokio::io::stdout(),
        shutdown.clone(),
    );
    let executor = NaparnikExecutor::new(client, config.ui_language());
    let server = BuddyServer::new(config, executor, shutdown.clone());
    let running = match server.serve_with_ct(transport, shutdown.clone()).await {
        Ok(running) => running,
        Err(ServerInitializeError::Cancelled | ServerInitializeError::ConnectionClosed(_))
            if shutdown.is_cancelled() =>
        {
            return Ok(());
        }
        Err(_error) => return Err(McpRuntimeError::Initialization),
    };
    running
        .waiting()
        .await
        .map_err(|_error| McpRuntimeError::ServiceTask)?;
    Ok(())
}
