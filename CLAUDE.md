# media-mcp

An MCP server, written in Rust, that lets an AI client edit and generate photos and videos. It orchestrates external engines (ffmpeg, libvips, AI generation APIs); it does not do pixel or codec work itself.

## Scope and assumptions

- Runs locally over **stdio** as a single binary. Streamable HTTP is a later, optional transport; do not add auth, uploads or remote storage unless asked.
- `ffmpeg`, `ffprobe` and `vips` are external binaries found on `PATH` (overridable with `FFMPEG_PATH`, `FFPROBE_PATH`, `VIPS_PATH`). Never link libav or libvips directly.
- AI generation (text-to-image, text-to-video) goes through a provider trait. No provider is implemented until one is chosen; ffmpeg and libvips only cover procedural generation (text, shapes, gradients, slideshows).

## Stack

- Rust stable, edition 2024.
- `rmcp` (official MCP Rust SDK) with the `server`, `macros` and `transport-io` features.
- `tokio` (process, sync, time, fs), `serde` / `serde_json`, `schemars`, `thiserror`, `tracing`, `uuid`.
- The `rmcp` API changes between major versions. **Check docs.rs for the version pinned in `Cargo.toml` before writing or changing SDK code; do not rely on memory.** Do not bump the major version without asking.

## Commands

```sh
cargo build                          # debug build
cargo build --release                # release binary: target/release/media-mcp
cargo test                           # unit + integration tests
cargo test <name>                    # single test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
npx @modelcontextprotocol/inspector cargo run --quiet   # manual testing against the server
```

Before calling a task done: `cargo fmt`, `cargo clippy` with no warnings, and `cargo test` passing.

## Layout

```
src/
  main.rs          startup: config, engine detection, stdio transport
  server.rs        MCP server struct and tool router
  tools/           one file per tool: input/output types + handler
  engine/
    ffmpeg.rs      builds argument lists, runs ffmpeg, parses progress
    probe.rs       ffprobe wrapper -> typed MediaInfo
    vips.rs        libvips CLI wrapper for photo operations
    generate.rs    GenerationProvider trait + implementations
  jobs.rs          job queue, worker pool, status, cancellation
  assets.rs        asset registry: ID <-> file in the workspace
  sandbox.rs       path validation, limits
  error.rs         error types
tests/             integration tests using small generated fixtures
```

## Architecture rules

### Tools

- Keep the tool surface small (about a dozen). Target set: `probe`, `image_edit`, `image_compose`, `video_edit`, `video_compose`, `audio_edit`, `generate_image`, `generate_video`, `preview`, `job_status`, `job_cancel`, `run_ffmpeg`.
- Edit tools take an **ordered list of operations** (a tagged enum: crop, resize, rotate, trim, overlay, text, color, speed, ...) plus export settings, and compile them into one command. Do not add a tool per operation.
- Every input type derives `Deserialize` and `JsonSchema`, with a doc comment on each field: those comments are what the model reads. Use enums instead of free-form strings wherever the set of values is closed.
- Tool descriptions state what the tool does, when to use it over its neighbours, and what it returns.
- `run_ffmpeg` is the escape hatch: it takes an argument array, passes through the same sandbox, and is never the implementation path for a typed tool.

### Assets

- Tools accept and return **asset IDs**, not raw paths. `assets.rs` maps IDs to files inside the workspace directory (`MEDIA_MCP_WORKSPACE`, default `./workspace`).
- An `import` path is the only way a file outside the workspace enters it, by copy.
- Results return: asset ID, absolute path, media type, dimensions, duration, size. Never embed video or full-size images in a response. `preview` may return one downscaled JPEG (max 1024 px on the long edge) as image content.

### Jobs

- Anything that can exceed about 2 seconds runs as a job: the tool returns a job ID immediately, and `job_status` / `job_cancel` do the rest. This works in every MCP client; the protocol's Tasks extension is an optional later addition.
- Bounded worker pool (`MEDIA_MCP_MAX_JOBS`, default 2). Video encodes are already multi-threaded; do not oversubscribe.
- Progress comes from `-progress pipe:1 -nostats`: parse `out_time_us` against the probed duration. Also send an MCP progress notification when the request carries a progress token.
- Cancellation kills the child process (`kill_on_drop(true)` plus an explicit kill) and deletes partial output.
- Outputs are written to a temp name and renamed on success, so a failed job never leaves a half-written asset.

### Performance

- Prefer stream copy (`-c copy`) for trim, concat and remux when codecs and cut points allow; re-encode only when the operations require it.
- Detect hardware encoders once at startup (`ffmpeg -encoders`, then a tiny test encode) and cache the result; fall back to software silently.
- One ffmpeg invocation with a single `-filter_complex` graph per render. No chains of intermediate files.
- Photos go through libvips, not ffmpeg.
- Never read whole media files into memory; pass paths to the engines.

### Safety

- Spawn with `tokio::process::Command` and explicit argument lists. **Never** build a shell string or call `sh -c`.
- Every path is canonicalized and must resolve inside the workspace; reject symlink escapes and `..`.
- Always pass `-nostdin`, `-hide_banner`, and `-protocol_whitelist file,pipe` to ffmpeg. Reject network URLs as inputs.
- User text for `drawtext` and similar filters is written to a temp file (`textfile=`) or escaped with the dedicated escaping function in `engine/ffmpeg.rs`; never interpolated raw into a filtergraph.
- Enforce a per-job timeout and caps on output resolution, duration and file size (configurable, with conservative defaults).
- `-y` is only added when the tool input has `overwrite: true`.

## Rust conventions

- **stdout belongs to the MCP transport.** Never `println!` or `dbg!`. All logging goes through `tracing` to stderr.
- No `unwrap()` / `expect()` outside tests and startup invariants. Library code returns `Result<_, Error>` with `thiserror`.
- Engine failures become tool results with `is_error: true`, containing a short message plus the last ~20 lines of ffmpeg stderr, so the model can correct its own input. Protocol-level errors are reserved for malformed requests.
- No blocking calls on the async runtime: use `tokio::process` and `tokio::fs`, or `spawn_blocking`.
- No `unsafe`.
- Shared state lives in the server struct behind `Arc`; keep lock scopes short and never hold a lock across an `.await` on a child process.
- Add a dependency only when it removes real code; ask before adding anything that needs a C toolchain.

## Testing

- Unit-test command construction: given a list of operations, assert on the exact ffmpeg / vips argument vector. Most logic should be testable without running an engine.
- Unit-test sandbox path validation, filter escaping and progress parsing.
- Integration tests generate their own fixtures with ffmpeg's `lavfi` sources (`testsrc`, `sine`), 1 to 2 seconds long, at low resolution; do not commit binary media. They skip with a clear message if ffmpeg or vips is missing.
- Every new operation gets a command-construction test and one end-to-end test that probes the output.

## Workflow

- When adding an operation: extend the operation enum, add its compile step in the engine, add tests, update the tool description if behaviour visible to the model changed.
- When unsure about an ffmpeg filter or flag, run it against a `lavfi` fixture and inspect the output with ffprobe before committing to it.
- Keep `README.md` in sync with the tool list and environment variables.