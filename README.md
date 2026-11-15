# media-mcp

An MCP server (Rust, stdio) that lets an AI client edit and generate photos and videos by orchestrating ffmpeg and libvips.

**Status:** planning. See [docs/PLAN.md](docs/PLAN.md) and [CLAUDE.md](CLAUDE.md).

## Development (Docker, no host toolchain)

```sh
docker build -t media-mcp-dev .
docker run --rm -v "$PWD":/app -v media-mcp-cargo:/usr/local/cargo/registry media-mcp-dev \
  sh -c "cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test"
```
