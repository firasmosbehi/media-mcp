//! `run_ffmpeg`: the escape hatch. The caller supplies ffmpeg arguments, but inputs and the
//! output can only be named through placeholders, so every path is resolved by the sandbox
//! before it reaches ffmpeg, and the server's own flags always win.
//! ponytail: a deny-list, not a parser: it blocks the options, filters and URL schemes that can
//! read other files or the network. New ffmpeg features that read files need adding here.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use super::ffmpeg::{
    Args, Compiled, MAX_OUTPUT_BYTES, MAX_OUTPUT_SECS, bad, base_args, push_input, s,
};
use crate::error::Result;

pub const MAX_INPUTS: usize = 8;
pub const MAX_ARGS: usize = 200;
pub const MAX_ARG_LEN: usize = 4000;

/// File type of the output; ffmpeg picks the muxer from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    Mp4,
    Webm,
    Mkv,
    Mov,
    Gif,
    Mp3,
    Wav,
    M4a,
    Ogg,
    Png,
    Jpg,
    Webp,
}

impl OutputFormat {
    pub fn ext(self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Webm => "webm",
            OutputFormat::Mkv => "mkv",
            OutputFormat::Mov => "mov",
            OutputFormat::Gif => "gif",
            OutputFormat::Mp3 => "mp3",
            OutputFormat::Wav => "wav",
            OutputFormat::M4a => "m4a",
            OutputFormat::Ogg => "ogg",
            OutputFormat::Png => "png",
            OutputFormat::Jpg => "jpg",
            OutputFormat::Webp => "webp",
        }
    }
}

pub struct Spec<'a> {
    /// Resolved sandbox paths of the inputs, in `{in0}`, `{in1}`... order.
    pub inputs: &'a [PathBuf],
    pub args: &'a [String],
    /// Used to turn `-progress` into a fraction; 0 means unknown.
    pub expected_secs: f64,
    pub output: &'a Path,
}

/// Options that read or write files outside our control, change what the server forces, or
/// make ffmpeg overwrite things.
const BANNED_OPTIONS: &[&str] = &[
    "-filter_script",
    "-filter_complex_script",
    "-attach",
    "-dump_attachment",
    "-passlogfile",
    "-vstats",
    "-report",
    "-protocol_whitelist",
    "-protocol_blacklist",
    "-y",
    "-n",
    "-fs",
    "-headers",
    "-user_agent",
    "-cookies",
];

/// Filters that open a file named in their arguments.
const FILE_FILTERS: &[&str] = &[
    "movie",
    "amovie",
    "subtitles",
    "ass",
    "sendcmd",
    "asendcmd",
    "zmq",
    "azmq",
];

/// Filter options that name a file.
const FILE_KEYS: &[&str] = &[
    "textfile", "fontfile", "filename", "file", "script", "cmd_file",
];

const PROTOCOLS: &[&str] = &[
    "http", "https", "ftp", "ftps", "sftp", "tcp", "udp", "tls", "rtp", "rtsp", "rtmp", "rtmps",
    "srt", "concat", "subfile", "data", "file", "pipe", "crypto", "async", "cache", "gopher",
    "hls", "mmsh", "mmst", "md5", "sctp", "unix", "tee", "ipfs", "ipns", "zmq", "lavfi",
];

/// Does `arg` contain `name=` as a whole word (so `bass=` is not `ass=`)?
fn has_key(arg: &str, name: &str) -> bool {
    let pat = format!("{name}=");
    arg.match_indices(&pat).any(|(i, _)| {
        !arg[..i]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn check_arg(arg: &str) -> Result<()> {
    if arg.len() > MAX_ARG_LEN || arg.contains('\0') {
        return Err(bad(format!(
            "each argument must be under {MAX_ARG_LEN} characters"
        )));
    }
    // ffmpeg treats any stray positional argument as an extra OUTPUT file, so a path in the
    // arguments could write outside the workspace. Relative names land in the scratch
    // directory (cwd), which is deleted afterwards; `tee` could smuggle paths past this check.
    if arg.starts_with('/') || arg.split('/').any(|c| c == "..") {
        return Err(bad(format!(
            "absolute paths and `..` are not allowed ({arg}); name files with {{inN}} and {{out}}"
        )));
    }
    let l = arg.to_ascii_lowercase();
    if l == "tee" {
        return Err(bad("the tee muxer is not allowed"));
    }
    if l.contains("://") {
        return Err(bad(format!("network URLs are not allowed: {arg}")));
    }
    if let Some((scheme, _)) = l.split_once(':')
        && PROTOCOLS.contains(&scheme)
    {
        return Err(bad(format!("protocol `{scheme}:` is not allowed in {arg}")));
    }
    if l.starts_with('-') {
        let name = l.split('=').next().unwrap_or(&l);
        if BANNED_OPTIONS
            .iter()
            .any(|b| name == *b || name.starts_with(&format!("{b}:")))
        {
            return Err(bad(format!("option {name} is not allowed")));
        }
        return Ok(());
    }
    for f in FILE_FILTERS {
        if has_key(&l, f) {
            return Err(bad(format!("filter `{f}` reads files and is not allowed")));
        }
    }
    for k in FILE_KEYS {
        if has_key(&l, k) {
            return Err(bad(format!(
                "filter option `{k}=` reads files and is not allowed"
            )));
        }
    }
    Ok(())
}

/// `{in3}` -> Some(3)
fn input_index(arg: &str) -> Option<usize> {
    arg.strip_prefix("{in")?.strip_suffix('}')?.parse().ok()
}

pub fn compile(sp: &Spec) -> Result<Compiled> {
    let n = sp.args.len();
    if n == 0 || n > MAX_ARGS {
        return Err(bad(format!("give between 1 and {MAX_ARGS} arguments")));
    }
    if sp.inputs.len() > MAX_INPUTS {
        return Err(bad(format!("at most {MAX_INPUTS} inputs")));
    }
    if sp.args[n - 1] != "{out}" {
        return Err(bad(
            "the last argument must be exactly {out}, the output file",
        ));
    }
    let mut a: Args = base_args();
    let mut used = vec![false; sp.inputs.len()];
    let mut i = 0;
    while i < n - 1 {
        let arg = &sp.args[i];
        if arg == "-i" {
            let idx = sp
                .args
                .get(i + 1)
                .and_then(|x| input_index(x))
                .ok_or_else(|| bad("-i must be followed by an input placeholder like {in0}"))?;
            let path = sp
                .inputs
                .get(idx)
                .ok_or_else(|| bad(format!("{{in{idx}}} refers to an input that was not given")))?;
            used[idx] = true;
            push_input(&mut a, path);
            i += 2;
            continue;
        }
        if arg.contains("{in") || arg.contains("{out}") {
            return Err(bad(format!(
                "placeholders are only allowed as `-i {{inN}}` and as the final {{out}}: {arg}"
            )));
        }
        check_arg(arg)?;
        a.push(arg.clone());
        i += 1;
    }
    if let Some(k) = used.iter().position(|u| !u) {
        return Err(bad(format!("input {k} is never used; add `-i {{in{k}}}`")));
    }
    if sp.inputs.is_empty() {
        return Err(bad(
            "give at least one input; lavfi and other generated sources are not supported (use generate_video)",
        ));
    }
    // Size cap always; duration cap unless the caller bounds the length themselves.
    // ponytail: a caller-supplied -t is not clamped to MAX_OUTPUT_SECS; the job timeout and
    // size cap still bound it.
    let bounded = sp.args.iter().any(|x| {
        matches!(x.as_str(), "-t" | "-to") || x.starts_with("-frames") || x.starts_with("-vframes")
    });
    if !bounded {
        a.extend(["-t".into(), MAX_OUTPUT_SECS.to_string()]);
    }
    a.extend(["-fs".into(), MAX_OUTPUT_BYTES.to_string(), s(sp.output)]);
    Ok(Compiled {
        args: a,
        text_files: vec![],
        out_secs: sp.expected_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn run(inputs: &[&str], args: &[&str]) -> Result<Compiled> {
        let inputs: Vec<PathBuf> = inputs.iter().map(PathBuf::from).collect();
        compile(&Spec {
            inputs: &inputs,
            args: &strs(args),
            expected_secs: 5.0,
            output: Path::new("/w/.t.tmp.mp4"),
        })
    }

    fn err(inputs: &[&str], args: &[&str]) -> String {
        run(inputs, args).unwrap_err().to_string()
    }

    #[test]
    fn builds_the_command_with_forced_flags() {
        let c = run(
            &["/w/a.mp4", "/w/b.png"],
            &[
                "-i",
                "{in0}",
                "-i",
                "{in1}",
                "-filter_complex",
                "[0:v][1:v]overlay=10:10",
                "{out}",
            ],
        )
        .unwrap();
        assert_eq!(c.out_secs, 5.0);
        let want = strs(&[
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
            "/w/a.mp4",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
            "/w/b.png",
            "-filter_complex",
            "[0:v][1:v]overlay=10:10",
            "-t",
            "3600",
            "-fs",
            &MAX_OUTPUT_BYTES.to_string(),
            "/w/.t.tmp.mp4",
        ]);
        assert_eq!(c.args, want);
        assert!(!c.args.contains(&"-y".to_string()));
    }

    #[test]
    fn caller_length_limit_is_respected() {
        let c = run(&["/w/a"], &["-i", "{in0}", "-t", "0.5", "{out}"]).unwrap();
        assert_eq!(c.args.iter().filter(|x| *x == "-t").count(), 1);
        let c = run(&["/w/a"], &["-i", "{in0}", "-frames:v", "1", "{out}"]).unwrap();
        assert!(!c.args.contains(&"-t".to_string()));
        assert!(err(&["/w/a"], &["-i", "{in0}", "-fs", "9", "{out}"]).contains("not allowed"));
    }

    #[test]
    fn stray_paths_cannot_become_outputs() {
        for p in ["/tmp/x.mp4", "../x.mp4", "a/../../x", "/etc/passwd"] {
            let e = err(&["/w/a"], &["-i", "{in0}", p, "{out}"]);
            assert!(e.contains("absolute paths"), "{p}: {e}");
        }
        assert!(err(&["/w/a"], &["-i", "{in0}", "-f", "tee", "{out}"]).contains("tee"));
        // relative names stay inside the scratch cwd; division and ranges are not paths
        run(
            &["/w/a"],
            &[
                "-i",
                "{in0}",
                "extra.png",
                "-vf",
                "scale=iw/2:ih/2",
                "{out}",
            ],
        )
        .unwrap();
    }

    #[test]
    fn placeholder_rules() {
        assert!(
            err(&["/w/a"], &["-i", "{in0}", "-vf", "x{in0}", "{out}"]).contains("placeholders")
        );
        assert!(err(&["/w/a"], &["-i", "{in0}", "{out}", "-vf", "x"]).contains("last argument"));
        assert!(err(&["/w/a"], &["-i", "/etc/passwd", "{out}"]).contains("placeholder"));
        assert!(err(&["/w/a"], &["-i", "{in3}", "{out}"]).contains("not given"));
        assert!(err(&["/w/a"], &["-i", "{in0}", "-i", "{out}", "{out}"]).contains("placeholder"));
        assert!(err(&["/w/a", "/w/b"], &["-i", "{in0}", "{out}"]).contains("never used"));
        assert!(err(&[], &["-vf", "x", "{out}"]).contains("at least one input"));
        assert!(err(&["/w/a"], &[]).contains("1 and"));
        assert!(
            err(&["/w/a"], &["-i", "{in0}", "-vf", "x{out}", "{out}"]).contains("placeholders")
        );
    }

    #[test]
    fn network_and_protocols_are_rejected() {
        for bad in [
            "http://x/y",
            "rtmp://h",
            "concat:a|b",
            "FILE:/etc/passwd",
            "data:text/plain,hi",
            "tcp:1.2.3.4",
        ] {
            let e = err(&["/w/a"], &["-i", "{in0}", "-metadata", bad, "{out}"]);
            assert!(e.contains("not allowed"), "{bad}: {e}");
        }
        // colons elsewhere are fine: stream specifiers, timestamps, ordinary filter args
        run(
            &["/w/a"],
            &[
                "-i",
                "{in0}",
                "-map",
                "0:v",
                "-ss",
                "00:00:01",
                "-vf",
                "scale=iw/2:ih/2,drawbox=x=1:y=2:w=3:h=4",
                "{out}",
            ],
        )
        .unwrap();
        run(
            &["/w/a"],
            &["-i", "{in0}", "-metadata", "title:x=y", "{out}"],
        )
        .unwrap();
    }

    #[test]
    fn banned_options_and_file_readers() {
        for opt in [
            "-y",
            "-n",
            "-filter_script:v",
            "-filter_complex_script",
            "-attach",
            "-dump_attachment:t",
            "-passlogfile",
            "-report",
            "-protocol_whitelist",
            "-headers",
        ] {
            assert!(
                err(&["/w/a"], &["-i", "{in0}", opt, "x", "{out}"]).contains("not allowed"),
                "{opt}"
            );
        }
        for f in [
            "movie=/etc/passwd",
            "amovie=x",
            "subtitles=/w/a.srt",
            "ass=a.ass",
            "[0:v]drawtext=textfile=/etc/passwd",
            "drawtext=text=hi:fontfile=/x.ttf",
            "sendcmd=f=cmds.txt",
            "lut3d=file=x.cube",
            "scale=1:1,MOVIE=x",
        ] {
            assert!(
                err(&["/w/a"], &["-i", "{in0}", "-vf", f, "{out}"]).contains("not allowed"),
                "{f}"
            );
        }
        // whole-word match: `bass` and `lowpass` are audio filters, not `ass`
        run(
            &["/w/a"],
            &["-i", "{in0}", "-af", "bass=g=5,lowpass=f=300", "{out}"],
        )
        .unwrap();
        run(
            &["/w/a"],
            &[
                "-i",
                "{in0}",
                "-vf",
                "drawtext=text=hello:fontsize=20",
                "{out}",
            ],
        )
        .unwrap();
    }

    #[test]
    fn size_limits() {
        let long = "a".repeat(MAX_ARG_LEN + 1);
        assert!(err(&["/w/a"], &["-i", "{in0}", "-vf", &long, "{out}"]).contains("characters"));
        let many: Vec<String> = (0..MAX_ARGS).map(|_| "-an".to_string()).collect();
        let mut v: Vec<&str> = many.iter().map(String::as_str).collect();
        v.push("{out}");
        assert!(err(&["/w/a"], &v).contains("1 and"));
    }

    #[test]
    fn format_extensions() {
        assert_eq!(OutputFormat::Jpg.ext(), "jpg");
        let f: OutputFormat = serde_json::from_value(serde_json::json!("webm")).unwrap();
        assert_eq!(f, OutputFormat::Webm);
        assert!(serde_json::from_value::<OutputFormat>(serde_json::json!("exe")).is_err());
    }
}
