mod common;
use common::{Server, debris, processes_mentioning};
use serde_json::json;
use std::time::{Duration, Instant};

/// A flat 6000x6000 image and a chain of blurs: several seconds of vips work.
fn slow_edit(s: &mut Server) -> serde_json::Value {
    let big = s.fixture(
        "big.png",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=6000x6000",
            "-frames:v",
            "1",
        ],
    );
    let asset = s.ok("probe", json!({"import_path": big}))["asset_id"].clone();
    let blurs: Vec<_> = (0..6).map(|_| json!({"op":"blur","sigma":20})).collect();
    json!({"asset_id": asset, "operations": blurs})
}

#[test]
fn job_returns_immediately_and_completes() {
    let Some(mut s) = Server::start() else { return };
    let img = s.fixture(
        "a.png",
        &["-f", "lavfi", "-i", "testsrc=s=200x100", "-frames:v", "1"],
    );
    let t = Instant::now();
    let st = s.ok(
        "image_edit",
        json!({"import_path": img, "operations": [{"op":"rotate","degrees":90}]}),
    );
    assert!(t.elapsed() < Duration::from_millis(1500));
    let id = st["job_id"].as_str().unwrap().to_string();
    assert!(matches!(st["state"].as_str(), Some("queued" | "running")));

    let done = s.wait_job(&id);
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["progress"], 1.0);
    assert_eq!(
        (
            done["result"]["width"].as_u64(),
            done["result"]["height"].as_u64()
        ),
        (Some(100), Some(200))
    );
    assert!(done["result"]["asset_id"].is_string());
    assert!(done["error"].is_null());

    assert_eq!(
        s.tool("job_status", json!({"job_id": "nope"}))["isError"],
        true
    );
    assert_eq!(
        s.tool("job_cancel", json!({"job_id": "nope"}))["isError"],
        true
    );
    // Cancelling a finished job changes nothing.
    assert_eq!(
        s.ok("job_cancel", json!({"job_id": id}))["state"],
        "succeeded"
    );
}

#[test]
fn progress_notifications_during_wait() {
    let Some(mut s) = Server::start() else { return };
    let args = slow_edit(&mut s);
    let id = s.start_job("image_edit", args);
    let r = s.request(
        "tools/call",
        json!({"name":"job_status","arguments":{"job_id": id, "wait_secs": 30},"_meta":{"progressToken":"tok-1"}}),
    );
    assert_eq!(
        r["result"]["structuredContent"]["state"], "succeeded",
        "{r}"
    );
    let seen: Vec<f64> = s
        .notifications
        .iter()
        .filter(|n| {
            n["method"] == "notifications/progress" && n["params"]["progressToken"] == "tok-1"
        })
        .map(|n| n["params"]["progress"].as_f64().unwrap())
        .collect();
    assert!(
        seen.len() >= 2,
        "expected several progress notifications, got {seen:?}"
    );
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "progress must increase: {seen:?}"
    );
    assert!(seen.iter().all(|p| (0.0..=1.0).contains(p)));
}

#[test]
fn cancel_kills_process_and_removes_partial_output() {
    let Some(mut s) = Server::start() else { return };
    let args = slow_edit(&mut s);
    let ws = s.workspace().to_string_lossy().into_owned();
    let id = s.start_job("image_edit", args);

    // Wait until vips is really running.
    let t = Instant::now();
    while processes_mentioning("vips") == 0 && cfg!(target_os = "linux") {
        assert!(t.elapsed() < Duration::from_secs(10), "vips never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = s.ok("job_cancel", json!({"job_id": id}));
    assert_eq!(st["state"], "cancelled");

    // The child is killed and scratch files disappear shortly after.
    let t = Instant::now();
    loop {
        if processes_mentioning(&ws) == 0 && debris(&s.workspace()).is_empty() {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "after cancel: {} processes, debris {:?}",
            processes_mentioning(&ws),
            debris(&s.workspace())
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let st = s.ok("job_status", json!({"job_id": id}));
    assert_eq!(st["state"], "cancelled");
    assert!(st["result"].is_null());
}
