//! Job queue: a bounded worker pool, status, progress and cancellation.
//!
//! A job is any future returning a JSON result. Cancelling (or timing out) drops the future,
//! which kills child processes (`kill_on_drop`) and runs `Scratch` guards that delete partial output.
//! ponytail: finished jobs are kept in memory (newest 100); IDs do not survive a restart.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tokio::{sync::Semaphore, task::AbortHandle};

use crate::error::{Error, Result};

const KEEP_FINISHED: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for a free worker.
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_done(self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }
}

/// Progress of one job, 0.0 to 1.0, shared between the job and its observers.
#[derive(Debug, Clone, Default)]
pub struct Progress(Arc<AtomicU32>);

impl Progress {
    pub fn set(&self, fraction: f32) {
        let v = (fraction.clamp(0.0, 1.0) * 10_000.0) as u32;
        self.0.store(v, Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        self.0.load(Ordering::Relaxed) as f32 / 10_000.0
    }
}

/// Snapshot of a job, as returned by `job_status`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct JobStatus {
    pub job_id: String,
    pub state: JobState,
    /// 0.0 to 1.0.
    pub progress: f32,
    /// Seconds since the job was submitted.
    pub elapsed_secs: f64,
    /// Present once the job succeeded: the same object the tool would have returned directly.
    pub result: Option<Value>,
    /// Present when the job failed or timed out: the message, including engine stderr.
    pub error: Option<String>,
}

struct Data {
    state: JobState,
    result: Option<Value>,
    error: Option<String>,
    finished: Option<Instant>,
}

struct Job {
    submitted: Instant,
    progress: Progress,
    data: Mutex<Data>,
    abort: Mutex<Option<AbortHandle>>,
}

struct Inner {
    workers: Arc<Semaphore>,
    timeout: Duration,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

#[derive(Clone)]
pub struct Jobs(Arc<Inner>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // No invariant spans a lock, so a poisoned guard is still usable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Jobs {
    pub fn new(max_jobs: usize, timeout: Duration) -> Self {
        Jobs(Arc::new(Inner {
            workers: Arc::new(Semaphore::new(max_jobs.max(1))),
            timeout,
            jobs: Mutex::default(),
        }))
    }

    /// Queue a job and return immediately. `make` runs once a worker is free.
    pub fn submit<F, Fut>(&self, make: F) -> JobStatus
    where
        F: FnOnce(Progress) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        let id = uuid::Uuid::new_v4().to_string();
        let job = Arc::new(Job {
            submitted: Instant::now(),
            progress: Progress::default(),
            data: Mutex::new(Data {
                state: JobState::Queued,
                result: None,
                error: None,
                finished: None,
            }),
            abort: Mutex::new(None),
        });
        self.prune_and_insert(id.clone(), job.clone());

        let (workers, timeout) = (self.0.workers.clone(), self.0.timeout);
        let task_job = job.clone();
        let handle = tokio::spawn(async move {
            let Ok(_permit) = workers.acquire_owned().await else {
                return;
            };
            {
                let mut d = lock(&task_job.data);
                if d.state != JobState::Queued {
                    return; // cancelled while waiting
                }
                d.state = JobState::Running;
            }
            let outcome = tokio::time::timeout(timeout, make(task_job.progress.clone())).await;
            let mut d = lock(&task_job.data);
            if d.state != JobState::Running {
                return; // cancelled while finishing
            }
            match outcome {
                Ok(Ok(v)) => {
                    d.state = JobState::Succeeded;
                    d.result = Some(v);
                    task_job.progress.set(1.0);
                }
                Ok(Err(e)) => {
                    d.state = JobState::Failed;
                    d.error = Some(e.to_string());
                }
                Err(_) => {
                    d.state = JobState::Failed;
                    d.error = Some(format!("timed out after {}s", timeout.as_secs()));
                }
            }
            d.finished = Some(Instant::now());
        });
        *lock(&job.abort) = Some(handle.abort_handle());
        snapshot(&id, &job)
    }

    pub fn status(&self, id: &str) -> Result<JobStatus> {
        let job = self.get(id)?;
        Ok(snapshot(id, &job))
    }

    /// Cancel a queued or running job. Finished jobs are left as they are.
    pub fn cancel(&self, id: &str) -> Result<JobStatus> {
        let job = self.get(id)?;
        {
            let mut d = lock(&job.data);
            if !d.state.is_done() {
                d.state = JobState::Cancelled;
                d.finished = Some(Instant::now());
            }
        }
        if let Some(h) = lock(&job.abort).as_ref() {
            h.abort(); // no-op if the task already ended
        }
        Ok(snapshot(id, &job))
    }

    fn get(&self, id: &str) -> Result<Arc<Job>> {
        lock(&self.0.jobs)
            .get(id)
            .cloned()
            .ok_or_else(|| Error::JobNotFound(id.to_string()))
    }

    fn prune_and_insert(&self, id: String, job: Arc<Job>) {
        let mut map = lock(&self.0.jobs);
        let mut finished: Vec<(Instant, String)> = map
            .iter()
            .filter_map(|(k, j)| lock(&j.data).finished.map(|t| (t, k.clone())))
            .collect();
        if finished.len() >= KEEP_FINISHED {
            finished.sort();
            for (_, k) in finished.iter().take(finished.len() + 1 - KEEP_FINISHED) {
                map.remove(k);
            }
        }
        map.insert(id, job);
    }
}

fn snapshot(id: &str, job: &Job) -> JobStatus {
    let d = lock(&job.data);
    JobStatus {
        job_id: id.to_string(),
        state: d.state,
        progress: job.progress.get(),
        elapsed_secs: job.submitted.elapsed().as_secs_f64(),
        result: d.result.clone(),
        error: d.error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    async fn until(jobs: &Jobs, id: &str, f: impl Fn(&JobStatus) -> bool) -> JobStatus {
        for _ in 0..200 {
            let s = jobs.status(id).unwrap();
            if f(&s) {
                return s;
            }
            tokio::time::sleep(MS(10)).await;
        }
        panic!("condition not reached: {:?}", jobs.status(id).unwrap());
    }

    #[test]
    fn progress_clamps() {
        let p = Progress::default();
        p.set(0.5);
        assert_eq!(p.get(), 0.5);
        p.set(7.0);
        assert_eq!(p.get(), 1.0);
        p.set(-1.0);
        assert_eq!(p.get(), 0.0);
    }

    #[tokio::test]
    async fn success_and_failure_are_reported() {
        let jobs = Jobs::new(2, Duration::from_secs(5));
        let ok = jobs.submit(|p| async move {
            p.set(0.5);
            Ok(json!({"x": 1}))
        });
        let s = until(&jobs, &ok.job_id, |s| s.state.is_done()).await;
        assert_eq!(s.state, JobState::Succeeded);
        assert_eq!(s.result, Some(json!({"x": 1})));
        assert_eq!(s.progress, 1.0);

        let bad = jobs.submit(|_| async { Err(Error::InvalidInput("boom".into())) });
        let s = until(&jobs, &bad.job_id, |s| s.state.is_done()).await;
        assert_eq!(s.state, JobState::Failed);
        assert!(s.error.unwrap().contains("boom"));
        assert!(jobs.status("nope").is_err());
        assert!(jobs.cancel("nope").is_err());
    }

    #[tokio::test]
    async fn worker_pool_is_bounded() {
        let jobs = Jobs::new(1, Duration::from_secs(5));
        let slow = || async {
            tokio::time::sleep(MS(150)).await;
            Ok(json!(null))
        };
        let a = jobs.submit(move |_| slow());
        let b = jobs.submit(move |_| slow());
        until(&jobs, &a.job_id, |s| s.state == JobState::Running).await;
        assert_eq!(jobs.status(&b.job_id).unwrap().state, JobState::Queued);
        until(&jobs, &b.job_id, |s| s.state == JobState::Succeeded).await;
    }

    /// Sets a flag when dropped, like a child process being killed.
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cancel_running_drops_the_future() {
        let jobs = Jobs::new(1, Duration::from_secs(5));
        let dropped = Arc::new(AtomicBool::new(false));
        let d = dropped.clone();
        let j = jobs.submit(move |_| async move {
            let _g = DropFlag(d);
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(json!(null))
        });
        until(&jobs, &j.job_id, |s| s.state == JobState::Running).await;
        assert_eq!(jobs.cancel(&j.job_id).unwrap().state, JobState::Cancelled);
        for _ in 0..100 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(MS(10)).await;
        }
        assert!(dropped.load(Ordering::SeqCst), "future was not dropped");
        // Stays cancelled; cancelling again is harmless.
        assert_eq!(jobs.cancel(&j.job_id).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn cancel_queued_job_never_runs() {
        let jobs = Jobs::new(1, Duration::from_secs(5));
        jobs.submit(|_| async {
            tokio::time::sleep(MS(200)).await;
            Ok(json!(null))
        });
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        let q = jobs.submit(move |_| async move {
            r.store(true, Ordering::SeqCst);
            Ok(json!(null))
        });
        assert_eq!(jobs.cancel(&q.job_id).unwrap().state, JobState::Cancelled);
        tokio::time::sleep(MS(400)).await;
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(jobs.status(&q.job_id).unwrap().state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn cancel_after_success_keeps_result() {
        let jobs = Jobs::new(1, Duration::from_secs(5));
        let j = jobs.submit(|_| async { Ok(json!(1)) });
        until(&jobs, &j.job_id, |s| s.state.is_done()).await;
        let s = jobs.cancel(&j.job_id).unwrap();
        assert_eq!((s.state, s.result), (JobState::Succeeded, Some(json!(1))));
    }

    #[tokio::test]
    async fn timeout_fails_the_job() {
        let jobs = Jobs::new(1, MS(100));
        let j = jobs.submit(|_| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(json!(null))
        });
        let s = until(&jobs, &j.job_id, |s| s.state.is_done()).await;
        assert_eq!(s.state, JobState::Failed);
        assert!(s.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn only_recent_finished_jobs_are_kept() {
        let jobs = Jobs::new(4, Duration::from_secs(5));
        let first = jobs.submit(|_| async { Ok(json!(0)) });
        until(&jobs, &first.job_id, |s| s.state.is_done()).await;
        for _ in 0..KEEP_FINISHED + 5 {
            let j = jobs.submit(|_| async { Ok(json!(0)) });
            until(&jobs, &j.job_id, |s| s.state.is_done()).await;
        }
        assert!(jobs.status(&first.job_id).is_err());
        assert!(lock(&jobs.0.jobs).len() <= KEEP_FINISHED + 1);
    }
}
