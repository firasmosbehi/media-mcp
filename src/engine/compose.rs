//! video_compose compiler: joins videos and still images into one video, either re-encoded
//! through one `concat` filter graph or, when the clips already match, by stream copy.

use std::path::{Path, PathBuf};

use super::{
    ffmpeg::{
        Args, Compiled, Export, MAX_DIM, MAX_OUTPUT_BYTES, MAX_OUTPUT_SECS, base_args, check_range,
        encoder_args, push_input, s,
    },
    probe::{MediaInfo, MediaKind},
};
use crate::error::{Error, Result};

pub const MAX_CLIPS: usize = 100;

/// One resolved input of the composition.
pub struct Clip {
    pub path: PathBuf,
    pub info: MediaInfo,
    /// How long it plays: the display time of an image, or the (possibly shortened) length of a video.
    pub secs: f64,
    /// The caller set `secs` explicitly; stream copy cannot shorten clips.
    pub explicit_secs: bool,
}

impl Clip {
    fn image(&self) -> bool {
        self.info.kind == MediaKind::Image
    }
}

pub struct Spec<'a> {
    pub clips: &'a [Clip],
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub export: Export,
    pub output: &'a Path,
}

fn bad(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

pub fn compile(sp: &Spec) -> Result<Compiled> {
    if sp.clips.is_empty() || sp.clips.len() > MAX_CLIPS {
        return Err(bad(format!("give between 1 and {MAX_CLIPS} clips")));
    }
    let mut total = 0.0;
    for (i, c) in sp.clips.iter().enumerate() {
        if !(c.secs.is_finite() && c.secs >= 0.1) {
            return Err(bad(format!("clip {i}: secs must be at least 0.1")));
        }
        total += c.secs;
    }
    if total > MAX_OUTPUT_SECS {
        return Err(bad(format!(
            "output would be {total:.0}s, longer than the {MAX_OUTPUT_SECS}s limit"
        )));
    }
    if sp.export.stream_copy {
        return copy_args(sp, total);
    }
    // h264/vp9 want even dimensions.
    let (w, h) = (sp.width & !1, sp.height & !1);
    for (n, v) in [("width", w), ("height", h)] {
        if !(2..=MAX_DIM).contains(&v) {
            return Err(bad(format!("{n} must be between 2 and {MAX_DIM}")));
        }
    }
    check_range("fps", sp.fps as f64, 1.0, 120.0)?;
    let fps = sp.fps;

    let mut a = base_args();
    for c in sp.clips {
        if c.image() {
            a.extend([
                "-loop".into(),
                "1".into(),
                "-framerate".into(),
                fps.to_string(),
            ]);
            a.extend(["-t".into(), c.secs.to_string()]);
        }
        push_input(&mut a, &c.path);
    }
    let any_audio = sp.clips.iter().any(|c| c.info.has_audio);
    let mut g = vec![];
    let mut joined = String::new();
    for (i, c) in sp.clips.iter().enumerate() {
        let d = c.secs;
        let cut = if c.image() {
            String::new()
        } else {
            format!("trim=duration={d},setpts=PTS-STARTPTS,")
        };
        g.push(format!(
            "[{i}:v:0]{cut}scale={w}:{h}:force_original_aspect_ratio=decrease,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps},format=yuv420p[v{i}]"
        ));
        joined += &format!("[v{i}]");
        if any_audio {
            // Every clip gets an audio part of exactly its length, silent if it has none,
            // so concat keeps sound and picture in sync.
            let src = if c.info.has_audio {
                format!("[{i}:a:0]aresample=48000,aformat=channel_layouts=stereo,apad,")
            } else {
                "anullsrc=r=48000:cl=stereo,".to_string()
            };
            g.push(format!(
                "{src}atrim=duration={d},asetpts=PTS-STARTPTS[a{i}]"
            ));
            joined += &format!("[a{i}]");
        }
    }
    let n = sp.clips.len();
    let (flags, outs) = if any_audio {
        ("a=1", "[v][a]")
    } else {
        ("a=0", "[v]")
    };
    g.push(format!("{joined}concat=n={n}:v=1:{flags}{outs}"));
    a.extend(["-filter_complex".into(), g.join(";")]);
    a.extend(["-map".into(), "[v]".into()]);
    if any_audio {
        a.extend(["-map".into(), "[a]".into()]);
    }
    if sp.export.strip_metadata {
        a.extend(["-map_metadata".into(), "-1".into()]);
    }
    a.extend(encoder_args(&sp.export));
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files: vec![],
        out_secs: total,
    })
}

/// What must match for the concat demuxer to join clips without re-encoding.
fn signature(i: &MediaInfo) -> String {
    format!(
        "video {:?} {:?}x{:?} {:?}fps, audio {:?}",
        i.video_codec, i.width, i.height, i.fps, i.audio_codec
    )
}

fn copy_args(sp: &Spec, total: f64) -> Result<Compiled> {
    let first = signature(&sp.clips[0].info);
    for (i, c) in sp.clips.iter().enumerate() {
        if c.image() || c.explicit_secs {
            return Err(bad(format!(
                "stream_copy needs plain videos with no `secs`; clip {i} is an image or has secs set"
            )));
        }
        let sig = signature(&c.info);
        if sig != first {
            return Err(bad(format!(
                "stream_copy needs identical codecs, size and fps: clip 0 is [{first}] but clip {i} is [{sig}]"
            )));
        }
    }
    // The concat demuxer's list file; quotes in paths are escaped for its own syntax.
    let list: String = sp
        .clips
        .iter()
        .map(|c| format!("file '{}'\n", s(&c.path).replace('\'', r"'\''")))
        .collect();
    let mut a: Args = base_args();
    a.extend(["-f".into(), "concat".into(), "-safe".into(), "0".into()]);
    push_input(&mut a, Path::new("list.txt"));
    a.extend([
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ]);
    a.extend(["-c".into(), "copy".into()]);
    if sp.export.strip_metadata {
        a.extend(["-map_metadata".into(), "-1".into()]);
    }
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files: vec![("list.txt".into(), list)],
        out_secs: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ffmpeg::VideoFormat;

    fn info(kind: MediaKind, audio: bool) -> MediaInfo {
        MediaInfo {
            kind,
            format: "x".into(),
            duration_secs: Some(2.0),
            size_bytes: 1,
            width: Some(160),
            height: Some(120),
            fps: Some(10.0),
            video_codec: Some("h264".into()),
            audio_codec: audio.then(|| "aac".into()),
            has_audio: audio,
        }
    }

    fn clip(name: &str, kind: MediaKind, audio: bool, secs: f64) -> Clip {
        Clip {
            path: PathBuf::from(name),
            info: info(kind, audio),
            secs,
            explicit_secs: false,
        }
    }

    fn spec(clips: &[Clip], copy: bool) -> Spec<'_> {
        Spec {
            clips,
            width: 321,
            height: 200,
            fps: 25,
            export: Export {
                format: VideoFormat::Mp4,
                crf: 23,
                strip_metadata: true,
                stream_copy: copy,
            },
            output: Path::new("/w/out.mp4"),
        }
    }

    fn graph(c: &Compiled) -> String {
        let i = c.args.iter().position(|a| a == "-filter_complex").unwrap();
        c.args[i + 1].clone()
    }

    #[test]
    fn image_then_video_with_audio() {
        let clips = [
            clip("/w/a.png", MediaKind::Image, false, 3.0),
            clip("/w/b.mp4", MediaKind::Video, true, 1.5),
        ];
        let c = compile(&spec(&clips, false)).unwrap();
        assert_eq!(c.out_secs, 4.5);
        let sc = "scale=320:200:force_original_aspect_ratio=decrease,\
                  pad=320:200:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=25,format=yuv420p";
        assert_eq!(
            graph(&c),
            format!(
                "[0:v:0]{sc}[v0];\
                 anullsrc=r=48000:cl=stereo,atrim=duration=3,asetpts=PTS-STARTPTS[a0];\
                 [1:v:0]trim=duration=1.5,setpts=PTS-STARTPTS,{sc}[v1];\
                 [1:a:0]aresample=48000,aformat=channel_layouts=stereo,apad,\
                 atrim=duration=1.5,asetpts=PTS-STARTPTS[a1];\
                 [v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]"
            )
        );
        // Image input carries loop/framerate/duration before its -i; the video does not.
        let a = &c.args;
        let i0 = a.iter().position(|x| x == "-loop").unwrap();
        assert_eq!(
            a[i0..i0 + 8],
            [
                "-loop",
                "1",
                "-framerate",
                "25",
                "-t",
                "3",
                "-protocol_whitelist",
                "file,pipe"
            ]
        );
        assert_eq!(a.iter().filter(|x| *x == "-i").count(), 2);
        assert!(a.windows(4).any(|w| w == ["-map", "[v]", "-map", "[a]"]));
        assert!(a.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(!a.contains(&"-y".to_string()));
    }

    #[test]
    fn silent_clips_give_a_video_only_output() {
        let clips = [
            clip("/w/a.png", MediaKind::Image, false, 1.0),
            clip("/w/b.png", MediaKind::Image, false, 2.0),
        ];
        let c = compile(&spec(&clips, false)).unwrap();
        let g = graph(&c);
        assert!(g.ends_with("[v0][v1]concat=n=2:v=1:a=0[v]"), "{g}");
        assert!(!g.contains("anullsrc"));
        assert_eq!(c.args.iter().filter(|x| *x == "-map").count(), 1);
    }

    #[test]
    fn limits() {
        let one = [clip("/w/a.png", MediaKind::Image, false, 1.0)];
        assert!(compile(&spec(&[], false)).is_err());
        let mut sp = spec(&one, false);
        sp.width = 1;
        assert!(compile(&sp).is_err());
        sp.width = MAX_DIM + 2;
        assert!(compile(&sp).is_err());
        let mut sp = spec(&one, false);
        sp.fps = 0;
        assert!(compile(&sp).is_err());
        let long = [clip("/w/a.png", MediaKind::Image, false, 3601.0)];
        assert!(compile(&spec(&long, false)).is_err());
        let short = [clip("/w/a.png", MediaKind::Image, false, 0.01)];
        assert!(compile(&spec(&short, false)).is_err());
        let many: Vec<Clip> = (0..=MAX_CLIPS)
            .map(|_| clip("/w/a.png", MediaKind::Image, false, 1.0))
            .collect();
        assert!(compile(&spec(&many, false)).is_err());
    }

    #[test]
    fn stream_copy_uses_the_concat_demuxer() {
        let clips = [
            clip("/w/a.mp4", MediaKind::Video, true, 2.0),
            clip("/w/it's.mp4", MediaKind::Video, true, 2.0),
        ];
        let c = compile(&spec(&clips, true)).unwrap();
        assert_eq!(c.out_secs, 4.0);
        assert_eq!(c.text_files.len(), 1);
        assert_eq!(
            c.text_files[0].1,
            "file '/w/a.mp4'\nfile '/w/it'\\''s.mp4'\n"
        );
        let a = &c.args;
        assert!(a.windows(4).any(|w| w == ["-f", "concat", "-safe", "0"]));
        assert!(a.windows(2).any(|w| w == ["-i", "list.txt"]));
        assert!(a.windows(2).any(|w| w == ["-c", "copy"]));
        assert!(!a.contains(&"-filter_complex".to_string()));
    }

    #[test]
    fn stream_copy_rejects_mismatches_images_and_secs() {
        let mut odd = clip("/w/b.mp4", MediaKind::Video, true, 2.0);
        odd.info.width = Some(320);
        let clips = [clip("/w/a.mp4", MediaKind::Video, true, 2.0), odd];
        let e = compile(&spec(&clips, true)).unwrap_err().to_string();
        assert!(e.contains("identical") && e.contains("clip 1"), "{e}");

        let clips = [clip("/w/a.png", MediaKind::Image, false, 2.0)];
        assert!(compile(&spec(&clips, true)).is_err());

        let mut short = clip("/w/a.mp4", MediaKind::Video, true, 1.0);
        short.explicit_secs = true;
        assert!(compile(&spec(&[short], true)).is_err());
    }
}
