mod server;

use rmcp::{ServiceExt, transport::stdio};
use server::MediaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // stdout belongs to the MCP transport: log to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("media-mcp starting");
    let service = MediaServer.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
