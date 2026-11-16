mod common;
use common::{Server, debris, pixel, rgb_pixels};
use serde_json::{Value, json};

fn path_of(v: &Value) -> std::path::PathBuf {
    v["path"].as_str().unwrap().into()
}

fn dims(v: &Value) -> (u64, u64) {
    (v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap())
}

fn near(p: [u8; 3], want: [u8; 3]) -> bool {
    p.iter()
        .zip(want)
        .all(|(a, b)| (*a as i32 - b as i32).abs() <= 8)
}

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
fn generate_image_solid_and_gradients() {
    let Some(mut s) = Server::start() else { return };
    let r = s.job_ok(
        "generate_image",
        json!({"width": 40, "height": 30, "fill": {"type":"solid","color":"#ff8000"}}),
    );
    assert_eq!((r["kind"].as_str(), dims(&r)), (Some("image"), (40, 30)));
    assert!(path_of(&r).starts_with(s.workspace().canonicalize().unwrap()));
    assert!(near(pixel(&path_of(&r), 40, 20, 15), [255, 128, 0]));

    let h = s.job_ok(
        "generate_image",
        json!({"width": 256, "height": 8, "fill": {"type":"gradient","from":"#000000","to":"#ff0000"}}),
    );
    let p = path_of(&h);
    assert!(near(pixel(&p, 256, 0, 4), [0, 0, 0]));
    assert!(near(pixel(&p, 256, 128, 4), [128, 0, 0]));
    assert!(near(pixel(&p, 256, 255, 4), [255, 0, 0]));
    // same colour along a column
    assert_eq!(pixel(&p, 256, 100, 0), pixel(&p, 256, 100, 7));

    let v = s.job_ok(
        "generate_image",
        json!({"width": 8, "height": 256, "fill": {"type":"gradient","from":"#0000ff","to":"#ffffff","direction":"vertical"},
               "export": {"format": "jpeg", "output_name": "grad"}}),
    );
    assert_eq!(path_of(&v).file_name().unwrap(), "grad.jpg");
    let p = path_of(&v);
    assert!(near(pixel(&p, 8, 4, 0), [0, 0, 255]) || pixel(&p, 8, 4, 0)[2] > 240);
    let bottom = pixel(&p, 8, 4, 255);
    assert!(bottom.iter().all(|&c| c > 235), "{bottom:?}");
    assert_eq!(dims(&v), (8, 256));
}

#[test]
fn image_compose_builds_a_grid() {
    let Some(mut s) = Server::start() else { return };
    let red = solid(&s, "red.png", "red", "100x100");
    let blue = solid(&s, "blue.png", "blue", "50x100"); // fits its cell with side margins
    let green = solid(&s, "green.png", "green", "100x100");
    let r = s.job_ok(
        "image_compose",
        json!({"images": [{"import_path": red}, {"import_path": blue}, {"import_path": green}],
               "columns": 2, "cell_width": 60, "cell_height": 60, "gap": 10, "background": "#ffffff"}),
    );
    // 2 columns x 2 rows of 60px cells with a 10px gap
    assert_eq!(dims(&r), (130, 130));
    let px = rgb_pixels(&path_of(&r));
    let at = |x: usize, y: usize| {
        let i = (y * 130 + x) * 3;
        [px[i], px[i + 1], px[i + 2]]
    };
    assert!(near(at(30, 30), [255, 0, 0]), "cell 0 {:?}", at(30, 30));
    assert!(near(at(100, 30), [0, 0, 255]), "cell 1 {:?}", at(100, 30));
    assert!(near(at(65, 30), [255, 255, 255]), "gap {:?}", at(65, 30));
    // the 30px-wide blue image is centred in its 60px cell: margins stay background
    assert!(near(at(75, 30), [255, 255, 255]), "margin {:?}", at(75, 30));
    assert!(
        at(30, 100)[1] > 100 && at(30, 100)[0] < 50,
        "cell 2 {:?}",
        at(30, 100)
    );
    assert!(
        near(at(100, 100), [255, 255, 255]),
        "empty cell {:?}",
        at(100, 100)
    );

    // defaults: 3 images -> 2 columns, 400px cells
    let d = s.job_ok(
        "image_compose",
        json!({"images": [{"asset_id": r["asset_id"]}], "export": {"format": "jpeg"}}),
    );
    assert_eq!(dims(&d), (400, 400));
}

#[test]
fn generate_video_is_a_silent_coloured_clip() {
    let Some(mut s) = Server::start() else { return };
    let r = s.job_ok(
        "generate_video",
        json!({"width": 81, "height": 60, "secs": 1.5, "fps": 10,
               "fill": {"type":"solid","color":"#00ff00"}, "export": {"output_name": "card"}}),
    );
    assert_eq!(r["kind"], "video");
    assert_eq!(dims(&r), (80, 60));
    assert!(
        (r["duration_secs"].as_f64().unwrap() - 1.5).abs() < 0.25,
        "{r}"
    );
    assert_eq!(r["has_audio"], false);
    assert_eq!(path_of(&r).file_name().unwrap(), "card.mp4");
    let px = rgb_pixels(&path_of(&r));
    let c = &px[(30 * 80 + 40) * 3..][..3];
    assert!(c[1] > 200 && c[0] < 60 && c[2] < 60, "{c:?}");
    // it composes with other clips
    let j = s.job_ok(
        "video_compose",
        json!({"clips": [{"asset_id": r["asset_id"]}, {"asset_id": r["asset_id"]}], "fps": 10}),
    );
    assert!((j["duration_secs"].as_f64().unwrap() - 3.0).abs() < 0.4);
}

#[test]
fn bad_input_is_a_tool_error_without_debris() {
    let Some(mut s) = Server::start() else { return };
    let img = solid(&s, "i.png", "red", "64x48");
    let aud = s.fixture("a.m4a", &["-f", "lavfi", "-i", "sine=d=1"]);
    let ok = json!({"type":"solid","color":"#000000"});
    for (tool, args, needle) in [
        (
            "generate_image",
            json!({"width":0,"height":5,"fill":ok}),
            "width",
        ),
        (
            "generate_image",
            json!({"width":99999,"height":5,"fill":ok}),
            "width",
        ),
        (
            "generate_image",
            json!({"width":5,"height":5,"fill":{"type":"solid","color":"red"}}),
            "#RRGGBB",
        ),
        (
            "generate_image",
            json!({"width":5,"height":5,"fill":ok,"export":{"output_name":"../x"}}),
            "output_name",
        ),
        (
            "generate_video",
            json!({"width":5000,"height":50,"secs":1,"fill":ok}),
            "at most",
        ),
        (
            "generate_video",
            json!({"width":50,"height":50,"secs":0.01,"fill":ok}),
            "secs",
        ),
        (
            "generate_video",
            json!({"width":50,"height":50,"secs":5000,"fill":ok}),
            "limit",
        ),
        (
            "generate_video",
            json!({"width":50,"height":50,"secs":1,"fill":ok,"export":{"stream_copy":true}}),
            "stream_copy",
        ),
        ("image_compose", json!({"images": []}), "between 1 and 64"),
        (
            "image_compose",
            json!({"images": [{"import_path": aud}]}),
            "only joins images",
        ),
        (
            "image_compose",
            json!({"images": [{"import_path": img}], "columns": 0}),
            "columns",
        ),
        (
            "image_compose",
            json!({"images": [{"import_path": img}], "gap": 5000}),
            "gap",
        ),
        (
            "image_compose",
            json!({"images": [{"import_path": img}, {"import_path": img}], "cell_width": 9000, "cell_height": 9000, "columns": 2}),
            "limit",
        ),
        (
            "image_compose",
            json!({"images": [{"import_path": img}], "background": "nope"}),
            "#RRGGBB",
        ),
        (
            "image_compose",
            json!({"images": [{"asset_id": "nope"}]}),
            "unknown asset",
        ),
    ] {
        let e = s.job_err(tool, args.clone());
        assert!(e.contains(needle), "{tool} {args}\n -> {e}");
    }
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}
