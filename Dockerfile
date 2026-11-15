# Dev/CI image: Rust toolchain plus the external engines the server shells out to.
FROM rust:1-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg libvips-tools \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add clippy rustfmt
WORKDIR /app
