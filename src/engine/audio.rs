//! audio_edit compiler: an ordered list of operations becomes one ffmpeg audio filter chain.

use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;

use super::ffmpeg::{
    Args, Compiled, MAX_OPS, MAX_OUTPUT_BYTES, MAX_OUTPUT_SECS, atempo_chain, bad, base_args,
    check_range, push_input, s, trim_bounds,
};
use crate::error::Result;

/// One step of an audio edit. Steps run in the order given, each on the previous result.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AudioOp {
    /// Keep only part of the timeline. Give start_secs, end_secs or both.
    Trim {
        /// Start time in seconds (default 0).
        start_secs: Option<f64>,
        /// End time in seconds (default: the end).
        end_secs: Option<f64>,
    },
    /// Multiply the loudness: 1 leaves it unchanged, 0.5 is half as loud, 2 twice as loud.
    /// Between 0 and 10.
    Volume { factor: f64 },
    /// Fade in from silence over the first `secs` seconds.
    FadeIn { secs: f64 },
    /// Fade out to silence over the last `secs` seconds of the current timeline.
    FadeOut { secs: f64 },
    /// Play faster (above 1) or slower (below 1); pitch is preserved. Between 0.1 and 16.
    Speed { factor: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// MP3 (VBR, about 190 kbit/s).
    Mp3,
    /// Uncompressed 16-bit PCM WAV.
    Wav,
    /// AAC in an .m4a container (192 kbit/s).
    M4a,
}

impl AudioFormat {
    pub fn ext(self) -> &'static str {
        match self {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
            AudioFormat::M4a => "m4a",
        }
    }

    fn codec(self) -> &'static [&'static str] {
        match self {
            AudioFormat::Mp3 => &["-c:a", "libmp3lame", "-q:a", "2"],
            AudioFormat::Wav => &["-c:a", "pcm_s16le"],
            AudioFormat::M4a => &["-c:a", "aac", "-b:a", "192k"],
        }
    }
}

pub struct Spec<'a> {
    /// A file with at least one audio stream (an audio file, or a video to extract it from).
    pub input: &'a Path,
    /// Duration of the input in seconds, from ffprobe.
    pub input_secs: f64,
    pub ops: &'a [AudioOp],
    pub format: AudioFormat,
    pub strip_metadata: bool,
    pub output: &'a Path,
}

pub fn compile(sp: &Spec) -> Result<Compiled> {
    if sp.ops.len() > MAX_OPS {
        return Err(bad(format!("at most {MAX_OPS} operations per call")));
    }
    if !(sp.input_secs.is_finite() && sp.input_secs > 0.0) {
        return Err(bad("input has no usable duration"));
    }
    let mut cur = sp.input_secs;
    let mut f: Vec<String> = vec![];
    for op in sp.ops {
        match op {
            AudioOp::Trim {
                start_secs,
                end_secs,
            } => {
                let (st, en) = trim_bounds(*start_secs, *end_secs, cur)?;
                f.push(format!("atrim=start={st}:end={en},asetpts=PTS-STARTPTS"));
                cur = en - st;
            }
            AudioOp::Volume { factor } => {
                check_range("factor", *factor, 0.0, 10.0)?;
                f.push(format!("volume={factor}"));
            }
            AudioOp::FadeIn { secs } => {
                check_range("secs", *secs, 0.01, cur)?;
                f.push(format!("afade=t=in:st=0:d={secs}"));
            }
            AudioOp::FadeOut { secs } => {
                check_range("secs", *secs, 0.01, cur)?;
                f.push(format!("afade=t=out:st={}:d={secs}", cur - secs));
            }
            AudioOp::Speed { factor } => {
                check_range("factor", *factor, 0.1, 16.0)?;
                f.push(atempo_chain(*factor));
                cur /= factor;
            }
        }
    }
    if cur > MAX_OUTPUT_SECS {
        return Err(bad(format!(
            "output would be {cur:.0}s, longer than the {MAX_OUTPUT_SECS}s limit"
        )));
    }
    let mut a: Args = base_args();
    push_input(&mut a, sp.input);
    if f.is_empty() {
        a.extend(["-map".into(), "0:a:0".into()]);
    } else {
        a.extend([
            "-filter_complex".into(),
            format!("[0:a:0]{}[out]", f.join(",")),
        ]);
        a.extend(["-map".into(), "[out]".into()]);
    }
    if sp.strip_metadata {
        a.extend(["-map_metadata".into(), "-1".into()]);
    }
    a.extend(sp.format.codec().iter().map(|x| x.to_string()));
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files: vec![],
        out_secs: cur,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(ops: &[AudioOp]) -> Spec<'_> {
        Spec {
            input: Path::new("/w/in.mp4"),
            input_secs: 10.0,
            ops,
            format: AudioFormat::Mp3,
            strip_metadata: true,
            output: Path::new("/w/out.mp3"),
        }
    }

    fn graph(c: &Compiled) -> String {
        let i = c.args.iter().position(|a| a == "-filter_complex").unwrap();
        c.args[i + 1].clone()
    }

    #[test]
    fn full_command() {
        let ops = [
            AudioOp::Trim {
                start_secs: Some(2.0),
                end_secs: Some(8.0),
            },
            AudioOp::Speed { factor: 2.0 },
            AudioOp::Volume { factor: 0.5 },
            AudioOp::FadeIn { secs: 0.5 },
            AudioOp::FadeOut { secs: 1.0 },
        ];
        let c = compile(&spec(&ops)).unwrap();
        // 10s -> trim 6s -> speed 3s: the fade-out starts 1s before the end.
        assert_eq!(c.out_secs, 3.0);
        assert_eq!(
            graph(&c),
            "[0:a:0]atrim=start=2:end=8,asetpts=PTS-STARTPTS,atempo=2,volume=0.5,\
             afade=t=in:st=0:d=0.5,afade=t=out:st=2:d=1[out]"
        );
        assert_eq!(
            c.args[..12],
            [
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
                "-filter_complex"
            ]
        );
        assert!(c.args.windows(2).any(|w| w == ["-map", "[out]"]));
        assert!(c.args.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        assert!(c.args.windows(2).any(|w| w == ["-map_metadata", "-1"]));
        assert_eq!(c.args.last().unwrap(), "/w/out.mp3");
        assert!(!c.args.contains(&"-y".to_string()));
    }

    #[test]
    fn no_ops_just_extracts_and_converts() {
        let mut sp = spec(&[]);
        sp.format = AudioFormat::Wav;
        sp.strip_metadata = false;
        let c = compile(&sp).unwrap();
        assert!(!c.args.contains(&"-filter_complex".to_string()));
        assert!(c.args.windows(2).any(|w| w == ["-map", "0:a:0"]));
        assert!(c.args.windows(2).any(|w| w == ["-c:a", "pcm_s16le"]));
        assert!(!c.args.contains(&"-map_metadata".to_string()));
        assert_eq!(c.out_secs, 10.0);
    }

    #[test]
    fn validation_errors() {
        let e = |op: AudioOp| compile(&spec(&[op])).is_err();
        assert!(e(AudioOp::Volume { factor: -1.0 }));
        assert!(e(AudioOp::Volume { factor: 11.0 }));
        assert!(e(AudioOp::Volume { factor: f64::NAN }));
        assert!(e(AudioOp::FadeIn { secs: 0.0 }));
        assert!(e(AudioOp::FadeOut { secs: 11.0 }));
        assert!(e(AudioOp::Speed { factor: 0.0 }));
        assert!(e(AudioOp::Trim {
            start_secs: None,
            end_secs: None
        }));
        assert!(e(AudioOp::Trim {
            start_secs: Some(10.0),
            end_secs: None
        }));
        // a fade longer than what is left after a trim
        let ops = [
            AudioOp::Trim {
                start_secs: None,
                end_secs: Some(2.0),
            },
            AudioOp::FadeOut { secs: 3.0 },
        ];
        assert!(compile(&spec(&ops)).is_err());
        let mut sp = spec(&[]);
        sp.input_secs = 0.0;
        assert!(compile(&sp).is_err());
    }

    #[test]
    fn op_json_shapes() {
        let ops: Vec<AudioOp> = serde_json::from_value(serde_json::json!([
            {"op":"fade_in","secs":1},{"op":"volume","factor":2}
        ]))
        .unwrap();
        assert_eq!(ops.len(), 2);
        assert!(serde_json::from_value::<AudioOp>(serde_json::json!({"op":"echo"})).is_err());
    }
}
