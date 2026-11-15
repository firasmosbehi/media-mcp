mod common;
use common::Server;
use serde_json::json;

#[test]
fn probe_video_image_and_errors() {
    let Some(mut s) = Server::start() else { return };
    let vid = s.fixture(
        "v.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x48:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=d=1",
            "-shortest",
        ],
    );
    let img = s.fixture(
        "i.png",
        &["-f", "lavfi", "-i", "testsrc=s=32x32", "-frames:v", "1"],
    );

    let names: Vec<String> = s.request("tools/list", json!({}))["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"probe".to_string()));

    let v = s.ok("probe", json!({"import_path": vid}));
    assert_eq!(
        (
            v["kind"].as_str(),
            v["width"].as_u64(),
            v["height"].as_u64()
        ),
        (Some("video"), Some(64), Some(48))
    );
    assert_eq!(v["has_audio"], true);
    assert!((v["duration_secs"].as_f64().unwrap() - 1.0).abs() < 0.2);

    // Re-probe by asset id.
    let id = v["asset_id"].as_str().unwrap().to_string();
    assert_eq!(s.ok("probe", json!({"asset_id": id}))["kind"], "video");

    let i = s.ok("probe", json!({"import_path": img}));
    assert_eq!(i["kind"], "image");
    assert!(i["duration_secs"].is_null());

    // Tool-level errors, not protocol errors.
    for bad in [
        json!({}),
        json!({"asset_id":"nope"}),
        json!({"import_path":"http://x/y.mp4"}),
        json!({"import_path": "/etc/hostname"}),
    ] {
        assert_eq!(s.tool("probe", bad.clone())["isError"], true, "{bad}");
    }
}
