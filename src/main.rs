// Sandbox/asset/engine plumbing is only partly used until the photo and video tools land (phases 3-5); remove then.
#![allow(dead_code)]

mod assets;
mod engine;
mod error;
mod sandbox;
mod server;
mod tools;

use assets::Assets;
use engine::Engines;
use rmcp::{ServiceExt, transport::stdio};
use sandbox::Sandbox;
use server::MediaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // stdout belongs to the MCP transport: log to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let workspace = std::env::var("MEDIA_MCP_WORKSPACE").unwrap_or_else(|_| "./workspace".into());
    let sandbox = Sandbox::new(&workspace).await?;
    let engines = Engines::detect()?;
    tracing::info!(workspace = %sandbox.root().display(), ?engines, "media-mcp starting");
    let service = MediaServer::new(Assets::new(sandbox), engines)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
