# media-mcp

An MCP server (Rust, stdio) that lets an AI client edit and generate photos and videos by orchestrating ffmpeg and libvips.

**Status:** early development (phase 6 of [the plan](docs/PLAN.md): photo tools, jobs, `video_edit`, `audio_edit`, `video_compose`, `image_compose` and procedural `generate_image`/`generate_video` work; hardware encoders and `run_ffmpeg` are next). See [docs/PLAN.md](docs/PLAN.md) and [CLAUDE.md](CLAUDE.md).

## Tools

| Tool | Purpose |
|------|---------|
| `probe` | Inspect an image/video/audio file (ffprobe). Accepts `asset_id` or `import_path`; returns asset ID, path, kind, size, dimensions, duration, codecs. |
| `image_edit` | Edit a photo with an ordered list of operations (crop, resize, rotate, flip, color, blur, text, overlay) via libvips; exports JPEG/PNG/WebP as a new asset. Runs as a job. |
| `preview` | Return one downscaled JPEG (max 1024 px) of an image, or of a video frame (`at_secs`, default the middle), so the model can see it. |
| `video_edit` | Edit a video with an ordered list of operations (trim, crop, resize, rotate, flip, speed, color, text, overlay) compiled into one ffmpeg render; audio follows trim/speed. Exports mp4 (H.264+AAC) or webm (VP9+Opus); `stream_copy` gives an instant lossless cut for a lone trim. Runs as a job with progress. |
| `audio_edit` | Edit audio, or extract the audio of a video, with an ordered list of operations (trim, volume, fade_in, fade_out, speed); exports mp3, wav or m4a. Runs as a job. |
| `video_compose` | Join videos and still images into one video (concatenation or photo slideshow): per-clip `secs`, output size/fps with black-bar padding, audio kept in sync. `stream_copy` joins matching videos instantly without re-encoding. Exports mp4 or webm. Runs as a job with progress. |
| `image_compose` | Arrange photos into a grid / contact sheet (columns, cell size, gap, background); each photo is fitted and centred in its cell. Exports png/jpeg/webp. Runs as a job. |
| `generate_image` | Create an image from nothing: a solid colour or linear gradient of a given size. Procedural only (no AI provider yet). Runs as a job. |
| `generate_video` | Create a silent solid-colour or gradient video of a given size, length and fps (mp4/webm). Procedural only. Runs as a job. |
| `job_status` | State, progress and result/error of a job. `wait_secs` (max 30) blocks until it finishes; sends MCP progress notifications if the request has a progress token. |
| `job_cancel` | Cancel a queued or running job: kills the process, deletes partial output. |

Anything that can take more than a couple of seconds runs as a job: the tool returns a `job_id` at once, and the result arrives via `job_status`.

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEDIA_MCP_WORKSPACE` | `./workspace` | Directory all assets live in |
| `MEDIA_MCP_MAX_JOBS` | `2` | Jobs running at once; the rest wait in the queue |
| `MEDIA_MCP_JOB_TIMEOUT_SECS` | `600` | A job running longer than this is killed and marked failed |
| `FFMPEG_PATH`, `FFPROBE_PATH`, `VIPS_PATH` | found on `PATH` | Engine binaries |

## Development (Docker, no host toolchain)

```sh
docker build -t media-mcp-dev .
docker run --rm -v "$PWD":/app -v media-mcp-cargo:/usr/local/cargo/registry media-mcp-dev \
  sh -c "cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test"
```
