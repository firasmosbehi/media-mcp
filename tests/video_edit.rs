mod common;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use common::{Server, debris, rgb_pixels};
use serde_json::{Value, json};

fn path_of(v: &Value) -> std::path::PathBuf {
    v["path"].as_str().unwrap().into()
}

fn dims(v: &Value) -> (u64, u64) {
    (v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap())
}

fn secs(v: &Value) -> f64 {
    v["duration_secs"].as_f64().unwrap()
}

/// 2 s, 160x120, 10 fps, with a sine audio track and a keyframe every second.
const AV: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc=d=2:s=160x120:r=10",
    "-f",
    "lavfi",
    "-i",
    "sine=d=2",
    "-shortest",
    "-g",
    "10",
    "-pix_fmt",
    "yuv420p",
];

#[test]
fn trim_resize_speed_keep_audio_in_sync() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let r = s.job_ok(
        "video_edit",
        json!({"import_path": v, "operations": [
            {"op":"trim","start_secs":0.5,"end_secs":1.5},
            {"op":"resize","width":80},
            {"op":"speed","factor":2}
        ]}),
    );
    assert_eq!(r["kind"], "video");
    assert_eq!(dims(&r), (80, 60));
    assert!((secs(&r) - 0.5).abs() < 0.15, "duration {}", secs(&r));
    assert_eq!(r["has_audio"], true);
    assert_eq!(r["video_codec"], "h264");
    assert!(path_of(&r).extension().unwrap() == "mp4");
    assert!(path_of(&r).starts_with(s.workspace().canonicalize().unwrap()));
}

#[test]
fn geometry_color_and_silent_input() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture(
        "silent.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=160x120:r=10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let run = |s: &mut Server, ops: Value| {
        s.job_ok("video_edit", json!({"import_path": v, "operations": ops}))
    };

    assert_eq!(
        dims(&run(&mut s, json!([{"op":"rotate","degrees":90}]))),
        (120, 160)
    );
    assert_eq!(
        dims(&run(
            &mut s,
            json!([{"op":"crop","x":10,"y":10,"width":100,"height":50}])
        )),
        (100, 50)
    );
    // Odd sizes are rounded to even for H.264.
    assert_eq!(
        dims(&run(
            &mut s,
            json!([{"op":"resize","width":101,"height":51}])
        )),
        (100, 50)
    );
    let slow = run(
        &mut s,
        json!([{"op":"speed","factor":0.5},{"op":"flip","direction":"vertical"}]),
    );
    assert!((secs(&slow) - 2.0).abs() < 0.2);
    assert_eq!(slow["has_audio"], false);

    // Saturation 0 -> every pixel of every frame is grey.
    let g = run(&mut s, json!([{"op":"color","saturation":0}]));
    let bad = rgb_pixels(&path_of(&g))
        .chunks(3)
        .filter(|c| (c[0] as i32 - c[1] as i32).abs() > 6 || (c[1] as i32 - c[2] as i32).abs() > 6)
        .count();
    assert_eq!(bad, 0, "non-grey pixels");
}

#[test]
fn overlay_and_text_are_drawn() {
    let Some(mut s) = Server::start() else { return };
    let black = s.fixture(
        "black.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=black:d=1:s=160x120:r=10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let red = s.fixture(
        "red.png",
        &["-f", "lavfi", "-i", "color=red:s=40x40", "-frames:v", "1"],
    );
    let r = s.job_ok(
        "video_edit",
        json!({"import_path": black, "operations": [
            {"op":"overlay","import_path": red,"x":10,"y":10},
            {"op":"text","text":"Hi 100% %{localtime} 'q'","x":60,"y":50,"size":40}
        ]}),
    );
    let px = rgb_pixels(&path_of(&r));
    let w = 160;
    let at = |x: usize, y: usize| {
        let i = (y * w + x) * 3;
        [px[i], px[i + 1], px[i + 2]]
    };
    let [rr, g, b] = at(30, 30);
    assert!(rr > 200 && g < 60 && b < 60, "overlay pixel {rr},{g},{b}");
    assert!(
        at(150, 110).iter().all(|&c| c < 30),
        "background should stay black"
    );
    assert!(
        px.chunks(3).any(|c| c.iter().all(|&v| v > 200)),
        "no white text pixels"
    );
}

#[test]
fn stream_copy_trim_is_fast_and_lossless() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let r = s.job_ok(
        "video_edit",
        json!({"import_path": v, "operations": [{"op":"trim","start_secs":1.0}], "export": {"stream_copy": true}}),
    );
    assert!((secs(&r) - 1.0).abs() < 0.35, "duration {}", secs(&r));
    assert_eq!(dims(&r), (160, 120));
    assert_eq!(r["has_audio"], true);

    let e = s.job_err(
        "video_edit",
        json!({"asset_id": r["asset_id"], "operations": [{"op":"resize","width":80}], "export": {"stream_copy": true}}),
    );
    assert!(e.contains("stream_copy"), "{e}");
}

#[test]
fn webm_export_and_naming() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture(
        "v.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x48:r=10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let r = s.job_ok(
        "video_edit",
        json!({"import_path": v, "export": {"format": "webm", "output_name": "clip1"}}),
    );
    assert_eq!(path_of(&r).file_name().unwrap(), "clip1.webm");
    assert_eq!(r["video_codec"], "vp9");
    let dup = s.job_err(
        "video_edit",
        json!({"asset_id": r["asset_id"], "export": {"format":"webm","output_name":"clip1"}}),
    );
    assert!(dup.contains("overwrite"), "{dup}");
    s.job_ok("video_edit", json!({"asset_id": r["asset_id"], "export": {"format":"webm","output_name":"clip1","overwrite":true}}));
}

#[test]
fn errors_leave_no_debris() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let id = s.ok("probe", json!({"import_path": v}))["asset_id"].clone();
    let img = s.fixture(
        "i.png",
        &["-f", "lavfi", "-i", "testsrc=s=32x32", "-frames:v", "1"],
    );
    let aud = s.fixture("a.m4a", &["-f", "lavfi", "-i", "sine=d=1"]);

    for (args, needle) in [
        (
            json!({"asset_id": id, "operations": [{"op":"trim","start_secs":5}]}),
            "past the end",
        ),
        (
            json!({"asset_id": id, "operations": [{"op":"rotate","degrees":45}]}),
            "90, 180 or 270",
        ),
        (
            json!({"asset_id": id, "operations": [{"op":"speed","factor":0}]}),
            "factor",
        ),
        (
            json!({"asset_id": id, "operations": [{"op":"resize","width":99999}]}),
            "between 1 and 4096",
        ),
        (json!({"asset_id": id, "export": {"crf": 99}}), "crf"),
        (
            json!({"asset_id": id, "export": {"output_name": "../x"}}),
            "output_name",
        ),
        (json!({"import_path": img}), "only handles videos"),
        (json!({"import_path": aud}), "only handles videos"),
        (json!({"asset_id": "nope"}), "unknown asset"),
        // ffmpeg itself rejects a 1x1 frame (rounded to 0x0 for H.264): the error carries its stderr.
        (
            json!({"asset_id": id, "operations": [{"op":"resize","width":1,"height":1}]}),
            "ffmpeg failed",
        ),
    ] {
        let e = s.job_err("video_edit", args.clone());
        assert!(e.contains(needle), "{args}\n -> {e}");
    }
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}

#[test]
fn preview_shows_a_video_frame() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture(
        "v.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=2:s=2000x1000:r=5",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let r = s.tool("preview", json!({"import_path": v, "at_secs": 1.0}));
    assert_ne!(r["isError"], true, "{r}");
    let img = r["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .expect("image content");
    let jpg = s.dir.join("p.jpg");
    std::fs::write(
        &jpg,
        STANDARD.decode(img["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        dims(&s.ok("probe", json!({"import_path": jpg}))),
        (1024, 512)
    );
    // default time works, an out-of-range time is a tool error
    assert_ne!(
        s.tool("preview", json!({"import_path": v}))["isError"],
        true
    );
    assert_eq!(
        s.tool("preview", json!({"import_path": v, "at_secs": 99}))["isError"],
        true
    );
    assert!(debris(&s.workspace()).is_empty());
}
