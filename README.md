# media-mcp

An MCP server (Rust, stdio) that lets an AI client edit and generate photos and videos by orchestrating ffmpeg and libvips.

**Status:** early development (phase 4 of [the plan](docs/PLAN.md): photo tools and the job system work). See [docs/PLAN.md](docs/PLAN.md) and [CLAUDE.md](CLAUDE.md).

## Tools

| Tool | Purpose |
|------|---------|
| `probe` | Inspect an image/video/audio file (ffprobe). Accepts `asset_id` or `import_path`; returns asset ID, path, kind, size, dimensions, duration, codecs. |
| `image_edit` | Edit a photo with an ordered list of operations (crop, resize, rotate, flip, color, blur, text, overlay) via libvips; exports JPEG/PNG/WebP as a new asset. Runs as a job. |
| `preview` | Return one downscaled JPEG (max 1024 px) of an image so the model can see it. Images only for now. |
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
