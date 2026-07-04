//! The single persistent mTLS Session (PRD §4, §5.4).

#![allow(dead_code)]

use crate::config::Config;
use crate::enroll::Identity;
use anyhow::Result;

/// Hold the persistent `Session` stream, multiplexing metrics, docker/systemd
/// state, log tails, PTY, and command results by `stream_id`. Reconnect with
/// exponential backoff + jitter; re-send a `Hello` snapshot on reconnect so the
/// fleet view self-heals (PRD §2.5, §5.4).
pub async fn run(_cfg: &Config, _identity: Identity) -> Result<()> {
    todo!("agent session loop -- build slice #1, PRD §5.4")
}
