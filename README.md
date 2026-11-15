# media-mcp

An MCP server (Rust, stdio) that lets an AI client edit and generate photos and videos by orchestrating ffmpeg and libvips.

**Status:** early development (phase 2 of [the plan](docs/PLAN.md): `probe` works). See [docs/PLAN.md](docs/PLAN.md) and [CLAUDE.md](CLAUDE.md).

## Tools

| Tool | Purpose |
|------|---------|
| `probe` | Inspect an image/video/audio file (ffprobe). Accepts `asset_id` or `import_path`; returns asset ID, path, kind, size, dimensions, duration, codecs. |

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
