use std::time::Duration;

use rmcp::{
    Peer, RoleServer,
    model::{ProgressNotificationParam, ProgressToken},
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{error::Result, jobs::JobStatus, server::MediaServer};

const MAX_WAIT_SECS: u32 = 30;
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobStatusInput {
    /// ID returned by the tool that started the job.
    pub job_id: String,
    /// Block up to this many seconds (max 30) for the job to finish before answering.
    /// Default 0 returns the current state immediately. Prefer waiting over rapid polling.
    pub wait_secs: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobCancelInput {
    /// ID of the job to cancel.
    pub job_id: String,
}

/// Where to send MCP progress notifications, when the request carried a progress token.
pub type ProgressSink = Option<(Peer<RoleServer>, ProgressToken)>;

pub async fn status(
    server: &MediaServer,
    input: JobStatusInput,
    sink: ProgressSink,
) -> Result<JobStatus> {
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(u64::from(input.wait_secs.unwrap_or(0).min(MAX_WAIT_SECS)));
    let mut last_sent = -1.0_f32;
    loop {
        let st = server.jobs.status(&input.job_id)?;
        if let Some((peer, token)) = &sink
            && st.progress > last_sent
        {
            last_sent = st.progress;
            // A failed notification must not fail the call.
            let _ = peer
                .notify_progress(
                    ProgressNotificationParam::new(token.clone(), f64::from(st.progress))
                        .with_total(1.0),
                )
                .await;
        }
        if st.state.is_done() || tokio::time::Instant::now() >= deadline {
            return Ok(st);
        }
        tokio::time::sleep(POLL).await;
    }
}

pub fn cancel(server: &MediaServer, input: JobCancelInput) -> Result<JobStatus> {
    server.jobs.cancel(&input.job_id)
}
