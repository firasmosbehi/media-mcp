//! libvips CLI wrapper. libvips has no way to chain operations in one process from the CLI
//! (no pipes for its native format), so an edit is one `vips` call per operation, linked by
//! intermediate `.v` files in a scratch directory that is deleted afterwards.
//! ponytail: intermediates are raw (w*h*bands bytes each); switch to a shared pipeline if
//! very large images become common. VIPS_BLOCK_UNTRUSTED cannot be set: it blocks `.v` loading.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use super::{Engines, stderr_tail};
use crate::{
    assets::AssetRef,
    error::{Error, Result},
    sandbox::Sandbox,
};

/// Largest width or height the server will produce.
/// ponytail: constant; make it an env setting if someone needs bigger.
pub const MAX_DIM: u32 = 16_384;
pub const MAX_OPS: usize = 32;
const VIPS_TIMEOUT: Duration = Duration::from_secs(60);
/// Stand-in for "no limit" when only one side of a resize box is given.
const UNBOUNDED: u32 = 10_000_000;

pub type Args = Vec<String>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResizeMode {
    /// Scale to fit inside the width x height box, keeping aspect ratio (default).
    #[default]
    Fit,
    /// Scale to cover the box, then crop the centre so the result is exactly width x height.
    /// Needs both width and height.
    Fill,
    /// Stretch to exactly width x height, ignoring aspect ratio. Needs both.
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlipDirection {
    /// Mirror left to right.
    Horizontal,
    /// Mirror top to bottom.
    Vertical,
}

/// One step of an image edit. Steps run in the order given, each on the previous result.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ImageOp {
    /// Cut out a rectangle. Coordinates are pixels from the top-left of the current image.
    Crop {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// Change the size. Give width, height or both.
    Resize {
        /// Target width in pixels.
        width: Option<u32>,
        /// Target height in pixels.
        height: Option<u32>,
        /// How the image is fitted to the box; default `fit`.
        #[serde(default)]
        mode: ResizeMode,
        /// Allow enlarging beyond the original size (fit/fill only); default false.
        #[serde(default)]
        upscale: bool,
    },
    /// Rotate clockwise by an angle in degrees. Multiples of 90 are lossless; other angles
    /// enlarge the canvas and fill the corners with black (transparent if the image has alpha).
    Rotate { degrees: f64 },
    /// Mirror the image.
    Flip { direction: FlipDirection },
    /// Colour adjustments. Give at least one field.
    Color {
        /// -1.0 (darker) to 1.0 (brighter); 0 leaves it unchanged.
        brightness: Option<f64>,
        /// 0.0 to 4.0; 1.0 leaves it unchanged, above 1 adds contrast.
        contrast: Option<f64>,
        /// Convert to black and white.
        #[serde(default)]
        grayscale: bool,
    },
    /// Gaussian blur.
    Blur {
        /// Blur radius (standard deviation) in pixels, greater than 0 and at most 100.
        sigma: f64,
    },
    /// Draw text onto the image.
    Text {
        /// The text to draw (plain text, not markup). Up to 1000 characters.
        text: String,
        /// Distance in pixels from the left edge to the text's left edge.
        x: i32,
        /// Distance in pixels from the top edge to the text's top edge.
        y: i32,
        /// Font size in pixels, default 32.
        size: Option<u32>,
        /// Colour as `#RRGGBB`, default white.
        color: Option<String>,
        /// Font family, default `sans`. Must be installed on the machine running the server.
        font: Option<String>,
    },
    /// Place another image on top; its transparency is respected.
    Overlay {
        #[serde(flatten)]
        source: AssetRef,
        /// Left edge of the overlay in pixels (may be negative).
        x: i32,
        /// Top edge of the overlay in pixels (may be negative).
        y: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
            ImageFormat::Webp => "webp",
        }
    }

    /// Same format as the source file where we can write it, otherwise PNG.
    pub fn from_path(p: &Path) -> Self {
        match ext_lower(p).as_deref() {
            Some("jpg" | "jpeg") => ImageFormat::Jpeg,
            Some("webp") => ImageFormat::Webp,
            _ => ImageFormat::Png,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Export {
    pub format: ImageFormat,
    pub quality: u8,
    pub strip_metadata: bool,
}

/// Inputs for compiling one operation. Every file the compiled commands touch is either
/// `input`, `output`, `overlay`, or a bare scratch file name relative to the working directory
/// the commands run in.
pub struct StepCtx<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub index: usize,
    /// Band count of `input`; only filled in for operations where `needs_bands` is true.
    pub bands: u32,
    pub overlay: Option<&'a Path>,
}

impl StepCtx<'_> {
    fn tmp(&self, name: &str) -> String {
        format!("s{}_{name}.v", self.index)
    }
}

impl ImageOp {
    pub fn needs_bands(&self) -> bool {
        matches!(self, ImageOp::Rotate { .. } | ImageOp::Color { .. })
    }
}

macro_rules! args {
    ($($x:expr),* $(,)?) => { vec![$(($x).to_string()),*] };
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn bad(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

fn check_dim(name: &str, v: u32) -> Result<()> {
    if v == 0 || v > MAX_DIM {
        return Err(bad(format!("{name} must be between 1 and {MAX_DIM}")));
    }
    Ok(())
}

fn zeros(bands: u32) -> String {
    vec!["0"; bands.max(1) as usize].join(" ")
}

/// Compile one operation into the `vips` argument lists that perform it.
pub fn compile_op(op: &ImageOp, c: &StepCtx) -> Result<Vec<Args>> {
    let (i, o) = (s(c.input), s(c.output));
    Ok(match op {
        ImageOp::Crop {
            x,
            y,
            width,
            height,
        } => {
            check_dim("width", *width)?;
            check_dim("height", *height)?;
            vec![args!["crop", i, o, x, y, width, height]]
        }
        ImageOp::Resize {
            width,
            height,
            mode,
            upscale,
        } => {
            if width.is_none() && height.is_none() {
                return Err(bad("resize needs width, height or both"));
            }
            if *mode != ResizeMode::Fit && (width.is_none() || height.is_none()) {
                return Err(bad(
                    "resize modes fill and exact need both width and height",
                ));
            }
            for (n, v) in [("width", width), ("height", height)] {
                if let Some(v) = v {
                    check_dim(n, *v)?;
                }
            }
            let size = match (mode, upscale) {
                (ResizeMode::Exact, _) => "force",
                (_, true) => "both",
                (_, false) => "down",
            };
            let mut a = args![
                "thumbnail",
                i,
                o,
                width.unwrap_or(UNBOUNDED),
                "--height",
                height.unwrap_or(UNBOUNDED),
                "--size",
                size
            ];
            if *mode == ResizeMode::Fill {
                a.extend(args!["--crop", "centre"]);
            }
            vec![a]
        }
        ImageOp::Rotate { degrees } => {
            if !degrees.is_finite() {
                return Err(bad("degrees must be a finite number"));
            }
            let d = degrees.rem_euclid(360.0);
            vec![if d == 0.0 {
                args!["copy", i, o]
            } else if [90.0, 180.0, 270.0].contains(&d) {
                args!["rot", i, o, format!("d{d}")]
            } else {
                args!["rotate", i, o, d, "--background", zeros(c.bands)]
            }]
        }
        ImageOp::Flip { direction } => {
            let dir = match direction {
                FlipDirection::Horizontal => "horizontal",
                FlipDirection::Vertical => "vertical",
            };
            vec![args!["flip", i, o, dir]]
        }
        ImageOp::Color {
            brightness,
            contrast,
            grayscale,
        } => {
            if brightness.is_none() && contrast.is_none() && !grayscale {
                return Err(bad("color needs brightness, contrast or grayscale"));
            }
            let (br, ct) = (brightness.unwrap_or(0.0), contrast.unwrap_or(1.0));
            if !(-1.0..=1.0).contains(&br) || !(0.0..=4.0).contains(&ct) {
                return Err(bad("brightness must be in -1..1 and contrast in 0..4"));
            }
            let mut steps = vec![];
            let mut cur = i;
            if brightness.is_some() || contrast.is_some() {
                // Pivot contrast around mid-grey; leave an alpha band (2 or 4 bands) untouched.
                let colour_bands = if c.bands == 2 || c.bands == 4 {
                    c.bands - 1
                } else {
                    c.bands.max(1)
                };
                let alpha = c.bands == 2 || c.bands == 4;
                let vec_of = |v: f64, alpha_v: &str| {
                    let mut parts = vec![v.to_string(); colour_bands as usize];
                    if alpha {
                        parts.push(alpha_v.into());
                    }
                    parts.join(" ")
                };
                let offset = 128.0 * (1.0 - ct) + br * 255.0;
                let target = if *grayscale { c.tmp("lin") } else { o.clone() };
                steps.push(args![
                    "linear",
                    cur,
                    target.clone(),
                    vec_of(ct, "1"),
                    vec_of(offset, "0"),
                    "--uchar"
                ]);
                cur = target;
            }
            if *grayscale {
                steps.push(args!["colourspace", cur, o, "b-w"]);
            }
            steps
        }
        ImageOp::Blur { sigma } => {
            if !(*sigma > 0.0 && *sigma <= 100.0) {
                return Err(bad("sigma must be greater than 0 and at most 100"));
            }
            vec![args!["gaussblur", i, o, sigma]]
        }
        ImageOp::Text {
            text,
            x,
            y,
            size,
            color,
            font,
        } => {
            let size = size.unwrap_or(32);
            if text.is_empty() || text.chars().count() > 1000 {
                return Err(bad("text must be 1 to 1000 characters"));
            }
            if !(1..=1000).contains(&size) {
                return Err(bad("size must be between 1 and 1000"));
            }
            let rgb = parse_color(color.as_deref().unwrap_or("#ffffff"))?;
            let family = font.as_deref().unwrap_or("sans").trim();
            if family.is_empty() || family.chars().any(char::is_control) {
                return Err(bad("invalid font family"));
            }
            let (mask, col, joined, rgba) =
                (c.tmp("mask"), c.tmp("col"), c.tmp("joined"), c.tmp("rgba"));
            vec![
                args![
                    "text",
                    mask.clone(),
                    pango_markup(text),
                    "--font",
                    format!("{family} {size}"),
                    "--dpi",
                    72
                ],
                // A constant-colour image the size of the mask...
                args!["linear", mask.clone(), col.clone(), "0 0 0", rgb, "--uchar"],
                // ...with the mask as its alpha channel.
                args!["bandjoin", format!("{col} {mask}"), joined.clone()],
                args!["copy", joined, rgba.clone(), "--interpretation", "srgb"],
                args!["composite2", i, rgba, o, "over", "--x", x, "--y", y],
            ]
        }
        ImageOp::Overlay { x, y, .. } => {
            let ov = c
                .overlay
                .ok_or_else(|| bad("overlay source was not resolved"))?;
            vec![args!["composite2", i, s(ov), o, "over", "--x", x, "--y", y]]
        }
    })
}

/// Commands that turn the final intermediate into the exported file.
pub fn export_steps(input: &Path, output: &Path, bands: u32, e: &Export) -> Vec<Args> {
    let mut steps = vec![];
    let mut cur = s(input);
    // JPEG cannot hold alpha: flatten onto white first.
    if e.format == ImageFormat::Jpeg && (bands == 2 || bands == 4) {
        let flat = "export_flat.v".to_string();
        steps.push(args![
            "flatten",
            cur,
            flat.clone(),
            "--background",
            vec!["255"; (bands - 1) as usize].join(" ")
        ]);
        cur = flat;
    }
    let mut opts = vec![];
    if e.format != ImageFormat::Png {
        opts.push(format!("Q={}", e.quality));
    }
    if e.strip_metadata {
        // ponytail: strip also drops embedded ICC profiles; no per-profile handling in vips 8.14.
        opts.push("strip".to_string());
    }
    let target = if opts.is_empty() {
        s(output)
    } else {
        format!("{}[{}]", s(output), opts.join(","))
    };
    steps.push(args!["copy", cur, target]);
    steps
}

/// `vips text` reads Pango markup. Wrapping in a span also stops text starting with `-`
/// from being parsed as a command-line option.
pub fn pango_markup(text: &str) -> String {
    let esc = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<span>{esc}</span>")
}

/// `#RRGGBB` (or `RRGGBB`) to the `"R G B"` form vips takes.
pub fn parse_color(c: &str) -> Result<String> {
    let h = c.strip_prefix('#').unwrap_or(c);
    if h.len() != 6 || !h.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(bad(format!("color must be #RRGGBB, got {c}")));
    }
    let n = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    Ok(format!("{} {} {}", n(0), n(2), n(4)))
}

fn ext_lower(p: &Path) -> Option<String> {
    p.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// vips treats `[...]` in a file name as options, so such paths cannot be passed safely.
fn check_vips_path(p: &Path) -> Result<()> {
    if p.to_string_lossy().contains(['[', ']']) {
        return Err(bad(format!(
            "path contains '[' or ']', which libvips cannot handle: {}",
            p.display()
        )));
    }
    Ok(())
}

pub struct EditJob<'a> {
    pub input: &'a Path,
    /// Resolved sources for the `Overlay` ops, in the order those ops appear.
    pub overlays: Vec<PathBuf>,
    pub ops: &'a [ImageOp],
    pub export: Export,
    /// Where the exported file is written; its extension selects the format.
    pub output: &'a Path,
}

/// Run an edit: one vips call per step, scratch files in a temp dir that is always removed.
pub async fn edit(engines: &Engines, sandbox: &Sandbox, job: EditJob<'_>) -> Result<()> {
    if job.ops.len() > MAX_OPS {
        return Err(bad(format!("at most {MAX_OPS} operations per call")));
    }
    let work = sandbox.make_temp_dir().await?;
    let result = edit_in(engines, &work, &job).await;
    let _ = tokio::fs::remove_dir_all(&work).await;
    result
}

async fn edit_in(engines: &Engines, work: &Path, job: &EditJob<'_>) -> Result<()> {
    for p in [job.input, job.output, work]
        .into_iter()
        .chain(job.overlays.iter().map(PathBuf::as_path))
    {
        check_vips_path(p)?;
    }
    let mut cur = job.input.to_path_buf();
    // Apply EXIF orientation up front so crops and text land where the viewer sees them.
    if matches!(
        ext_lower(job.input).as_deref(),
        Some("jpg" | "jpeg" | "tif" | "tiff" | "heic" | "heif")
    ) {
        let next = work.join("auto.v");
        run_vips(engines, work, args!["autorot", s(&cur), s(&next)]).await?;
        cur = next;
    }
    let mut overlays = job.overlays.iter();
    for (index, op) in job.ops.iter().enumerate() {
        let next = work.join(format!("s{index}.v"));
        let bands = if op.needs_bands() {
            header_bands(engines, &cur).await?
        } else {
            0
        };
        let ctx = StepCtx {
            input: &cur,
            output: &next,
            index,
            bands,
            overlay: if matches!(op, ImageOp::Overlay { .. }) {
                overlays.next().map(PathBuf::as_path)
            } else {
                None
            },
        };
        for step in compile_op(op, &ctx)? {
            run_vips(engines, work, step).await?;
        }
        cur = next;
    }
    let bands = if job.export.format == ImageFormat::Jpeg {
        header_bands(engines, &cur).await?
    } else {
        0
    };
    for step in export_steps(&cur, job.output, bands, &job.export) {
        run_vips(engines, work, step).await?;
    }
    Ok(())
}

async fn run_vips(engines: &Engines, cwd: &Path, args: Args) -> Result<()> {
    run(&engines.vips, "vips", cwd, args).await.map(|_| ())
}

async fn header_bands(engines: &Engines, file: &Path) -> Result<u32> {
    // `vipsheader` ships in the same directory as `vips`.
    let bin = engines.vips.with_file_name("vipsheader");
    let out = run(
        &bin,
        "vipsheader",
        Path::new("."),
        args!["-f", "bands", s(file)],
    )
    .await?;
    out.trim().parse().map_err(|_| Error::Parse {
        program: "vipsheader",
        detail: format!("expected a band count, got {out:?}"),
    })
}

async fn run(bin: &Path, program: &'static str, cwd: &Path, args: Args) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(VIPS_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Engine {
            program,
            status: "timed out".into(),
            stderr_tail: String::new(),
        })?
        .map_err(|e| Error::io(bin, e))?;
    if !out.status.success() {
        return Err(Error::Engine {
            program,
            status: out.status.to_string(),
            stderr_tail: stderr_tail(&out.stderr),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(bands: u32, overlay: Option<&'a Path>) -> StepCtx<'a> {
        StepCtx {
            input: Path::new("/w/in.v"),
            output: Path::new("/w/out.v"),
            index: 3,
            bands,
            overlay,
        }
    }

    fn one(op: ImageOp, bands: u32) -> Vec<Args> {
        compile_op(&op, &ctx(bands, None)).unwrap()
    }

    fn err(op: ImageOp) -> bool {
        compile_op(&op, &ctx(3, None)).is_err()
    }

    #[test]
    fn crop_args() {
        let a = one(
            ImageOp::Crop {
                x: 1,
                y: 2,
                width: 30,
                height: 40,
            },
            3,
        );
        assert_eq!(a, vec![args!["crop", "/w/in.v", "/w/out.v", 1, 2, 30, 40]]);
        assert!(err(ImageOp::Crop {
            x: 0,
            y: 0,
            width: 0,
            height: 5
        }));
    }

    fn resize(w: Option<u32>, h: Option<u32>, mode: ResizeMode, up: bool) -> ImageOp {
        ImageOp::Resize {
            width: w,
            height: h,
            mode,
            upscale: up,
        }
    }

    #[test]
    fn resize_modes() {
        let fit = one(resize(Some(100), None, ResizeMode::Fit, false), 3);
        assert_eq!(
            fit,
            vec![args![
                "thumbnail",
                "/w/in.v",
                "/w/out.v",
                100,
                "--height",
                10_000_000,
                "--size",
                "down"
            ]]
        );
        let fill = one(resize(Some(10), Some(20), ResizeMode::Fill, true), 3);
        assert_eq!(
            fill[0][3..],
            args!["10", "--height", "20", "--size", "both", "--crop", "centre"]
        );
        let exact = one(resize(Some(10), Some(20), ResizeMode::Exact, false), 3);
        assert_eq!(exact[0][7], "force");
    }

    #[test]
    fn resize_validation() {
        assert!(err(resize(None, None, ResizeMode::Fit, false)));
        assert!(err(resize(Some(10), None, ResizeMode::Fill, false)));
        assert!(err(resize(Some(0), None, ResizeMode::Fit, false)));
        assert!(err(resize(Some(MAX_DIM + 1), None, ResizeMode::Fit, false)));
    }

    #[test]
    fn rotate_variants() {
        let rot = |d| one(ImageOp::Rotate { degrees: d }, 4);
        assert_eq!(rot(90.0), vec![args!["rot", "/w/in.v", "/w/out.v", "d90"]]);
        assert_eq!(rot(-90.0)[0][3], "d270");
        assert_eq!(rot(360.0), vec![args!["copy", "/w/in.v", "/w/out.v"]]);
        assert_eq!(
            rot(30.0),
            vec![args![
                "rotate",
                "/w/in.v",
                "/w/out.v",
                30,
                "--background",
                "0 0 0 0"
            ]]
        );
        assert!(err(ImageOp::Rotate { degrees: f64::NAN }));
    }

    #[test]
    fn color_brightness_contrast_keeps_alpha() {
        let a = one(
            ImageOp::Color {
                brightness: Some(0.1),
                contrast: Some(1.0),
                grayscale: false,
            },
            4,
        );
        assert_eq!(
            a,
            vec![args![
                "linear",
                "/w/in.v",
                "/w/out.v",
                "1 1 1 1",
                "25.5 25.5 25.5 0",
                "--uchar"
            ]]
        );
    }

    #[test]
    fn color_grayscale_chains_through_scratch_file() {
        let a = one(
            ImageOp::Color {
                brightness: None,
                contrast: Some(2.0),
                grayscale: true,
            },
            3,
        );
        assert_eq!(a.len(), 2);
        assert_eq!(a[0][2], "s3_lin.v");
        assert_eq!(a[0][3], "2 2 2");
        assert_eq!(a[0][4], "-128 -128 -128");
        assert_eq!(a[1], args!["colourspace", "s3_lin.v", "/w/out.v", "b-w"]);
        assert!(err(ImageOp::Color {
            brightness: None,
            contrast: None,
            grayscale: false
        }));
        assert!(err(ImageOp::Color {
            brightness: Some(2.0),
            contrast: None,
            grayscale: false
        }));
    }

    #[test]
    fn text_pipeline() {
        let a = one(
            ImageOp::Text {
                text: "-a<b>&c".into(),
                x: 5,
                y: -6,
                size: Some(20),
                color: Some("#ff8000".into()),
                font: None,
            },
            3,
        );
        assert_eq!(a.len(), 5);
        assert_eq!(
            a[0],
            args![
                "text",
                "s3_mask.v",
                "<span>-a&lt;b&gt;&amp;c</span>",
                "--font",
                "sans 20",
                "--dpi",
                72
            ]
        );
        assert_eq!(a[1][3..5], args!["0 0 0", "255 128 0"]);
        assert_eq!(a[2], args!["bandjoin", "s3_col.v s3_mask.v", "s3_joined.v"]);
        assert_eq!(
            a[4],
            args![
                "composite2",
                "/w/in.v",
                "s3_rgba.v",
                "/w/out.v",
                "over",
                "--x",
                5,
                "--y",
                -6
            ]
        );
        for bad_text in ["", &"x".repeat(1001)] {
            assert!(err(ImageOp::Text {
                text: bad_text.into(),
                x: 0,
                y: 0,
                size: None,
                color: None,
                font: None
            }));
        }
    }

    #[test]
    fn overlay_needs_resolved_source() {
        let op = ImageOp::Overlay {
            source: AssetRef::default(),
            x: 1,
            y: 2,
        };
        assert!(compile_op(&op, &ctx(3, None)).is_err());
        let ov = Path::new("/w/ov.png");
        let a = compile_op(&op, &ctx(3, Some(ov))).unwrap();
        assert_eq!(
            a,
            vec![args![
                "composite2",
                "/w/in.v",
                "/w/ov.png",
                "/w/out.v",
                "over",
                "--x",
                1,
                "--y",
                2
            ]]
        );
    }

    #[test]
    fn blur_and_flip() {
        assert_eq!(
            one(ImageOp::Blur { sigma: 2.5 }, 3),
            vec![args!["gaussblur", "/w/in.v", "/w/out.v", 2.5]]
        );
        assert!(err(ImageOp::Blur { sigma: 0.0 }));
        assert!(err(ImageOp::Blur { sigma: 101.0 }));
        assert_eq!(
            one(
                ImageOp::Flip {
                    direction: FlipDirection::Vertical
                },
                3
            )[0][3],
            "vertical"
        );
    }

    #[test]
    fn export_jpeg_flattens_alpha() {
        let e = Export {
            format: ImageFormat::Jpeg,
            quality: 85,
            strip_metadata: true,
        };
        let a = export_steps(Path::new("/w/in.v"), Path::new("/w/o.jpg"), 4, &e);
        assert_eq!(
            a,
            vec![
                args![
                    "flatten",
                    "/w/in.v",
                    "export_flat.v",
                    "--background",
                    "255 255 255"
                ],
                args!["copy", "export_flat.v", "/w/o.jpg[Q=85,strip]"],
            ]
        );
        let a = export_steps(Path::new("/w/in.v"), Path::new("/w/o.jpg"), 3, &e);
        assert_eq!(a, vec![args!["copy", "/w/in.v", "/w/o.jpg[Q=85,strip]"]]);
    }

    #[test]
    fn export_png_has_no_quality() {
        let e = Export {
            format: ImageFormat::Png,
            quality: 90,
            strip_metadata: false,
        };
        let a = export_steps(Path::new("/w/in.v"), Path::new("/w/o.png"), 4, &e);
        assert_eq!(a, vec![args!["copy", "/w/in.v", "/w/o.png"]]);
    }

    #[test]
    fn color_parsing_and_escaping() {
        assert_eq!(parse_color("#0a0B0c").unwrap(), "10 11 12");
        assert_eq!(parse_color("ffffff").unwrap(), "255 255 255");
        assert!(parse_color("red").is_err());
        assert!(parse_color("#12345").is_err());
        assert_eq!(pango_markup("a&b"), "<span>a&amp;b</span>");
    }

    #[test]
    fn brackets_in_paths_are_rejected() {
        assert!(check_vips_path(Path::new("/a/b.png")).is_ok());
        assert!(check_vips_path(Path::new("/a[1]/b.png")).is_err());
    }

    #[test]
    fn format_follows_source_extension() {
        assert_eq!(
            ImageFormat::from_path(Path::new("a.JPEG")),
            ImageFormat::Jpeg
        );
        assert_eq!(
            ImageFormat::from_path(Path::new("a.webp")),
            ImageFormat::Webp
        );
        assert_eq!(ImageFormat::from_path(Path::new("a.bmp")), ImageFormat::Png);
    }
}
