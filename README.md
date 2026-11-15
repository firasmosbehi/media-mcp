# media-mcp

An MCP server (Rust, stdio) that lets an AI client edit and generate photos and videos by orchestrating ffmpeg and libvips.

**Status:** early development (phase 3 of [the plan](docs/PLAN.md): `probe`, `image_edit`, `preview` work). See [docs/PLAN.md](docs/PLAN.md) and [CLAUDE.md](CLAUDE.md).

## Tools

| Tool | Purpose |
|------|---------|
| `probe` | Inspect an image/video/audio file (ffprobe). Accepts `asset_id` or `import_path`; returns asset ID, path, kind, size, dimensions, duration, codecs. |
| `image_edit` | Edit a photo with an ordered list of operations (crop, resize, rotate, flip, color, blur, text, overlay) via libvips; exports JPEG/PNG/WebP as a new asset. |
| `preview` | Return one downscaled JPEG (max 1024 px) of an image so the model can see it. Images only for now. |

## Environment variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `MEDIA_MCP_WORKSPACE` | `./workspace` | Directory all assets live in |
| `FFMPEG_PATH`, `FFPROBE_PATH`, `VIPS_PATH` | found on `PATH` | Engine binaries |

## Development (Docker, no host toolchain)

```sh
docker build -t media-mcp-dev .
docker run --rm -v "$PWD":/app -v media-mcp-cargo:/usr/local/cargo/registry media-mcp-dev \
  sh -c "cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test"
```
