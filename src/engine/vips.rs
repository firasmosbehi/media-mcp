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

use super::{
    Engines,
    generate::{Fill, GradientDirection},
    stderr_tail,
};
use crate::{
    assets::AssetRef,
    error::{Error, Result},
    jobs::Progress,
    sandbox::{Sandbox, Scratch},
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
    /// Updated after each operation; use `Progress::default()` if nobody is watching.
    pub progress: Progress,
}

/// Run an edit: one vips call per step, scratch files in a temp dir that is always removed.
pub async fn edit(engines: &Engines, sandbox: &Sandbox, job: EditJob<'_>) -> Result<()> {
    if job.ops.len() > MAX_OPS {
        return Err(bad(format!("at most {MAX_OPS} operations per call")));
    }
    let work = sandbox.make_temp_dir().await?;
    let _cleanup = Scratch::new(&work); // also runs if the job is cancelled mid-way
    edit_in(engines, &work, &job).await
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
        job.progress
            .set((index + 1) as f32 / (job.ops.len() + 1) as f32);
    }
    finish(engines, work, &cur, job.output, &job.export).await
}

/// Export the final intermediate to `output`.
async fn finish(
    engines: &Engines,
    work: &Path,
    cur: &Path,
    output: &Path,
    export: &Export,
) -> Result<()> {
    let bands = if export.format == ImageFormat::Jpeg {
        header_bands(engines, cur).await?
    } else {
        0
    };
    for step in export_steps(cur, output, bands, export) {
        run_vips(engines, work, step).await?;
    }
    Ok(())
}

/// A grid of images: each fitted inside a `cell` box, centred, on a background.
pub struct ComposeJob<'a> {
    pub inputs: &'a [PathBuf],
    pub columns: u32,
    pub cell: (u32, u32),
    /// Pixels between cells (and nothing around the outside).
    pub gap: u32,
    /// `#RRGGBB`.
    pub background: &'a str,
    pub export: Export,
    pub output: &'a Path,
}

pub const MAX_COMPOSE_IMAGES: usize = 64;

/// Size of the finished grid.
pub fn grid_size(n: usize, columns: u32, cell: (u32, u32), gap: u32) -> (u64, u64) {
    let cols = columns.min(n as u32).max(1) as u64;
    let rows = (n as u64).div_ceil(cols);
    let (cw, ch, g) = (cell.0 as u64, cell.1 as u64, gap as u64);
    (cols * cw + (cols - 1) * g, rows * ch + (rows - 1) * g)
}

/// Validate a grid job and return the background as `"R G B"`.
fn check_compose(job: &ComposeJob) -> Result<String> {
    let n = job.inputs.len();
    if n == 0 || n > MAX_COMPOSE_IMAGES {
        return Err(bad(format!(
            "give between 1 and {MAX_COMPOSE_IMAGES} images"
        )));
    }
    check_dim("cell_width", job.cell.0)?;
    check_dim("cell_height", job.cell.1)?;
    if job.columns == 0 {
        return Err(bad("columns must be at least 1"));
    }
    if job.gap > 1000 {
        return Err(bad("gap must be at most 1000"));
    }
    let (w, h) = grid_size(n, job.columns, job.cell, job.gap);
    if w > MAX_DIM as u64 || h > MAX_DIM as u64 {
        return Err(bad(format!(
            "the grid would be {w}x{h}, larger than the {MAX_DIM}px limit"
        )));
    }
    parse_color(job.background)
}

/// Step 1 (cwd is the scratch dir): fit every input inside the cell box into `t{i}.v`.
/// thumbnail also applies EXIF rotation.
pub fn thumb_steps(job: &ComposeJob) -> Result<Vec<Args>> {
    check_compose(job)?;
    Ok(job
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            args![
                "thumbnail",
                s(input),
                format!("t{i}.v"),
                job.cell.0,
                "--height",
                job.cell.1
            ]
        })
        .collect())
}

/// Step 2: given the band count of each `t{i}.v`, put transparency on the background
/// (`flatten` only accepts images that have alpha) and join the cells into `grid.v`.
/// ponytail: grey+alpha cells are flattened onto the background's red channel only.
pub fn join_steps(job: &ComposeJob, bands: &[u32]) -> Result<Vec<Args>> {
    let bg = check_compose(job)?;
    let mut steps = vec![];
    let mut names = vec![];
    for (i, b) in bands.iter().enumerate() {
        let (t, c) = (format!("t{i}.v"), format!("c{i}.v"));
        match b {
            4 => steps.push(args!["flatten", t, c.clone(), "--background", bg.clone()]),
            2 => steps.push(args![
                "flatten",
                t,
                c.clone(),
                "--background",
                bg.split(' ').next().unwrap_or("255")
            ]),
            _ => steps.push(args!["copy", t, c.clone()]),
        }
        names.push(c);
    }
    // arrayjoin crashes (SIGSEGV, vips 8.14) on a single input.
    if names.len() == 1 {
        steps.push(args!["copy", names[0].clone(), "grid.v"]);
        return Ok(steps);
    }
    steps.push(args![
        "arrayjoin",
        names.join(" "),
        "grid.v",
        "--across",
        job.columns.min(job.inputs.len() as u32),
        "--shim",
        job.gap,
        "--background",
        bg,
        "--halign",
        "centre",
        "--valign",
        "centre"
    ]);
    Ok(steps)
}

/// Run a grid composition; scratch files live in a temp dir that is always removed.
pub async fn compose(engines: &Engines, sandbox: &Sandbox, job: ComposeJob<'_>) -> Result<()> {
    for p in job.inputs.iter().map(PathBuf::as_path).chain([job.output]) {
        check_vips_path(p)?;
    }
    let thumbs = thumb_steps(&job)?;
    let work = sandbox.make_temp_dir().await?;
    let _cleanup = Scratch::new(&work);
    check_vips_path(&work)?;
    for step in thumbs {
        run_vips(engines, &work, step).await?;
    }
    let mut bands = vec![];
    for i in 0..job.inputs.len() {
        bands.push(header_bands(engines, &work.join(format!("t{i}.v"))).await?);
    }
    for step in join_steps(&job, &bands)? {
        run_vips(engines, &work, step).await?;
    }
    finish(
        engines,
        &work,
        &work.join("grid.v"),
        job.output,
        &job.export,
    )
    .await
}

/// Commands that paint `fill` into `fill.v` (8-bit RGB, cwd is the scratch dir): the x or y
/// coordinate of every pixel scaled linearly from one colour to the other.
pub fn fill_steps(fill: &Fill, width: u32, height: u32) -> Result<Vec<Args>> {
    check_dim("width", width)?;
    check_dim("height", height)?;
    let rgb = |c: &str| -> Result<[f64; 3]> {
        let v: Vec<f64> = parse_color(c)?
            .split(' ')
            .filter_map(|n| n.parse().ok())
            .collect();
        Ok([v[0], v[1], v[2]])
    };
    let (from, to, vertical) = match fill {
        Fill::Solid { color } => (rgb(color)?, rgb(color)?, false),
        Fill::Gradient {
            from,
            to,
            direction,
        } => (
            rgb(from)?,
            rgb(to)?,
            *direction == GradientDirection::Vertical,
        ),
    };
    let span = if vertical { height } else { width };
    let div = (span.max(2) - 1) as f64;
    let join = |v: [f64; 3]| format!("{} {} {}", v[0], v[1], v[2]);
    let k = [0, 1, 2].map(|i| (to[i] - from[i]) / div);
    Ok(vec![
        args!["xyz", "xy.v", width, height],
        args![
            "extract_band",
            "xy.v",
            "axis.v",
            if vertical { 1 } else { 0 }
        ],
        args!["linear", "axis.v", "lin.v", join(k), join(from)],
        args!["cast", "lin.v", "fill.v", "uchar"],
    ])
}

/// Render a procedural fill to `output`.
pub async fn render_fill(
    engines: &Engines,
    sandbox: &Sandbox,
    fill: &Fill,
    (width, height): (u32, u32),
    export: &Export,
    output: &Path,
) -> Result<()> {
    check_vips_path(output)?;
    let steps = fill_steps(fill, width, height)?;
    let work = sandbox.make_temp_dir().await?;
    let _cleanup = Scratch::new(&work);
    check_vips_path(&work)?;
    for step in steps {
        run_vips(engines, &work, step).await?;
    }
    finish(engines, &work, &work.join("fill.v"), output, export).await
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

    #[test]
    fn grid_sizes() {
        assert_eq!(grid_size(5, 3, (100, 50), 10), (320, 110));
        assert_eq!(grid_size(2, 5, (100, 50), 10), (210, 50)); // fewer images than columns
        assert_eq!(grid_size(1, 1, (7, 9), 0), (7, 9));
    }

    #[test]
    fn compose_command_construction() {
        let inputs = [PathBuf::from("/w/a.png"), PathBuf::from("/w/b.jpg")];
        let job = ComposeJob {
            inputs: &inputs,
            columns: 2,
            cell: (100, 80),
            gap: 4,
            background: "#102030",
            export: Export {
                format: ImageFormat::Png,
                quality: 90,
                strip_metadata: true,
            },
            output: Path::new("/w/o.png"),
        };
        let thumbs = thumb_steps(&job).unwrap();
        assert_eq!(
            thumbs[0],
            ["thumbnail", "/w/a.png", "t0.v", "100", "--height", "80"]
        );
        assert_eq!(thumbs[1][1], "/w/b.jpg");
        // alpha is flattened, plain RGB is copied, grey+alpha uses one background value
        let steps = join_steps(&job, &[4, 3]).unwrap();
        assert_eq!(
            steps[0],
            ["flatten", "t0.v", "c0.v", "--background", "16 32 48"]
        );
        assert_eq!(steps[1], ["copy", "t1.v", "c1.v"]);
        assert_eq!(join_steps(&job, &[2, 1]).unwrap()[0][4], "16");
        assert_eq!(
            join_steps(&job, &[3]).unwrap()[1],
            ["copy", "c0.v", "grid.v"]
        );
        assert_eq!(
            steps[2],
            [
                "arrayjoin",
                "c0.v c1.v",
                "grid.v",
                "--across",
                "2",
                "--shim",
                "4",
                "--background",
                "16 32 48",
                "--halign",
                "centre",
                "--valign",
                "centre"
            ]
        );
        let with = |f: &dyn Fn(&mut ComposeJob)| {
            let mut j = ComposeJob {
                inputs: &inputs,
                background: "#102030",
                output: Path::new("/w/o"),
                ..job
            };
            f(&mut j);
            thumb_steps(&j).is_err()
        };
        assert!(with(&|j| j.columns = 0));
        assert!(with(&|j| j.cell = (0, 5)));
        assert!(with(&|j| j.gap = 1001));
        assert!(with(&|j| j.background = "red"));
        assert!(with(&|j| j.cell = (MAX_DIM, 10)));
        assert!(with(&|j| j.inputs = &[]));
    }

    #[test]
    fn fill_command_construction() {
        let g = Fill::Gradient {
            from: "#000000".into(),
            to: "#ff0000".into(),
            direction: GradientDirection::Horizontal,
        };
        let steps = fill_steps(&g, 256, 10).unwrap();
        assert_eq!(steps[0], ["xyz", "xy.v", "256", "10"]);
        assert_eq!(steps[1], ["extract_band", "xy.v", "axis.v", "0"]);
        assert_eq!(steps[2], ["linear", "axis.v", "lin.v", "1 0 0", "0 0 0"]);
        assert_eq!(steps[3], ["cast", "lin.v", "fill.v", "uchar"]);
        let v = Fill::Gradient {
            from: "#000000".into(),
            to: "#ff0000".into(),
            direction: GradientDirection::Vertical,
        };
        let steps = fill_steps(&v, 10, 256).unwrap();
        assert_eq!(steps[1][3], "1");
        let solid = Fill::Solid {
            color: "#0a0b0c".into(),
        };
        assert_eq!(
            fill_steps(&solid, 1, 1).unwrap()[2][3..],
            ["0 0 0", "10 11 12"]
        );
        assert!(fill_steps(&solid, 0, 5).is_err());
        assert!(fill_steps(&solid, MAX_DIM + 1, 5).is_err());
        assert!(
            fill_steps(
                &Fill::Solid {
                    color: "red".into()
                },
                5,
                5
            )
            .is_err()
        );
    }
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
