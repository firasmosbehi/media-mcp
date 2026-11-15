# media-mcp: implementation plan

Written 2026-11-15. Architecture rules live in [CLAUDE.md](../CLAUDE.md); this file is the build order.

## Research summary

- **SDK:** official `rmcp` (modelcontextprotocol/rust-sdk), stable 3.x on crates.io. `#[tool_router(server_handler)]` + `#[tool]` + `Parameters<T>` give typed tools with schemars schemas. `CallToolResult::error` for engine failures, `Err(ErrorData)` only for malformed requests. Re-check docs.rs for the exact pinned version before each SDK change.
- **Prior art** (all Python/TS, ffmpeg wrappers, mostly one tool per operation): kyanitelabs/mcp-video (87 tools), video-creator/ffmpeg-mcp, misbahsy/video-audio-mcp, conneroisu/vfx-mcp, PedroMarianoAlmeida/ffmpeg-mcp (workspace sandbox), yubraaj11/ffmpeg-mcp.
- **Our differentiators:** ~12 tools with an ordered-operations model compiled to one ffmpeg/vips invocation; asset IDs instead of paths; async jobs with progress and cancel; strict sandbox; photos via libvips; single static-ish Rust binary.

## Decisions

- Public repo `firasmosbehi/media-mcp`, MIT.
- No AI generation provider in v1: `GenerationProvider` trait only; `generate_image`/`generate_video` return procedural output (text, gradients, shapes, slideshows) until a provider is chosen.
- Dev and CI run in Docker (rust + ffmpeg + libvips-tools); no host toolchain needed. Release ships a Docker image plus binaries.

## Phases (each ends with fmt + clippy + tests green)

0. **Scaffold.** `cargo init`, deps, `Dockerfile` (dev/test image), GitHub Actions CI (fmt, clippy, test in that image), `main.rs` serving an empty tool router over stdio, tracing to stderr.
1. **Core plumbing.** `error.rs`, `sandbox.rs` (canonicalize, symlink/`..` rejection, caps), `assets.rs` (ID <-> file, `import` by copy), engine detection from env/PATH.
2. **Probe.** `engine/probe.rs` -> typed `MediaInfo`; `probe` tool. First end-to-end test with a lavfi fixture.
3. **Photos.** `engine/vips.rs`; `image_edit` with op enum (crop, resize, rotate, color, text, overlay) + export settings; `preview`.
4. **Jobs.** `jobs.rs` worker pool, temp-name-then-rename outputs, cancel (kill + cleanup), per-job timeout; `job_status`, `job_cancel`; progress parsing from `-progress pipe:1`.
5. **Video/audio.** `engine/ffmpeg.rs` op compiler (single `-filter_complex`, stream-copy fast path, hw-encoder detection, drawtext via `textfile=`); `video_edit` (trim, crop, resize, rotate, speed, text, overlay, color), `audio_edit`, `video_compose` (concat, slideshow, overlay).
6. **Compose + generate.** `image_compose`, procedural `generate_image`/`generate_video`, provider trait stub.
7. **Escape hatch.** `run_ffmpeg` through the same sandbox.
8. **Release.** README tool/env tables, Docker image, tagged binaries, Inspector smoke test, client config examples (Claude Desktop/Code).

## Risks

- libvips CLI has no uniform op-chaining: photo ops may need a small pipeline of vips calls, or a single `vips` composite where possible (revisit in phase 3; CLAUDE.md forbids linking libvips).
- Docker + stdio MCP clients: workspace must be a bind mount; document `docker run -i --rm -v ...`.
- Hardware encoder detection does not work inside Docker on macOS; software fallback is the tested path.
