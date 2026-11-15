use std::{path::Path, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::{Engines, stderr_tail};
use crate::error::{Error, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

/// What ffprobe found out about a file.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct MediaInfo {
    /// `image`, `video` or `audio`.
    pub kind: MediaKind,
    /// ffprobe container/format name, e.g. `mov,mp4,m4a,3gp,3g2,mj2` or `png_pipe`.
    pub format: String,
    /// Duration in seconds; absent for still images.
    pub duration_secs: Option<f64>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Width in pixels of the first video/image stream.
    pub width: Option<u32>,
    /// Height in pixels of the first video/image stream.
    pub height: Option<u32>,
    /// Frames per second of the first video stream; absent for stills and audio.
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub has_audio: bool,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: RawFormat,
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: String,
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    #[serde(default)]
    disposition: RawDisposition,
}

#[derive(Deserialize, Default)]
struct RawDisposition {
    #[serde(default)]
    attached_pic: u8,
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
pub fn parse(json: &str) -> Result<MediaInfo> {
    let raw: Raw = serde_json::from_str(json).map_err(|e| Error::Parse {
        program: "ffprobe",
        detail: e.to_string(),
    })?;
    // Cover art in audio files shows up as a video stream; it is not video.
    let video = raw
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic == 0);
    let audio = raw
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));
    let format = raw.format.format_name;
    // ponytail: stills are recognised by ffprobe's image demuxer names; add new demuxers here.
    let is_image = format == "image2" || format.ends_with("_pipe");
    let kind = match (video.is_some(), is_image) {
        (true, true) => MediaKind::Image,
        (true, false) => MediaKind::Video,
        (false, _) if audio.is_some() => MediaKind::Audio,
        _ => {
            return Err(Error::Parse {
                program: "ffprobe",
                detail: "no video or audio stream found".into(),
            });
        }
    };
    Ok(MediaInfo {
        kind,
        duration_secs: match kind {
            MediaKind::Image => None,
            _ => raw.format.duration.and_then(|d| d.parse().ok()),
        },
        size_bytes: raw.format.size.and_then(|s| s.parse().ok()).unwrap_or(0),
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        fps: match kind {
            MediaKind::Video => video
                .and_then(|v| v.avg_frame_rate.as_deref())
                .and_then(parse_rate),
            _ => None,
        },
        video_codec: video.and_then(|v| v.codec_name.clone()),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
        has_audio: audio.is_some(),
        format,
    })
}

/// `30000/1001` -> 29.97; `0/0` -> None.
fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0 && n != 0.0).then_some(n / d)
}

pub async fn probe(engines: &Engines, path: &Path) -> Result<MediaInfo> {
    let mut cmd = Command::new(&engines.ffprobe);
    cmd.args(["-v", "error", "-hide_banner"])
        .args(["-protocol_whitelist", "file,pipe"])
        .args(["-print_format", "json", "-show_format", "-show_streams"])
        .arg("-i")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Engine {
            program: "ffprobe",
            status: "timed out".into(),
            stderr_tail: String::new(),
        })?
        .map_err(|e| Error::io(&engines.ffprobe, e))?;
    if !out.status.success() {
        return Err(Error::Engine {
            program: "ffprobe",
            status: out.status.to_string(),
            stderr_tail: stderr_tail(&out.stderr),
        });
    }
    parse(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO: &str = r#"{"streams":[
      {"codec_type":"video","codec_name":"h264","width":64,"height":48,"avg_frame_rate":"30000/1001","disposition":{"attached_pic":0}},
      {"codec_type":"audio","codec_name":"aac"}],
      "format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"1.000000","size":"5000"}}"#;

    #[test]
    fn parses_video_with_audio() {
        let i = parse(VIDEO).unwrap();
        assert_eq!(i.kind, MediaKind::Video);
        assert_eq!((i.width, i.height), (Some(64), Some(48)));
        assert_eq!(i.duration_secs, Some(1.0));
        assert_eq!(i.size_bytes, 5000);
        assert!(i.has_audio);
        assert_eq!(i.audio_codec.as_deref(), Some("aac"));
        assert!((i.fps.unwrap() - 29.97).abs() < 0.01);
    }

    #[test]
    fn parses_still_image_without_duration() {
        let j = r#"{"streams":[{"codec_type":"video","codec_name":"png","width":32,"height":32,"avg_frame_rate":"25/1"}],
          "format":{"format_name":"png_pipe","duration":"0.040000","size":"99"}}"#;
        let i = parse(j).unwrap();
        assert_eq!(i.kind, MediaKind::Image);
        assert_eq!((i.duration_secs, i.fps), (None, None));
    }

    #[test]
    fn audio_with_cover_art_is_audio() {
        let j = r#"{"streams":[
          {"codec_type":"audio","codec_name":"mp3"},
          {"codec_type":"video","codec_name":"mjpeg","width":10,"height":10,"disposition":{"attached_pic":1}}],
          "format":{"format_name":"mp3","duration":"2.5","size":"10"}}"#;
        let i = parse(j).unwrap();
        assert_eq!(i.kind, MediaKind::Audio);
        assert_eq!(i.width, None);
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"streams":[],"format":{"format_name":"x"}}"#).is_err());
    }

    #[test]
    fn rate_zero_is_none() {
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(parse_rate("25/1"), Some(25.0));
    }
}
