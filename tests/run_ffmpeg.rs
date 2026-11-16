mod common;
use common::{Server, debris, mean_volume_db, rgb_pixels};
use serde_json::{Value, json};

fn path_of(v: &Value) -> std::path::PathBuf {
    v["path"].as_str().unwrap().into()
}

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
fn runs_user_arguments_through_the_sandbox() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);

    // Greyscale re-encode: every pixel ends up grey.
    let g = s.job_ok(
        "run_ffmpeg",
        json!({"inputs": [{"import_path": v}], "args": ["-i","{in0}","-vf","hue=s=0","-an","{out}"],
               "output_format": "mp4", "output_name": "grey"}),
    );
    assert_eq!(path_of(&g).file_name().unwrap(), "grey.mp4");
    assert!(path_of(&g).starts_with(s.workspace().canonicalize().unwrap()));
    assert_eq!(g["has_audio"], false);
    let bad = rgb_pixels(&path_of(&g))
        .chunks(3)
        .filter(|c| (c[0] as i32 - c[1] as i32).abs() > 6 || (c[1] as i32 - c[2] as i32).abs() > 6)
        .count();
    assert_eq!(bad, 0);

    // The caller's own -t is honoured (the server only adds its cap when none is given).
    let short = s.job_ok(
        "run_ffmpeg",
        json!({"inputs": [{"asset_id": g["asset_id"]}], "args": ["-i","{in0}","-t","0.5","-c","copy","{out}"],
               "output_format": "mkv"}),
    );
    let d = short["duration_secs"].as_f64().unwrap();
    assert!((d - 0.5).abs() < 0.3, "{d}");

    // Animated GIF: something the typed tools cannot make.
    let gif = s.job_ok(
        "run_ffmpeg",
        json!({"inputs": [{"asset_id": g["asset_id"]}],
               "args": ["-i","{in0}","-vf","fps=5,scale=80:-1","{out}"], "output_format": "gif"}),
    );
    assert_eq!(gif["video_codec"], "gif");
    assert_eq!(gif["width"], 80);

    // Two inputs: mix audio from two assets.
    let a = s.job_ok("audio_edit", json!({"import_path": v}));
    let mix = s.job_ok(
        "run_ffmpeg",
        json!({"inputs": [{"asset_id": a["asset_id"]}, {"asset_id": a["asset_id"]}],
               "args": ["-i","{in0}","-i","{in1}","-filter_complex","amix=inputs=2","{out}"],
               "output_format": "wav"}),
    );
    assert_eq!(mix["kind"], "audio");
    assert!(mean_volume_db(&path_of(&mix)) < 0.0);
}

#[test]
fn unsafe_or_wrong_arguments_are_tool_errors_without_debris() {
    let Some(mut s) = Server::start() else { return };
    let v = s.fixture("v.mp4", AV);
    let id = s.ok("probe", json!({"import_path": v}))["asset_id"].clone();
    let run =
        |args: Value| json!({"inputs": [{"asset_id": id}], "args": args, "output_format": "mp4"});
    for (args, needle) in [
        (
            json!(["-i", "http://example.com/a.mp4", "{out}"]),
            "placeholder",
        ),
        (
            json!(["-i", "{in0}", "-metadata", "comment=http://x/y", "{out}"]),
            "not allowed",
        ),
        (json!(["-i", "{in0}", "-y", "{out}"]), "-y"),
        (
            json!(["-i", "{in0}", "-vf", "movie=/etc/passwd", "{out}"]),
            "movie",
        ),
        (
            json!([
                "-i",
                "{in0}",
                "-vf",
                "drawtext=textfile=/etc/passwd",
                "{out}"
            ]),
            "textfile",
        ),
        (json!(["-i", "{in0}", "-vf", "hue=s=0"]), "last argument"),
        (
            json!(["-i", "{in0}", "/tmp/escape.mp4", "{out}"]),
            "absolute paths",
        ),
        (
            json!(["-i", "{in0}", "../escape.mp4", "{out}"]),
            "absolute paths",
        ),
        (
            json!(["-i", "{in0}", "-vf", "nosuchfilter", "{out}"]),
            "ffmpeg failed",
        ),
        (json!(["-vf", "hue=s=0", "{out}"]), "never used"),
        (
            json!(["-f", "lavfi", "-i", "testsrc", "{out}"]),
            "placeholder",
        ),
    ] {
        let e = s.job_err("run_ffmpeg", run(args.clone()));
        assert!(e.contains(needle), "{args}\n -> {e}");
    }
    assert!(
        s.job_err(
            "run_ffmpeg",
            json!({"inputs": [], "args": ["{out}"], "output_format": "mp4"})
        )
        .contains("at least one input")
    );
    assert!(s.job_err("run_ffmpeg", json!({"inputs": [{"asset_id": "nope"}], "args": ["-i","{in0}","{out}"], "output_format": "mp4"})).contains("unknown asset"));
    assert!(!std::path::Path::new("/tmp/escape.mp4").exists());
    // A relative extra output goes to the scratch dir and is deleted with it.
    s.job_ok(
        "run_ffmpeg",
        run(json!([
            "-i",
            "{in0}",
            "-frames:v",
            "1",
            "extra.png",
            "-frames:v",
            "1",
            "{out}"
        ])),
    );
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}
