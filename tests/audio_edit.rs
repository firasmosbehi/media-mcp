mod common;
use common::{Server, debris, mean_volume_db};
use serde_json::{Value, json};

fn path_of(v: &Value) -> std::path::PathBuf {
    v["path"].as_str().unwrap().into()
}

fn secs(v: &Value) -> f64 {
    v["duration_secs"].as_f64().unwrap()
}

/// 2 s, 160x120 video with a sine audio track.
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
    "-pix_fmt",
    "yuv420p",
];

#[test]
fn extracts_trims_and_changes_volume_and_speed() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);

    // No operations: pull the audio out of a video.
    let plain = s.job_ok("audio_edit", json!({"import_path": v}));
    assert_eq!(plain["kind"], "audio");
    assert_eq!(plain["audio_codec"], "mp3");
    assert!((secs(&plain) - 2.0).abs() < 0.2, "{}", secs(&plain));
    assert!(path_of(&plain).extension().unwrap() == "mp3");
    assert!(path_of(&plain).starts_with(s.workspace().canonicalize().unwrap()));

    // volume 0.1 is 20 dB quieter.
    let quiet = s.job_ok(
        "audio_edit",
        json!({"asset_id": plain["asset_id"], "operations": [{"op":"volume","factor":0.1}]}),
    );
    let drop = mean_volume_db(&path_of(&plain)) - mean_volume_db(&path_of(&quiet));
    assert!((drop - 20.0).abs() < 3.0, "volume dropped {drop} dB");

    // trim 0.5..1.5 then 2x speed -> 0.5 s; fades keep it running.
    let r = s.job_ok(
        "audio_edit",
        json!({"asset_id": plain["asset_id"], "operations": [
            {"op":"trim","start_secs":0.5,"end_secs":1.5},
            {"op":"speed","factor":2},
            {"op":"fade_in","secs":0.1},
            {"op":"fade_out","secs":0.1}
        ], "export": {"format": "wav", "output_name": "cut"}}),
    );
    assert!((secs(&r) - 0.5).abs() < 0.1, "{}", secs(&r));
    assert_eq!(r["audio_codec"], "pcm_s16le");
    assert_eq!(path_of(&r).file_name().unwrap(), "cut.wav");
    // The faded edges pull the average below the unfaded clip.
    let unfaded = s.job_ok(
        "audio_edit",
        json!({"asset_id": plain["asset_id"], "operations": [
            {"op":"trim","start_secs":0.5,"end_secs":1.5},{"op":"speed","factor":2}
        ], "export": {"format": "wav"}}),
    );
    assert!(mean_volume_db(&path_of(&r)) < mean_volume_db(&path_of(&unfaded)) - 0.5);

    let m4a = s.job_ok(
        "audio_edit",
        json!({"asset_id": plain["asset_id"], "export": {"format": "m4a"}}),
    );
    assert_eq!(m4a["audio_codec"], "aac");
    assert!(path_of(&m4a).extension().unwrap() == "m4a");
}

#[test]
fn bad_input_is_a_tool_error_without_debris() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let id = s.job_ok("audio_edit", json!({"import_path": v}))["asset_id"].clone();
    let silent = s.fixture(
        "silent.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=d=1:s=64x48:r=10",
            "-pix_fmt",
            "yuv420p",
        ],
    );
    let img = s.fixture(
        "i.png",
        &["-f", "lavfi", "-i", "testsrc=s=32x32", "-frames:v", "1"],
    );
    for (args, needle) in [
        (json!({"import_path": silent}), "no audio"),
        (json!({"import_path": img}), "no audio"),
        (
            json!({"asset_id": id, "operations": [{"op":"fade_out","secs":9}]}),
            "secs",
        ),
        (
            json!({"asset_id": id, "operations": [{"op":"speed","factor":0}]}),
            "factor",
        ),
        (
            json!({"asset_id": id, "operations": [{"op":"trim","start_secs":9}]}),
            "past the end",
        ),
        (
            json!({"asset_id": id, "export": {"output_name": "../x"}}),
            "output_name",
        ),
        (json!({"asset_id": "nope"}), "unknown asset"),
    ] {
        let e = s.job_err("audio_edit", args.clone());
        assert!(e.contains(needle), "{args}\n -> {e}");
    }
    let dup = s.job_ok(
        "audio_edit",
        json!({"asset_id": id, "export": {"output_name": "a1"}}),
    );
    let e = s.job_err(
        "audio_edit",
        json!({"asset_id": dup["asset_id"], "export": {"output_name": "a1"}}),
    );
    assert!(e.contains("overwrite"), "{e}");
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}
