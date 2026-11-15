// Sandbox/asset/engine plumbing is only partly used until the photo and video tools land (phases 3-5); remove then.
#![allow(dead_code)]

mod assets;
mod engine;
mod error;
mod jobs;
mod sandbox;
mod server;
mod tools;

use assets::Assets;
use engine::Engines;
use jobs::Jobs;
use rmcp::{ServiceExt, transport::stdio};
use sandbox::Sandbox;
use server::MediaServer;

/// Numeric env var with a default; an unparseable value is logged and ignored.
fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            tracing::warn!("{name}={v:?} is not a number, using {default}");
            default
        }),
        Err(_) => default,
    }
}

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
    let jobs = Jobs::new(
        env_u64("MEDIA_MCP_MAX_JOBS", 2) as usize,
        std::time::Duration::from_secs(env_u64("MEDIA_MCP_JOB_TIMEOUT_SECS", 600)),
    );
    tracing::info!(workspace = %sandbox.root().display(), ?engines, "media-mcp starting");
    let service = MediaServer::new(Assets::new(sandbox), engines, jobs)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
