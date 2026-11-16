//! Generated media. Two kinds: procedural fills (solid colour, gradient), rendered by libvips,
//! and AI generation behind `GenerationProvider`.
//! ponytail: no provider is implemented (none chosen for v1); add one by implementing the trait
//! and registering it in the server, then expose `prompt` on the generate tools.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::Result;

/// What a generated image or video shows.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fill {
    /// One flat colour.
    Solid {
        /// Colour as `#RRGGBB`.
        color: String,
    },
    /// A linear blend between two colours.
    Gradient {
        /// Colour at the left (horizontal) or top (vertical) edge, `#RRGGBB`.
        from: String,
        /// Colour at the right (horizontal) or bottom (vertical) edge, `#RRGGBB`.
        to: String,
        /// `horizontal` (default, left to right) or `vertical` (top to bottom).
        #[serde(default)]
        direction: GradientDirection,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GradientDirection {
    #[default]
    Horizontal,
    Vertical,
}

pub struct ImageRequest {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    /// The provider writes the image here.
    pub output: PathBuf,
}

pub struct VideoRequest {
    pub prompt: String,
    pub secs: f64,
    pub width: u32,
    pub height: u32,
    /// The provider writes the video here.
    pub output: PathBuf,
}

/// A text-to-image / text-to-video backend. Implementations write the result to the request's
/// `output` path; the tool layer registers it as an asset like any other output.
pub trait GenerationProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn text_to_image(&self, req: ImageRequest) -> impl Future<Output = Result<()>> + Send;
    fn text_to_video(&self, req: VideoRequest) -> impl Future<Output = Result<()>> + Send;
}
