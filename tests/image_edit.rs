mod common;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use common::{Server, debris, pixel, rgb_pixels};
use serde_json::{Value, json};

fn path_of(v: &Value) -> std::path::PathBuf {
    v["path"].as_str().unwrap().into()
}

fn dims(v: &Value) -> (u64, u64) {
    (v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap())
}

const PNG: &[&str] = &["-f", "lavfi", "-i", "testsrc=s=200x100", "-frames:v", "1"];

#[test]
fn geometry_ops() {
    let Some(mut s) = Server::start() else { return };
    let img = s.fixture("a.png", PNG);
    let edit = |s: &mut Server, ops: Value| {
        s.job_ok("image_edit", json!({"import_path": img, "operations": ops}))
    };

    let r = edit(
        &mut s,
        json!([{"op":"crop","x":0,"y":0,"width":100,"height":50}, {"op":"resize","width":50}]),
    );
    assert_eq!(dims(&r), (50, 25));
    assert_eq!(r["kind"], "image");
    assert!(path_of(&r).starts_with(s.workspace().canonicalize().unwrap()));

    assert_eq!(
        dims(&edit(&mut s, json!([{"op":"rotate","degrees":90}]))),
        (100, 200)
    );
    assert_eq!(
        dims(&edit(
            &mut s,
            json!([{"op":"resize","width":60,"height":60,"mode":"fill"}])
        )),
        (60, 60)
    );
    assert_eq!(
        dims(&edit(
            &mut s,
            json!([{"op":"resize","width":60,"height":60,"mode":"exact"}])
        )),
        (60, 60)
    );
    // fit never enlarges unless asked
    assert_eq!(
        dims(&edit(&mut s, json!([{"op":"resize","width":400}]))),
        (200, 100)
    );
    assert_eq!(
        dims(&edit(
            &mut s,
            json!([{"op":"resize","width":400,"upscale":true}])
        )),
        (400, 200)
    );
    // JPEG source: goes through autorot, keeps its format by default.
    let jpg = s.fixture("a.jpg", PNG);
    let r = s.job_ok("image_edit", json!({"import_path": jpg, "operations": [{"op":"crop","x":10,"y":10,"width":80,"height":40}]}));
    assert_eq!(dims(&r), (80, 40));
    assert!(path_of(&r).extension().unwrap() == "jpg");
    // empty operations list just converts format
    let r = s.job_ok(
        "image_edit",
        json!({"import_path": img, "export": {"format": "jpeg"}}),
    );
    assert!(path_of(&r).extension().unwrap() == "jpg");
    assert_eq!(dims(&r), (200, 100));
}

#[test]
fn pixel_ops_text_overlay_color() {
    let Some(mut s) = Server::start() else { return };
    let black = s.fixture(
        "black.png",
        &[
            "-f",
            "lavfi",
            "-i",
            "color=black:s=200x100",
            "-frames:v",
            "1",
        ],
    );
    let red = s.fixture(
        "red.png",
        &["-f", "lavfi", "-i", "color=red:s=40x40", "-frames:v", "1"],
    );

    // Overlay a red square at (10,10): its centre is red, elsewhere stays black.
    let r = s.job_ok(
        "image_edit",
        json!({"import_path": black, "operations": [{"op":"overlay","import_path": red,"x":10,"y":10}], "export":{"format":"png"}}),
    );
    let p = path_of(&r);
    let [rr, g, b] = pixel(&p, 200, 30, 30);
    assert!(rr > 240 && g < 20 && b < 20, "overlay pixel {rr},{g},{b}");
    assert_eq!(pixel(&p, 200, 150, 80), [0, 0, 0]);

    // Text: white glyphs appear on a black canvas.
    let r = s.job_ok(
        "image_edit",
        json!({"asset_id": r["asset_id"], "operations": [{"op":"text","text":"Hi & <b>","x":60,"y":30,"size":40}], "export":{"format":"png"}}),
    );
    let px = rgb_pixels(&path_of(&r));
    assert!(
        px.chunks(3).any(|c| c.iter().all(|&v| v > 200)),
        "no white text pixels"
    );

    // Grayscale: every pixel has r == g == b.
    let colour = s.fixture("c.png", PNG);
    let r = s.job_ok(
        "image_edit",
        json!({"import_path": colour, "operations": [{"op":"color","grayscale":true}]}),
    );
    assert!(
        rgb_pixels(&path_of(&r))
            .chunks(3)
            .all(|c| c[0] == c[1] && c[1] == c[2])
    );

    // Brightness raises the mean of a mid-grey canvas.
    let grey = s.fixture(
        "grey.png",
        &["-f", "lavfi", "-i", "color=gray:s=20x20", "-frames:v", "1"],
    );
    let before = pixel(&grey, 20, 5, 5)[0] as i32;
    let r = s.job_ok("image_edit", json!({"import_path": grey, "operations": [{"op":"color","brightness":0.2}], "export":{"format":"png"}}));
    assert!(pixel(&path_of(&r), 20, 5, 5)[0] as i32 > before + 30);

    // Blur, flip and a 30-degree rotate with a JPEG export of an image with alpha all run.
    let r = s.job_ok(
        "image_edit",
        json!({"asset_id": r["asset_id"], "operations": [
            {"op":"blur","sigma":2.0},{"op":"flip","direction":"horizontal"},{"op":"rotate","degrees":30}
        ], "export": {"format":"jpeg","quality":70}}),
    );
    assert_eq!(r["kind"], "image");
    assert!(r["width"].as_u64().unwrap() > 20);
}

#[test]
fn naming_overwrite_and_failures_leave_no_debris() {
    let Some(mut s) = Server::start() else { return };
    let img = s.fixture("a.png", PNG);

    let a = s.job_ok(
        "image_edit",
        json!({"import_path": img, "export": {"output_name": "out1"}}),
    );
    assert_eq!(path_of(&a).file_name().unwrap(), "out1.png");

    let dup = s.job_err(
        "image_edit",
        json!({"asset_id": a["asset_id"], "export": {"output_name": "out1"}}),
    );
    assert!(dup.contains("overwrite"), "{dup}");
    s.job_ok(
        "image_edit",
        json!({"asset_id": a["asset_id"], "export": {"output_name": "out1", "overwrite": true}}),
    );

    // Failing steps surface vips's message in the job error and leave nothing behind.
    let bad = s.job_err(
        "image_edit",
        json!({"asset_id": a["asset_id"], "operations": [{"op":"crop","x":190,"y":90,"width":50,"height":50}]}),
    );
    assert!(bad.contains("bad extract area"), "{bad}");
    for bad_args in [
        json!({"asset_id": a["asset_id"], "operations": [{"op":"resize"}]}),
        json!({"asset_id": a["asset_id"], "export": {"output_name": "../x"}}),
        json!({"asset_id": a["asset_id"], "export": {"quality": 0}}),
        json!({"asset_id": "nope"}),
    ] {
        s.job_err("image_edit", bad_args);
    }
    let left = debris(&s.workspace());
    assert!(left.is_empty(), "debris: {left:?}");
}

#[test]
fn image_edit_rejects_video_and_preview_downscales() {
    let Some(mut s) = Server::start() else { return };
    let vid = s.fixture("v.mp4", &["-f", "lavfi", "-i", "testsrc=d=1:s=64x48:r=10"]);
    let e = s.job_err("image_edit", json!({"import_path": vid}));
    assert!(e.contains("only handles images"), "{e}");
    let aud = s.fixture("a.m4a", &["-f", "lavfi", "-i", "sine=d=1"]);
    assert_eq!(
        s.tool("preview", json!({"import_path": aud}))["isError"],
        true
    );

    let big = s.fixture(
        "big.png",
        &["-f", "lavfi", "-i", "testsrc=s=2000x1000", "-frames:v", "1"],
    );
    let r = s.tool("preview", json!({"import_path": big}));
    assert_ne!(r["isError"], true, "{r}");
    let img = r["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["type"] == "image")
        .expect("image content");
    assert_eq!(img["mimeType"], "image/jpeg");
    let jpg = s.dir.join("preview.jpg");
    std::fs::write(
        &jpg,
        STANDARD.decode(img["data"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let probe = s.ok("probe", json!({"import_path": jpg}));
    assert_eq!(dims(&probe), (1024, 512));

    // Small images are not enlarged.
    let small = s.fixture("small.png", PNG);
    let r = s.tool("preview", json!({"import_path": small}));
    let data = r["content"][1]["data"].as_str().unwrap();
    std::fs::write(&jpg, STANDARD.decode(data).unwrap()).unwrap();
    assert_eq!(
        dims(&s.ok("probe", json!({"import_path": jpg}))),
        (200, 100)
    );
}
