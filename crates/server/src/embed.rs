//! The built React frontend, embedded into the binary via `rust-embed` (PRD §10).
//!
//! Build the frontend before compiling the server so `frontend/dist` exists:
//!   npm --prefix frontend ci && npm --prefix frontend run build

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../frontend/dist"]
pub struct Assets;
