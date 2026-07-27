mod config;
mod error;
mod limits;
mod mcp;
mod naparnik;

use clap::Parser;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "onec-buddy-mcp",
    about = "Local stdio MCP server for 1C:Enterprise development assistance",
    version
)]
struct Cli;

#[tokio::main]
async fn main() -> ExitCode {
    let _cli = Cli::parse();

    let config = match config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = config.init_tracing() {
        eprintln!("configuration error: {error}");
        return ExitCode::FAILURE;
    }
    tracing::debug!("configuration validated");

    let client = match naparnik::NaparnikClient::new(config.token()) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("client error: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = mcp::run_stdio(&config, client).await {
        eprintln!("server error: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
