mod common;
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

/// 1 s, 160x120, 10 fps with audio; every call makes the same codecs and size.
const AV: &[&str] = &[
    "-f",
    "lavfi",
    "-i",
    "testsrc=d=1:s=160x120:r=10",
    "-f",
    "lavfi",
    "-i",
    "sine=d=1",
    "-shortest",
    "-g",
    "10",
    "-pix_fmt",
    "yuv420p",
];

fn solid(s: &Server, name: &str, colour: &str, size: &str) -> std::path::PathBuf {
    s.fixture(
        name,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("color={colour}:s={size}"),
            "-frames:v",
            "1",
        ],
    )
}

#[test]
fn slideshow_mixes_images_and_video_with_synced_audio() {
    let Some(mut s) = Server::start() else { return };
    let red = solid(&s, "red.png", "red", "160x120");
    let blue = solid(&s, "blue.png", "blue", "40x120"); // portrait: gets black bars
    let v = s.fixture("v.mp4", AV);
    let r = s.job_ok(
        "video_compose",
        json!({"clips": [
            {"import_path": red, "secs": 1},
            {"import_path": blue},
            {"import_path": v}
        ], "image_secs": 1, "fps": 10}),
    );
    assert_eq!(r["kind"], "video");
    assert_eq!(dims(&r), (160, 120));
    assert!((secs(&r) - 3.0).abs() < 0.3, "duration {}", secs(&r));
    assert_eq!(r["has_audio"], true);
    assert!(path_of(&r).starts_with(s.workspace().canonicalize().unwrap()));

    let px = rgb_pixels(&path_of(&r));
    let any = |f: &dyn Fn(&[u8]) -> bool| px.chunks(3).any(f);
    assert!(
        any(&|c| c[0] > 200 && c[1] < 60 && c[2] < 60),
        "no red frame"
    );
    assert!(
        any(&|c| c[2] > 200 && c[0] < 60 && c[1] < 60),
        "no blue frame"
    );
    // the 40px-wide blue slide is centred in a 160px frame, so its left edge stays black
    let frame = 160 * 120 * 3;
    let mid = &px[(14 * frame)..(15 * frame)]; // inside the second slide (frames 10-19)
    assert!(
        mid[(60 * 160) * 3..(60 * 160) * 3 + 9]
            .iter()
            .all(|&c| c < 30),
        "no bars"
    );
}

#[test]
fn custom_size_and_a_shortened_video() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let r = s.job_ok(
        "video_compose",
        json!({"clips": [{"import_path": v, "secs": 0.5}], "width": 81, "height": 50,
               "export": {"format": "webm", "output_name": "short"}}),
    );
    assert_eq!(dims(&r), (80, 50)); // rounded down to even
    assert!((secs(&r) - 0.5).abs() < 0.2, "duration {}", secs(&r));
    assert_eq!(path_of(&r).file_name().unwrap(), "short.webm");
    assert_eq!(r["video_codec"], "vp9");
}

#[test]
fn stream_copy_joins_matching_videos_without_reencoding() {
    let Some(mut s) = Server::start() else { return };
    let a = s.fixture("a.mp4", AV);
    let b = s.fixture("b.mp4", AV);
    let r = s.job_ok(
        "video_compose",
        json!({"clips": [{"import_path": a}, {"import_path": b}], "export": {"stream_copy": true}}),
    );
    assert!((secs(&r) - 2.0).abs() < 0.3, "duration {}", secs(&r));
    assert_eq!(dims(&r), (160, 120));
    assert_eq!(r["has_audio"], true);
    assert_eq!(r["video_codec"], "h264");

    let big = s.fixture(
        "big.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=320x240:r=10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let e = s.job_err(
        "video_compose",
        json!({"clips": [{"asset_id": r["asset_id"]}, {"import_path": big}], "export": {"stream_copy": true}}),
    );
    assert!(e.contains("identical"), "{e}");
}

#[test]
fn bad_input_is_a_tool_error_without_debris() {
    let Some(mut s) = Server::start() else { return };
    let img = solid(&s, "i.png", "red", "64x48");
    let aud = s.fixture("a.m4a", &["-f", "lavfi", "-i", "sine=d=1"]);
    let v = s.fixture("v.mp4", AV);
    for (args, needle) in [
        (json!({"clips": []}), "at least one"),
        (json!({"clips": [{"import_path": aud}]}), "audio"),
        (json!({"clips": [{"import_path": img}], "fps": 0}), "fps"),
        (
            json!({"clips": [{"import_path": img}], "width": 1}),
            "width",
        ),
        (
            json!({"clips": [{"import_path": img, "secs": 0.01}]}),
            "secs",
        ),
        (
            json!({"clips": [{"import_path": img}], "export": {"stream_copy": true}}),
            "stream_copy",
        ),
        (
            json!({"clips": [{"import_path": v, "secs": 0.5}], "export": {"stream_copy": true}}),
            "stream_copy",
        ),
        (json!({"clips": [{"asset_id": "nope"}]}), "unknown asset"),
        (
            json!({"clips": [{"import_path": img}], "export": {"output_name": "../x"}}),
            "output_name",
        ),
    ] {
        let e = s.job_err("video_compose", args.clone());
        assert!(e.contains(needle), "{args}\n -> {e}");
    }
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}
