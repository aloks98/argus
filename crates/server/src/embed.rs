//! Must be built before compiling the server, or `frontend/dist` won't exist (PRD §10):
//!   pnpm --dir frontend install --frozen-lockfile && pnpm --dir frontend run build

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../frontend/dist"]
pub struct Assets;
