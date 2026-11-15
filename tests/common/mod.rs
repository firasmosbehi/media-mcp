//! Shared harness: spawn the server binary and talk MCP over stdio. Fixtures come from ffmpeg lavfi.
#![allow(dead_code)]
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok()
}

pub struct Server {
    child: Child,
    w: ChildStdin,
    r: BufReader<ChildStdout>,
    next_id: u64,
    /// Scratch dir for fixtures; the workspace is `dir/ws`.
    pub dir: PathBuf,
}

impl Server {
    /// None (with a message) when ffmpeg or vips is missing, so tests skip instead of failing.
    pub fn start() -> Option<Server> {
        if !(have("ffmpeg") && have("vips")) {
            eprintln!("skipping: ffmpeg or vips not installed");
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "media-mcp-it-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_media-mcp"))
            .env("MEDIA_MCP_WORKSPACE", dir.join("ws"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let w = child.stdin.take().unwrap();
        let r = BufReader::new(child.stdout.take().unwrap());
        let mut s = Server {
            child,
            w,
            r,
            next_id: 1,
            dir,
        };
        s.request(
            "initialize",
            json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}),
        );
        writeln!(
            s.w,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        Some(s)
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.join("ws")
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.w,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// The `result` object of a tools/call.
    pub fn tool(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name":name,"arguments":args}))["result"].clone()
    }

    /// Like `tool`, asserting success and returning structuredContent.
    pub fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.tool(name, args);
        assert_ne!(r["isError"], true, "tool failed: {r}");
        r["structuredContent"].clone()
    }

    /// Generate a fixture file in the scratch dir with ffmpeg and return its path.
    pub fn fixture(&self, name: &str, ffmpeg_args: &[&str]) -> PathBuf {
        let out = self.dir.join(name);
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y"])
            .args(ffmpeg_args)
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "fixture generation failed: {name}");
        out
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Decode an image to packed RGB24 bytes with ffmpeg.
pub fn rgb_pixels(path: &Path) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    assert!(out.status.success());
    out.stdout
}

pub fn pixel(path: &Path, width: usize, x: usize, y: usize) -> [u8; 3] {
    let px = rgb_pixels(path);
    let i = (y * width + x) * 3;
    [px[i], px[i + 1], px[i + 2]]
}
