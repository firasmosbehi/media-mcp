//! End-to-end: spawn the server binary, speak MCP over stdio, import a generated fixture and probe it.
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok()
}

fn call(w: &mut impl Write, r: &mut impl BufRead, id: u64, method: &str, params: Value) -> Value {
    let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    writeln!(w, "{req}").unwrap();
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn make_fixture(args: &[&str], out: &Path) {
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .arg(out)
        .status()
        .unwrap()
        .success();
    assert!(ok, "fixture generation failed");
}

#[test]
fn probe_video_image_and_errors() {
    if !(have("ffmpeg") && have("vips")) {
        eprintln!("skipping: ffmpeg or vips not installed");
        return;
    }
    let dir = std::env::temp_dir().join(format!("media-mcp-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let vid = dir.join("v.mp4");
    let img = dir.join("i.png");
    make_fixture(
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
        &vid,
    );
    make_fixture(
        &["-f", "lavfi", "-i", "testsrc=s=32x32", "-frames:v", "1"],
        &img,
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_media-mcp"))
        .env("MEDIA_MCP_WORKSPACE", dir.join("ws"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut w = child.stdin.take().unwrap();
    let mut r = BufReader::new(child.stdout.take().unwrap());

    call(
        &mut w,
        &mut r,
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}),
    );
    writeln!(
        w,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();

    let tools = call(&mut w, &mut r, 2, "tools/list", json!({}));
    assert_eq!(tools["result"]["tools"][0]["name"], "probe");

    let probe = |w: &mut _, r: &mut _, id, args: Value| {
        call(
            w,
            r,
            id,
            "tools/call",
            json!({"name":"probe","arguments":args}),
        )["result"]
            .clone()
    };

    let v = probe(&mut w, &mut r, 3, json!({"import_path": vid}));
    let s = &v["structuredContent"];
    assert_eq!(
        (
            s["kind"].as_str(),
            s["width"].as_u64(),
            s["height"].as_u64()
        ),
        (Some("video"), Some(64), Some(48))
    );
    assert_eq!(s["has_audio"], true);
    assert!((s["duration_secs"].as_f64().unwrap() - 1.0).abs() < 0.2);

    // Re-probe by asset id.
    let id = s["asset_id"].as_str().unwrap().to_string();
    let again = probe(&mut w, &mut r, 4, json!({"asset_id": id}));
    assert_eq!(again["structuredContent"]["kind"], "video");

    let i = probe(&mut w, &mut r, 5, json!({"import_path": img}));
    assert_eq!(i["structuredContent"]["kind"], "image");
    assert!(i["structuredContent"]["duration_secs"].is_null());

    // Tool-level errors, not protocol errors.
    for bad in [
        json!({}),
        json!({"asset_id":"nope"}),
        json!({"import_path":"http://x/y.mp4"}),
        json!({"import_path": "/etc/hostname"}),
    ] {
        let e = probe(&mut w, &mut r, 9, bad.clone());
        assert_eq!(e["isError"], true, "{bad}");
    }

    drop(w);
    let _ = child.wait();
    std::fs::remove_dir_all(dir).unwrap();
}
