//! ffmpeg wrapper: compiles an ordered list of operations into ONE ffmpeg invocation with a
//! single `-filter_complex` graph, runs it, and turns `-progress` output into job progress.
//! ponytail: software encoders only (libx264 / libvpx-vp9); hardware encoder detection is a
//! later addition and would only change `encoder_args`.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use super::Engines;
use crate::{
    assets::AssetRef,
    engine::vips::{FlipDirection, parse_color},
    error::{Error, Result},
    jobs::Progress,
    sandbox::{Sandbox, Scratch},
};

/// Largest width or height accepted in operations and produced in output.
/// ponytail: constants; make them env settings if someone needs more.
pub const MAX_DIM: u32 = 4096;
pub const MAX_OPS: usize = 32;
pub const MAX_OUTPUT_SECS: f64 = 3600.0;
pub const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub type Args = Vec<String>;

/// One step of a video edit. Steps run in the order given, each on the previous result,
/// so a `trim` after a `speed` refers to the already-sped-up timeline.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VideoOp {
    /// Keep only part of the timeline. Give start_secs, end_secs or both.
    Trim {
        /// Start time in seconds (default 0).
        start_secs: Option<f64>,
        /// End time in seconds (default: the end).
        end_secs: Option<f64>,
    },
    /// Cut out a rectangle. Coordinates are pixels from the top-left of the current frame.
    Crop {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    /// Change the frame size. Give width, height or both; with one, the other follows the
    /// aspect ratio. With both, the picture is stretched to exactly that size.
    Resize {
        width: Option<u32>,
        height: Option<u32>,
    },
    /// Rotate clockwise by 90, 180 or 270 degrees.
    Rotate { degrees: i32 },
    /// Mirror the picture.
    Flip { direction: FlipDirection },
    /// Play faster (above 1) or slower (below 1); audio is adjusted to match.
    /// Between 0.1 and 16.
    Speed { factor: f64 },
    /// Colour adjustments. Give at least one field.
    Color {
        /// -1.0 (darker) to 1.0 (brighter); 0 leaves it unchanged.
        brightness: Option<f64>,
        /// 0.0 to 4.0; 1.0 leaves it unchanged.
        contrast: Option<f64>,
        /// 0.0 to 3.0; 1.0 leaves it unchanged, 0 is black and white.
        saturation: Option<f64>,
    },
    /// Draw text on every frame.
    Text {
        /// The text to draw (plain text, up to 1000 characters; `%` has no special meaning).
        text: String,
        /// Distance in pixels from the left edge.
        x: i32,
        /// Distance in pixels from the top edge.
        y: i32,
        /// Font size in pixels, default 32.
        size: Option<u32>,
        /// Colour as `#RRGGBB`, default white.
        color: Option<String>,
    },
    /// Place an image on top of every frame; its transparency is respected.
    Overlay {
        #[serde(flatten)]
        source: AssetRef,
        /// Left edge of the overlay in pixels.
        x: i32,
        /// Top edge of the overlay in pixels.
        y: i32,
        /// Scale the overlay to this width first (height follows the aspect ratio).
        width: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VideoFormat {
    /// H.264 video + AAC audio in an .mp4 container (plays everywhere).
    Mp4,
    /// VP9 video + Opus audio in a .webm container.
    Webm,
}

impl VideoFormat {
    pub fn ext(self) -> &'static str {
        match self {
            VideoFormat::Mp4 => "mp4",
            VideoFormat::Webm => "webm",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Export {
    pub format: VideoFormat,
    /// Constant rate factor, 0 (best) to 51; lower means higher quality and bigger files.
    pub crf: u8,
    pub strip_metadata: bool,
    /// Copy streams without re-encoding (instant, lossless). Only valid for a lone `trim`
    /// or no operations; cuts snap to the nearest earlier keyframe.
    pub stream_copy: bool,
}

pub struct Spec<'a> {
    pub input: &'a Path,
    /// Duration of the input in seconds, from ffprobe.
    pub input_secs: f64,
    pub has_audio: bool,
    /// Frame rate of the input, from ffprobe; keeps `speed` from padding the output with
    /// extra frames (ffmpeg's constant-frame-rate output otherwise rounds the length up).
    pub fps: Option<f64>,
    /// Resolved sources of the `Overlay` ops, in the order those ops appear.
    pub overlays: Vec<PathBuf>,
    pub ops: &'a [VideoOp],
    pub export: Export,
    pub output: &'a Path,
}

#[derive(Debug)]
pub struct Compiled {
    pub args: Args,
    /// (file name, content): user text, written into ffmpeg's working directory and read by
    /// `drawtext=textfile=`, so it never enters the filtergraph.
    pub text_files: Vec<(String, String)>,
    /// Expected duration of the output, used to turn `-progress` into a fraction.
    pub out_secs: f64,
}

pub(super) fn bad(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

pub(super) fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn check_dim(name: &str, v: u32) -> Result<()> {
    if v == 0 || v > MAX_DIM {
        return Err(bad(format!("{name} must be between 1 and {MAX_DIM}")));
    }
    Ok(())
}

pub(super) fn check_range(name: &str, v: f64, lo: f64, hi: f64) -> Result<()> {
    if !(v.is_finite() && (lo..=hi).contains(&v)) {
        return Err(bad(format!("{name} must be between {lo} and {hi}")));
    }
    Ok(())
}

/// Validate a trim and return (start, end) with `end` clamped to `len`.
pub(super) fn trim_bounds(start: Option<f64>, end: Option<f64>, len: f64) -> Result<(f64, f64)> {
    if start.is_none() && end.is_none() {
        return Err(bad("trim needs start_secs, end_secs or both"));
    }
    let st = start.unwrap_or(0.0);
    check_range("start_secs", st, 0.0, 1.0e7)?;
    let en = match end {
        Some(e) => {
            check_range("end_secs", e, 0.0, 1.0e7)?;
            e
        }
        None => len,
    };
    if st >= len {
        return Err(bad(format!(
            "trim start_secs {st} is past the end ({len:.2}s) of the current timeline"
        )));
    }
    if en <= st {
        return Err(bad("trim end_secs must be greater than start_secs"));
    }
    Ok((st, en.min(len)))
}

/// Output duration after applying `ops` to an input of `input_secs`.
pub fn expected_duration(input_secs: f64, ops: &[VideoOp]) -> Result<f64> {
    let mut cur = input_secs;
    for op in ops {
        match op {
            VideoOp::Trim {
                start_secs,
                end_secs,
            } => {
                let (st, en) = trim_bounds(*start_secs, *end_secs, cur)?;
                cur = en - st;
            }
            VideoOp::Speed { factor } => {
                check_range("factor", *factor, 0.1, 16.0)?;
                cur /= factor;
            }
            _ => {}
        }
    }
    Ok(cur)
}

/// `atempo` only accepts 0.5 to 2.0 per instance, so chain instances for other factors.
pub fn atempo_chain(mut f: f64) -> String {
    let mut parts = vec![];
    while f > 2.0 {
        parts.push("atempo=2.0".to_string());
        f /= 2.0;
    }
    while f < 0.5 {
        parts.push("atempo=0.5".to_string());
        f /= 0.5;
    }
    parts.push(format!("atempo={f}"));
    parts.join(",")
}

/// Filtergraph under construction: current video/audio labels plus a counter for fresh ones.
struct Chain {
    graph: Vec<String>,
    v: String,
    a: Option<String>,
    n: usize,
}

impl Chain {
    fn video(&mut self, filter: &str) {
        self.n += 1;
        let out = format!("v{}", self.n);
        self.graph.push(format!("[{}]{filter}[{out}]", self.v));
        self.v = out;
    }

    fn audio(&mut self, filter: &str) {
        if let Some(a) = &self.a {
            self.n += 1;
            let out = format!("a{}", self.n);
            self.graph.push(format!("[{a}]{filter}[{out}]"));
            self.a = Some(out);
        }
    }
}

/// `-map` wants `[label]` for filter outputs but a bare specifier for input streams.
fn map_arg(label: &str) -> String {
    if label.starts_with('0') {
        label.to_string()
    } else {
        format!("[{label}]")
    }
}

fn hex_color(c: &str) -> Result<String> {
    parse_color(c)?; // validates #RRGGBB
    Ok(format!("0x{}", c.trim_start_matches('#')))
}

pub(super) fn base_args() -> Args {
    ["-nostdin", "-hide_banner", "-loglevel", "error", "-nostats"]
        .iter()
        .chain(&["-progress", "pipe:1"])
        .map(|x| x.to_string())
        .collect()
}

pub(super) fn push_input(a: &mut Args, path: &Path) {
    a.extend(["-protocol_whitelist".into(), "file,pipe".into()]);
    a.extend(["-i".into(), s(path)]);
}

pub(super) fn encoder_args(e: &Export) -> Args {
    let crf = e.crf.to_string();
    let v: &[&str] = match e.format {
        VideoFormat::Mp4 => &[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            &crf,
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
        ],
        VideoFormat::Webm => &[
            "-c:v",
            "libvpx-vp9",
            "-b:v",
            "0",
            "-crf",
            &crf,
            "-row-mt",
            "1",
            "-deadline",
            "good",
            "-cpu-used",
            "4",
            "-c:a",
            "libopus",
            "-b:a",
            "96k",
        ],
    };
    v.iter().map(|x| x.to_string()).collect()
}

/// Compile the whole edit into one ffmpeg argument list.
pub fn compile(sp: &Spec) -> Result<Compiled> {
    if sp.ops.len() > MAX_OPS {
        return Err(bad(format!("at most {MAX_OPS} operations per call")));
    }
    if !(sp.input_secs.is_finite() && sp.input_secs > 0.0) {
        return Err(bad("input has no usable duration"));
    }
    let out_secs = expected_duration(sp.input_secs, sp.ops)?;
    if out_secs > MAX_OUTPUT_SECS {
        return Err(bad(format!(
            "output would be {out_secs:.0}s, longer than the {MAX_OUTPUT_SECS}s limit"
        )));
    }
    if sp.export.stream_copy {
        return copy_args(sp, out_secs);
    }

    let mut a = base_args();
    push_input(&mut a, sp.input);
    for o in &sp.overlays {
        push_input(&mut a, o);
    }
    let mut c = Chain {
        graph: vec![],
        v: "0:v:0".into(),
        a: sp.has_audio.then(|| "0:a:0".to_string()),
        n: 0,
    };
    let mut text_files = vec![];
    let mut overlays = 0usize;
    for op in sp.ops {
        match op {
            VideoOp::Trim {
                start_secs,
                end_secs,
            } => {
                let mut o = vec![];
                if let Some(v) = start_secs {
                    o.push(format!("start={v}"));
                }
                if let Some(v) = end_secs {
                    o.push(format!("end={v}"));
                }
                let o = o.join(":");
                c.video(&format!("trim={o},setpts=PTS-STARTPTS"));
                c.audio(&format!("atrim={o},asetpts=PTS-STARTPTS"));
            }
            VideoOp::Crop {
                x,
                y,
                width,
                height,
            } => {
                check_dim("width", *width)?;
                check_dim("height", *height)?;
                c.video(&format!("crop={width}:{height}:{x}:{y}"));
            }
            VideoOp::Resize { width, height } => {
                if width.is_none() && height.is_none() {
                    return Err(bad("resize needs width, height or both"));
                }
                for (n, v) in [("width", width), ("height", height)] {
                    if let Some(v) = v {
                        check_dim(n, *v)?;
                    }
                }
                let w = width.map_or("-2".to_string(), |v| v.to_string());
                let h = height.map_or("-2".to_string(), |v| v.to_string());
                c.video(&format!("scale={w}:{h}"));
            }
            VideoOp::Rotate { degrees } => match degrees.rem_euclid(360) {
                90 => c.video("transpose=1"),
                180 => c.video("hflip,vflip"),
                270 => c.video("transpose=2"),
                _ => return Err(bad("degrees must be 90, 180 or 270 (or -90, -180, -270)")),
            },
            VideoOp::Flip { direction } => c.video(match direction {
                FlipDirection::Horizontal => "hflip",
                FlipDirection::Vertical => "vflip",
            }),
            VideoOp::Speed { factor } => {
                match sp.fps {
                    Some(fps) => c.video(&format!("setpts=PTS/{factor},fps={fps}")),
                    None => c.video(&format!("setpts=PTS/{factor}")),
                }
                c.audio(&atempo_chain(*factor));
            }
            VideoOp::Color {
                brightness,
                contrast,
                saturation,
            } => {
                if brightness.is_none() && contrast.is_none() && saturation.is_none() {
                    return Err(bad("color needs brightness, contrast or saturation"));
                }
                let mut o = vec![];
                for (name, v, lo, hi) in [
                    ("brightness", brightness, -1.0, 1.0),
                    ("contrast", contrast, 0.0, 4.0),
                    ("saturation", saturation, 0.0, 3.0),
                ] {
                    if let Some(v) = v {
                        check_range(name, *v, lo, hi)?;
                        o.push(format!("{name}={v}"));
                    }
                }
                c.video(&format!("eq={}", o.join(":")));
            }
            VideoOp::Text {
                text,
                x,
                y,
                size,
                color,
            } => {
                let size = size.unwrap_or(32);
                if text.is_empty() || text.chars().count() > 1000 {
                    return Err(bad("text must be 1 to 1000 characters"));
                }
                if !(1..=1000).contains(&size) {
                    return Err(bad("size must be between 1 and 1000"));
                }
                let col = hex_color(color.as_deref().unwrap_or("#ffffff"))?;
                let name = format!("t{}.txt", text_files.len());
                // expansion=none: no %{...} expressions from user text.
                c.video(&format!(
                    "drawtext=textfile={name}:expansion=none:x={x}:y={y}:fontsize={size}:fontcolor={col}"
                ));
                text_files.push((name, text.clone()));
            }
            VideoOp::Overlay { x, y, width, .. } => {
                overlays += 1;
                if overlays > sp.overlays.len() {
                    return Err(bad("overlay source was not resolved"));
                }
                let input = overlays; // input 0 is the video itself
                c.n += 1;
                let src = if let Some(w) = width {
                    check_dim("width", *w)?;
                    let sc = format!("ov{}", c.n);
                    c.graph.push(format!("[{input}:v]scale={w}:-1[{sc}]"));
                    sc
                } else {
                    format!("{input}:v")
                };
                let out = format!("v{}", c.n);
                c.graph
                    .push(format!("[{}][{src}]overlay={x}:{y}[{out}]", c.v));
                c.v = out;
            }
        }
    }
    // h264/vp9 want even dimensions and 4:2:0.
    c.video("scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p");
    a.extend(["-filter_complex".into(), c.graph.join(";")]);
    a.extend(["-map".into(), map_arg(&c.v)]);
    if let Some(au) = &c.a {
        a.extend(["-map".into(), map_arg(au)]);
    }
    if sp.export.strip_metadata {
        a.extend(["-map_metadata".into(), "-1".into()]);
    }
    a.extend(encoder_args(&sp.export));
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files,
        out_secs,
    })
}

fn copy_args(sp: &Spec, out_secs: f64) -> Result<Compiled> {
    let trim = match sp.ops {
        [] => None,
        [
            VideoOp::Trim {
                start_secs,
                end_secs,
            },
        ] => Some((*start_secs, *end_secs)),
        _ => {
            return Err(bad(
                "stream_copy only works with no operations or a single trim; \
                 other operations need re-encoding",
            ));
        }
    };
    let mut a = base_args();
    if let Some((start, _)) = trim
        && let Some(st) = start.filter(|v| *v > 0.0)
    {
        a.extend(["-ss".into(), st.to_string()]);
    }
    push_input(&mut a, sp.input);
    if trim.is_some() {
        a.extend(["-t".into(), out_secs.to_string()]);
    }
    a.extend([
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ]);
    a.extend(["-c".into(), "copy".into()]);
    a.extend(["-avoid_negative_ts".into(), "make_zero".into()]);
    if sp.export.strip_metadata {
        a.extend(["-map_metadata".into(), "-1".into()]);
    }
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files: vec![],
        out_secs,
    })
}

/// `out_time_us=1500000` -> fraction of `total_secs`, held below 1 until the process exits.
pub fn parse_progress(line: &str, total_secs: f64) -> Option<f32> {
    let us: f64 = line.strip_prefix("out_time_us=")?.trim().parse().ok()?;
    if total_secs <= 0.0 || us < 0.0 {
        return None;
    }
    Some(((us / 1.0e6 / total_secs) as f32).clamp(0.0, 0.99))
}

/// Compile and run an edit.
pub async fn edit(
    engines: &Engines,
    sandbox: &Sandbox,
    sp: Spec<'_>,
    progress: &Progress,
) -> Result<()> {
    execute(engines, sandbox, compile(&sp)?, progress).await
}

/// Run an already compiled command. Scratch files live in a temp dir that is removed on every
/// exit, including cancellation.
pub async fn execute(
    engines: &Engines,
    sandbox: &Sandbox,
    compiled: Compiled,
    progress: &Progress,
) -> Result<()> {
    let work = sandbox.make_temp_dir().await?;
    let _cleanup = Scratch::new(&work);
    for (name, content) in &compiled.text_files {
        let p = work.join(name);
        tokio::fs::write(&p, content)
            .await
            .map_err(|e| Error::io(&p, e))?;
    }
    run(engines, &work, compiled.args, compiled.out_secs, progress).await
}

/// Grab one frame at `at_secs` as an image file.
pub async fn extract_frame(
    engines: &Engines,
    input: &Path,
    at_secs: f64,
    output: &Path,
) -> Result<()> {
    let mut a = base_args();
    a.extend(["-ss".into(), at_secs.to_string()]);
    push_input(&mut a, input);
    a.extend(["-frames:v".into(), "1".into(), s(output)]);
    run(engines, Path::new("."), a, 0.0, &Progress::default()).await
}

pub(super) async fn run(
    engines: &Engines,
    cwd: &Path,
    args: Args,
    total_secs: f64,
    progress: &Progress,
) -> Result<()> {
    let mut cmd = Command::new(&engines.ffmpeg);
    cmd.args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true); // dropping this future (cancel, timeout) kills ffmpeg
    let mut child = cmd.spawn().map_err(|e| Error::io(&engines.ffmpeg, e))?;
    let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(Error::Engine {
            program: "ffmpeg",
            status: "no output pipes".into(),
            stderr_tail: String::new(),
        });
    };
    let watch = async {
        let mut lines = BufReader::new(out).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(f) = parse_progress(&l, total_secs) {
                progress.set(f);
            }
        }
    };
    let collect = async {
        let mut tail: VecDeque<String> = VecDeque::new();
        let mut lines = BufReader::new(err).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tail.len() == 20 {
                tail.pop_front();
            }
            tail.push_back(l);
        }
        tail
    };
    let (_, tail, status) = tokio::join!(watch, collect, child.wait());
    let status = status.map_err(|e| Error::io(&engines.ffmpeg, e))?;
    if !status.success() {
        return Err(Error::Engine {
            program: "ffmpeg",
            status: status.to_string(),
            stderr_tail: Vec::from(tail).join("\n"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export() -> Export {
        Export {
            format: VideoFormat::Mp4,
            crf: 23,
            strip_metadata: true,
            stream_copy: false,
        }
    }

    fn spec<'a>(ops: &'a [VideoOp], audio: bool, overlays: Vec<PathBuf>) -> Spec<'a> {
        Spec {
            input: Path::new("/w/in.mp4"),
            input_secs: 10.0,
            has_audio: audio,
            fps: Some(10.0),
            overlays,
            ops,
            export: export(),
            output: Path::new("/w/out.mp4"),
        }
    }

    fn graph(c: &Compiled) -> String {
        let i = c.args.iter().position(|a| a == "-filter_complex").unwrap();
        c.args[i + 1].clone()
    }

    fn trim(s: Option<f64>, e: Option<f64>) -> VideoOp {
        VideoOp::Trim {
            start_secs: s,
            end_secs: e,
        }
    }

    #[test]
    fn full_command_for_trim_resize_speed_with_audio() {
        let ops = [
            trim(Some(1.0), Some(5.0)),
            VideoOp::Resize {
                width: Some(640),
                height: None,
            },
            VideoOp::Speed { factor: 2.0 },
        ];
        let c = compile(&spec(&ops, true, vec![])).unwrap();
        assert_eq!(c.out_secs, 2.0);
        assert_eq!(
            graph(&c),
            "[0:v:0]trim=start=1:end=5,setpts=PTS-STARTPTS[v1];\
             [0:a:0]atrim=start=1:end=5,asetpts=PTS-STARTPTS[a2];\
             [v1]scale=640:-2[v3];\
             [v3]setpts=PTS/2,fps=10[v4];\
             [a2]atempo=2[a5];\
             [v4]scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p[v6]"
        );
        let want: Args = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostats",
            "-progress",
            "pipe:1",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
            "/w/in.mp4",
            "-filter_complex",
        ]
        .iter()
        .map(|x| x.to_string())
        .collect();
        assert_eq!(c.args[..12], want[..]);
        let tail = &c.args[13..];
        assert_eq!(tail[..4], ["-map", "[v6]", "-map", "[a5]"]);
        assert_eq!(tail[4..6], ["-map_metadata", "-1"]);
        assert!(tail.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(tail.windows(2).any(|w| w == ["-crf", "23"]));
        assert_eq!(tail[tail.len() - 1], "/w/out.mp4");
        assert!(!c.args.contains(&"-y".to_string()), "never -y");
    }

    #[test]
    fn no_audio_means_no_audio_chain_or_map() {
        let ops = [VideoOp::Speed { factor: 4.0 }];
        let c = compile(&spec(&ops, false, vec![])).unwrap();
        assert!(!graph(&c).contains("atempo"));
        assert_eq!(c.args.iter().filter(|a| *a == "-map").count(), 1);
    }

    #[test]
    fn untouched_audio_maps_input_stream_directly() {
        let ops = [VideoOp::Flip {
            direction: FlipDirection::Horizontal,
        }];
        let c = compile(&spec(&ops, true, vec![])).unwrap();
        assert!(c.args.windows(2).any(|w| w == ["-map", "0:a:0"]));
    }

    #[test]
    fn geometry_and_colour_filters() {
        let ops = [
            VideoOp::Crop {
                x: 1,
                y: 2,
                width: 30,
                height: 40,
            },
            VideoOp::Rotate { degrees: -90 },
            VideoOp::Rotate { degrees: 180 },
            VideoOp::Resize {
                width: Some(10),
                height: Some(20),
            },
            VideoOp::Color {
                brightness: Some(0.1),
                contrast: None,
                saturation: Some(0.0),
            },
        ];
        let g = graph(&compile(&spec(&ops, false, vec![])).unwrap());
        for f in [
            "crop=30:40:1:2",
            "transpose=2",
            "hflip,vflip",
            "scale=10:20",
            "eq=brightness=0.1:saturation=0",
        ] {
            assert!(g.contains(f), "{f} missing from {g}");
        }
    }

    #[test]
    fn text_goes_through_a_file_not_the_graph() {
        let ops = [VideoOp::Text {
            text: "50% off: 'now' %{localtime}".into(),
            x: 5,
            y: -6,
            size: Some(20),
            color: Some("#ff8000".into()),
        }];
        let c = compile(&spec(&ops, false, vec![])).unwrap();
        assert_eq!(c.text_files.len(), 1);
        assert_eq!(c.text_files[0].0, "t0.txt");
        assert_eq!(c.text_files[0].1, "50% off: 'now' %{localtime}");
        let g = graph(&c);
        assert!(g.contains(
            "drawtext=textfile=t0.txt:expansion=none:x=5:y=-6:fontsize=20:fontcolor=0xff8000"
        ));
        assert!(!g.contains("localtime") && !g.contains("now"));
    }

    #[test]
    fn overlay_inputs_are_numbered_in_order() {
        let ops = [
            VideoOp::Overlay {
                source: AssetRef::default(),
                x: 1,
                y: 2,
                width: Some(50),
            },
            VideoOp::Overlay {
                source: AssetRef::default(),
                x: 3,
                y: 4,
                width: None,
            },
        ];
        let ovs = vec![PathBuf::from("/w/a.png"), PathBuf::from("/w/b.png")];
        let c = compile(&spec(&ops, false, ovs)).unwrap();
        let g = graph(&c);
        assert!(g.starts_with(
            "[1:v]scale=50:-1[ov1];[0:v:0][ov1]overlay=1:2[v1];[v1][2:v]overlay=3:4[v2]"
        ));
        assert_eq!(c.args.iter().filter(|a| *a == "-i").count(), 3);
        // A second overlay without a resolved source is an error.
        assert!(compile(&spec(&ops, false, vec![PathBuf::from("/w/a.png")])).is_err());
    }

    #[test]
    fn webm_encoder() {
        let mut sp = spec(&[], false, vec![]);
        sp.export.format = VideoFormat::Webm;
        sp.export.crf = 31;
        let c = compile(&sp).unwrap();
        assert!(c.args.windows(2).any(|w| w == ["-c:v", "libvpx-vp9"]));
        assert!(c.args.windows(2).any(|w| w == ["-crf", "31"]));
        assert!(c.args.windows(2).any(|w| w == ["-c:a", "libopus"]));
    }

    #[test]
    fn stream_copy_args_and_restrictions() {
        let mut sp = spec(&[], true, vec![]);
        let ops = [trim(Some(2.0), Some(5.0))];
        sp.ops = &ops;
        sp.export.stream_copy = true;
        let c = compile(&sp).unwrap();
        assert_eq!(c.out_secs, 3.0);
        assert!(c.args.windows(2).any(|w| w == ["-ss", "2"]));
        assert!(c.args.windows(2).any(|w| w == ["-t", "3"]));
        assert!(c.args.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(!c.args.contains(&"-filter_complex".to_string()));
        // -ss comes before the input it applies to.
        let ss = c.args.iter().position(|a| a == "-ss").unwrap();
        assert!(ss < c.args.iter().position(|a| a == "-i").unwrap());

        let ops = [VideoOp::Speed { factor: 2.0 }];
        sp.ops = &ops;
        assert!(compile(&sp).is_err());
        let ops = [trim(Some(1.0), None), trim(None, Some(3.0))];
        sp.ops = &ops;
        assert!(compile(&sp).is_err());
    }

    #[test]
    fn validation_errors() {
        let e = |op: VideoOp| compile(&spec(&[op], true, vec![])).is_err();
        assert!(e(trim(None, None)));
        assert!(e(trim(Some(5.0), Some(5.0))));
        assert!(e(trim(Some(-1.0), None)));
        assert!(e(trim(Some(11.0), None)));
        assert!(e(trim(Some(f64::NAN), None)));
        assert!(e(VideoOp::Speed { factor: 0.0 }));
        assert!(e(VideoOp::Speed { factor: 100.0 }));
        assert!(e(VideoOp::Rotate { degrees: 45 }));
        assert!(e(VideoOp::Resize {
            width: None,
            height: None
        }));
        assert!(e(VideoOp::Resize {
            width: Some(MAX_DIM + 1),
            height: None
        }));
        assert!(e(VideoOp::Crop {
            x: 0,
            y: 0,
            width: 0,
            height: 5
        }));
        assert!(e(VideoOp::Color {
            brightness: None,
            contrast: None,
            saturation: None
        }));
        assert!(e(VideoOp::Color {
            brightness: Some(2.0),
            contrast: None,
            saturation: None
        }));
        assert!(e(VideoOp::Text {
            text: String::new(),
            x: 0,
            y: 0,
            size: None,
            color: None
        }));
        assert!(e(VideoOp::Text {
            text: "x".into(),
            x: 0,
            y: 0,
            size: None,
            color: Some("red".into())
        }));
        // 10s input slowed to 0.1x is 100s: fine; a 3600s+ result is not.
        let mut sp = spec(&[VideoOp::Speed { factor: 0.1 }], false, vec![]);
        sp.input_secs = 400.0;
        assert!(compile(&sp).is_err());
    }

    #[test]
    fn durations_follow_the_timeline() {
        let ops = [
            VideoOp::Speed { factor: 2.0 },
            trim(Some(1.0), Some(3.0)),
            VideoOp::Speed { factor: 0.5 },
        ];
        // 10s -> 5s -> 2s -> 4s
        assert_eq!(expected_duration(10.0, &ops).unwrap(), 4.0);
        // end past the timeline is clamped
        assert_eq!(
            expected_duration(10.0, &[trim(Some(8.0), Some(99.0))]).unwrap(),
            2.0
        );
        assert_eq!(
            expected_duration(10.0, &[trim(None, Some(4.0))]).unwrap(),
            4.0
        );
    }

    #[test]
    fn atempo_chains() {
        assert_eq!(atempo_chain(1.5), "atempo=1.5");
        assert_eq!(atempo_chain(4.0), "atempo=2.0,atempo=2");
        assert_eq!(atempo_chain(0.25), "atempo=0.5,atempo=0.5");
        assert_eq!(
            atempo_chain(16.0),
            "atempo=2.0,atempo=2.0,atempo=2.0,atempo=2"
        );
    }

    #[test]
    fn progress_lines() {
        assert_eq!(parse_progress("out_time_us=1000000", 4.0), Some(0.25));
        assert_eq!(parse_progress("out_time_us=9000000", 4.0), Some(0.99));
        assert_eq!(parse_progress("out_time_us=N/A", 4.0), None);
        assert_eq!(parse_progress("out_time_us=-5", 4.0), None);
        assert_eq!(parse_progress("frame=3", 4.0), None);
        assert_eq!(parse_progress("out_time_us=100", 0.0), None);
    }

    #[test]
    fn op_json_shapes() {
        let ops: Vec<VideoOp> = serde_json::from_value(serde_json::json!([
            {"op":"trim","end_secs":2},
            {"op":"overlay","asset_id":"a","x":1,"y":2,"width":30},
            {"op":"rotate","degrees":90}
        ]))
        .unwrap();
        assert_eq!(ops.len(), 3);
        assert!(serde_json::from_value::<VideoOp>(serde_json::json!({"op":"zoom"})).is_err());
    }
}
