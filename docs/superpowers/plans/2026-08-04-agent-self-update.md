# Agent Self-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Push the control plane's bundled agent binary to a machine over the existing mTLS Session in chunks; the agent verifies sha256, swaps its binary (keeping `.old`), and re-execs in place.

**Architecture:** Additive proto (`UpdateAgent` gains `total_bytes`/`command_id`/`issued_by`; new `UpdateChunk` ServerFrame arm). The server loads an optional bundled binary (`ARGUS_AGENT_BINARY`), and a verb-style endpoint dispatches the trigger frame plus 256 KiB chunks through the Hub's existing bounded channel, reusing `register_pending`/`CommandResult`/audit correlation unchanged. The agent stages into a temp file, verifies with `ring`, renames current → `.old`, temp → current, replies, and `exec()`s itself (pid preserved). UI: warn badge + confirm-guarded button on the machine page, quiet `agent outdated` text on fleet rows, driven by a new `GET /api/server-info`.

**Tech Stack:** Rust (tonic/prost via protox, axum, ring), React/TS (TanStack Query, rnui).

**Spec:** `docs/superpowers/specs/2026-08-04-agent-self-update-design.md` — read it first.

## Global Constraints

- Branch: `feat/agent-self-update` (exists; spec committed on it).
- Conventional commits; every commit ends with the two trailer lines used by prior commits on this branch (`git log -1` shows the format).
- **NEVER run `cargo test --workspace -- --ignored`** — rotates the dev CA, orphans the live agent. Plain `cargo test` is safe.
- Proto changes are ADDITIVE ONLY, exact field numbers as written in Task 1. Proto is the source of truth (`crates/proto/proto/argus.proto`, protox codegen — no protoc).
- If any task adds a new `sqlx::query!` string (tests included): `DATABASE_URL='postgres://postgres:argus@localhost:5432/argus' cargo sqlx prepare --workspace -- --all-targets`, commit `.sqlx/`, verify with the `--check` variant. (No new queries are expected in this plan.)
- Constants, verbatim: chunk size `256 * 1024` bytes; implausibility cap `64 * 1024 * 1024` bytes; endpoint bounded wait `Duration::from_secs(60)`; audit action `"agent.update"`; `.old` filename = `argus-agent.old` in the binary's directory (derived from the exe filename + `.old`, see Task 1).
- Frontend gates: `pnpm --dir frontend run typecheck && pnpm --dir frontend run lint && pnpm --dir frontend run fmt:check && pnpm --dir frontend run build` (pnpm + oxlint + oxfmt, never npm/eslint/prettier).
- Agent stays lean: the only new agent dependency allowed is `ring` (workspace-pinned; already in tree transitively).

---

### Task 1: Proto additions + agent `update.rs` staging module

**Files:**
- Modify: `crates/proto/proto/argus.proto` (UpdateAgent fields, ServerFrame arm)
- Create: `crates/agent/src/update.rs` (staging logic + unit tests)
- Modify: `crates/agent/src/main.rs` (add `mod update;` — match the existing mod list)
- Modify: `crates/agent/Cargo.toml` (add `ring.workspace = true`)
- Modify: `crates/agent/src/session.rs` (two new match arms wiring the module)

**Interfaces:**
- Consumes: existing proto types; `agent_frame::Payload::CommandResult` reply path via `inbound_tx` (see session.rs's Command arm).
- Produces (Task 2 relies on the proto shapes; the session wiring is final here):

```proto
// in message UpdateAgent (existing fields 1-3 unchanged):
  uint64 total_bytes = 4;
  string command_id = 5;   // UpdateAgent is a bare ServerFrame arm (no Command
                           // envelope), so it carries its own correlation id
  string issued_by = 6;    // and actor, mirroring Command, for the agent-side trail
// in ServerFrame oneof payload (after `PtyFlow pty_flow = 12;`):
    UpdateChunk update_chunk = 13;      // binary payload following an UpdateAgent
// new message, next to UpdateAgent:
message UpdateChunk {
  bytes data = 1;
  bool last = 2;
}
```

```rust
// crates/agent/src/update.rs
pub struct Updater { /* exe: PathBuf, pending: Option<Pending> */ }
pub struct Staged { pub version: String, pub command_id: String, pub exe: PathBuf }
impl Updater {
    /// `exe` = the CURRENT binary path (resolve /proc/self/exe ONCE at startup).
    pub fn new(exe: std::path::PathBuf) -> Self;
    /// Refusals are Err(String) — the caller turns them into CommandResult{ok:false}.
    pub fn begin(&mut self, version: &str, sha256_hex: &str, total_bytes: u64, command_id: &str) -> Result<(), String>;
    /// Ok(None) = more chunks expected; Ok(Some(staged)) = binary swapped, caller
    /// replies then re-execs. Err = refused; internal state cleared, temp deleted.
    pub fn chunk(&mut self, data: &[u8], last: bool) -> Result<Option<Staged>, String>;
}
/// exec()s `exe` with the process's ORIGINAL argv (std::env::args_os) — never returns.
pub fn reexec(exe: &std::path::Path) -> std::io::Error; // returns only on failure
```

- [ ] **Step 1: Proto changes**

Apply the proto block above verbatim: three fields appended to `message UpdateAgent` (update the `url` comment to `// superseded: unset — the binary streams as UpdateChunk frames on this stream`), the `UpdateChunk update_chunk = 13;` oneof arm, and the new `UpdateChunk` message beside `UpdateAgent` in the `---- Self-update ----` section.

- [ ] **Step 2: Verify codegen compiles**

Run: `cargo check -p argus-proto`
Expected: clean (protox regenerates; no protoc involved).

- [ ] **Step 3: Write the failing unit tests**

Create `crates/agent/src/update.rs` containing ONLY the tests first (module skeleton + `#[cfg(test)] mod tests`). Tests drive a fake "current binary" in a tempdir (the agent has no tempfile dep — build paths under `std::env::temp_dir()` with a `Uuid`-free unique suffix via `std::process::id()` + a counter, and clean up at test end):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch dir per test; contains a fake "current" agent binary.
    fn scratch() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "argus-update-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("argus-agent");
        fs::write(&exe, b"OLD-BINARY").unwrap();
        (dir, exe)
    }

    fn hex_sha256(data: &[u8]) -> String {
        let d = ring::digest::digest(&ring::digest::SHA256, data);
        d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn stages_a_valid_update_and_keeps_old() {
        let (dir, exe) = scratch();
        let new_bin = b"NEW-BINARY-CONTENTS".to_vec();
        let mut u = Updater::new(exe.clone());
        u.begin("9.9.9", &hex_sha256(&new_bin), new_bin.len() as u64, "cmd-1")
            .unwrap();
        let staged = u.chunk(&new_bin, true).unwrap().expect("staged");
        assert_eq!(staged.version, "9.9.9");
        assert_eq!(staged.command_id, "cmd-1");
        assert_eq!(staged.exe, exe);
        // New binary in place, old preserved beside it, temp gone.
        assert_eq!(fs::read(&exe).unwrap(), new_bin);
        assert_eq!(fs::read(dir.join("argus-agent.old")).unwrap(), b"OLD-BINARY");
        assert!(!dir.join(".argus-agent.update").exists());
        // Executable bit set.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn multi_chunk_assembly() {
        let (dir, exe) = scratch();
        let new_bin: Vec<u8> = (0..=255u8).cycle().take(700_000).collect();
        let mut u = Updater::new(exe.clone());
        u.begin("9.9.9", &hex_sha256(&new_bin), new_bin.len() as u64, "cmd-2")
            .unwrap();
        assert!(u.chunk(&new_bin[..256 * 1024], false).unwrap().is_none());
        assert!(u.chunk(&new_bin[256 * 1024..512 * 1024], false).unwrap().is_none());
        let staged = u.chunk(&new_bin[512 * 1024..], true).unwrap().expect("staged");
        assert_eq!(staged.version, "9.9.9");
        assert_eq!(fs::read(&exe).unwrap(), new_bin);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sha_mismatch_refuses_and_leaves_current_untouched() {
        let (dir, exe) = scratch();
        let new_bin = b"NEW-BINARY".to_vec();
        let mut u = Updater::new(exe.clone());
        u.begin("9.9.9", &hex_sha256(b"different bytes"), new_bin.len() as u64, "c")
            .unwrap();
        let err = u.chunk(&new_bin, true).unwrap_err();
        assert!(err.contains("sha256"), "got: {err}");
        assert_eq!(fs::read(&exe).unwrap(), b"OLD-BINARY");
        assert!(!dir.join(".argus-agent.update").exists());
        assert!(!dir.join("argus-agent.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn size_overrun_and_underrun_refuse() {
        let (dir, exe) = scratch();
        let mut u = Updater::new(exe.clone());
        u.begin("9", &hex_sha256(b"xx"), 2, "c").unwrap();
        assert!(u.chunk(b"xxx", true).unwrap_err().contains("size"));
        // Fresh begin after a refusal must work (state cleared).
        u.begin("9", &hex_sha256(b"xx"), 2, "c").unwrap();
        assert!(u.chunk(b"x", true).unwrap_err().contains("size"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_concurrent_and_implausible() {
        let (dir, exe) = scratch();
        let mut u = Updater::new(exe.clone());
        assert!(u.begin("9", "00", 0, "c").is_err()); // zero bytes
        assert!(u.begin("9", "00", 65 * 1024 * 1024, "c").is_err()); // > 64 MiB cap
        u.begin("9", "00", 10, "c").unwrap();
        assert!(u.begin("9", "00", 10, "c").unwrap_err().contains("in flight"));
        fs::remove_dir_all(dir).unwrap();
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p argus-agent update::`
Expected: compile error (`Updater` undefined) — the expected RED signal.

- [ ] **Step 5: Implement the module**

Above the tests in `update.rs` (add `ring.workspace = true` to `crates/agent/Cargo.toml` `[dependencies]`, and `mod update;` to `main.rs`):

```rust
// Self-update staging: receive the new binary as UpdateChunk frames, verify,
// swap, and re-exec. Everything here is synchronous std::fs on purpose --
// chunks are 256 KiB writes to local disk, far below the threshold where
// blocking the inbound loop matters, and it keeps the module dependency-free.
//
// Every failure path leaves the CURRENT binary untouched and running; the
// worst possible outcome of a refused update is a deleted temp file.
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Spec cap: anything larger than this is implausible for the agent binary.
const MAX_UPDATE_BYTES: u64 = 64 * 1024 * 1024;

pub struct Staged {
    pub version: String,
    pub command_id: String,
    pub exe: PathBuf,
}

struct Pending {
    version: String,
    sha256_hex: String,
    total_bytes: u64,
    command_id: String,
    file: File,
    temp: PathBuf,
    received: u64,
    hasher: ring::digest::Context,
}

pub struct Updater {
    exe: PathBuf,
    pending: Option<Pending>,
}

impl Updater {
    /// `exe` = the current binary's path, resolved ONCE (via /proc/self/exe)
    /// before any update can rename things out from under the symlink.
    pub fn new(exe: PathBuf) -> Self {
        Updater { exe, pending: None }
    }

    pub fn begin(
        &mut self,
        version: &str,
        sha256_hex: &str,
        total_bytes: u64,
        command_id: &str,
    ) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("an update is already in flight".into());
        }
        if total_bytes == 0 || total_bytes > MAX_UPDATE_BYTES {
            return Err(format!("implausible total_bytes {total_bytes}"));
        }
        // Same directory as the binary: the final rename must be atomic,
        // which requires the same filesystem.
        let temp = self.temp_path();
        let file = File::create(&temp).map_err(|e| format!("create {}: {e}", temp.display()))?;
        self.pending = Some(Pending {
            version: version.to_string(),
            sha256_hex: sha256_hex.to_lowercase(),
            total_bytes,
            command_id: command_id.to_string(),
            file,
            temp,
            received: 0,
            hasher: ring::digest::Context::new(&ring::digest::SHA256),
        });
        Ok(())
    }

    pub fn chunk(&mut self, data: &[u8], last: bool) -> Result<Option<Staged>, String> {
        let Some(p) = self.pending.as_mut() else {
            return Err("chunk without an announced update".into());
        };
        p.received += data.len() as u64;
        if p.received > p.total_bytes {
            let msg = format!("size overrun: got {} of {}", p.received, p.total_bytes);
            self.abort();
            return Err(msg);
        }
        if let Err(e) = p.file.write_all(data) {
            let msg = format!("write: {e}");
            self.abort();
            return Err(msg);
        }
        p.hasher.update(data);
        if !last {
            return Ok(None);
        }
        if p.received != p.total_bytes {
            let msg = format!("size underrun: got {} of {}", p.received, p.total_bytes);
            self.abort();
            return Err(msg);
        }
        let digest = p.hasher.clone().finish();
        let got: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if got != p.sha256_hex {
            let msg = format!("sha256 mismatch: got {got}, announced {}", p.sha256_hex);
            self.abort();
            return Err(msg);
        }
        if let Err(e) = p.file.sync_all() {
            let msg = format!("fsync: {e}");
            self.abort();
            return Err(msg);
        }
        // Verified. Swap: chmod temp, current -> .old, temp -> current.
        let p = self.pending.take().expect("checked above");
        let old = self.old_path();
        let swap = (|| -> std::io::Result<()> {
            fs::set_permissions(&p.temp, fs::Permissions::from_mode(0o755))?;
            fs::rename(&self.exe, &old)?;
            fs::rename(&p.temp, &self.exe)?;
            Ok(())
        })();
        if let Err(e) = swap {
            // Best effort to restore: if the current binary was already moved
            // to .old but the temp rename failed, move it back.
            if !self.exe.exists() && old.exists() {
                let _ = fs::rename(&old, &self.exe);
            }
            let _ = fs::remove_file(&p.temp);
            return Err(format!("swap: {e}"));
        }
        Ok(Some(Staged {
            version: p.version,
            command_id: p.command_id,
            exe: self.exe.clone(),
        }))
    }

    fn abort(&mut self) {
        if let Some(p) = self.pending.take() {
            let _ = fs::remove_file(&p.temp);
        }
    }

    fn temp_path(&self) -> PathBuf {
        self.exe.with_file_name(format!(
            ".{}.update",
            self.exe.file_name().unwrap_or_default().to_string_lossy()
        ))
    }

    fn old_path(&self) -> PathBuf {
        self.exe.with_file_name(format!(
            "{}.old",
            self.exe.file_name().unwrap_or_default().to_string_lossy()
        ))
    }
}

/// Replace this process with `exe`, preserving the original argv and env --
/// pid is preserved, so a supervising systemd unit never notices. Returns
/// only if exec itself failed.
pub fn reexec(exe: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    std::process::Command::new(exe).args(args).exec()
}
```

Note `ring::digest::Context` is `Clone`; the `p.hasher.clone().finish()` avoids moving out of the borrow. If clippy objects to the closure-swap idiom, an inner `fn` is fine — behavior over form.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p argus-agent update::`
Expected: 5/5 PASS.

- [ ] **Step 7: Wire the session loop**

In `crates/agent/src/session.rs`, following the existing structure (see the `Command` arm around line 286):

1. Near session start (where `info::gather` and the samplers are set up), resolve the exe once and create the updater — it is used only by the inbound loop, so it lives as a plain `let mut updater` in that scope:

```rust
// Resolved ONCE per session, before any update can swap the binary out
// from under /proc/self/exe.
let mut updater = match std::env::current_exe() {
    Ok(exe) => Some(crate::update::Updater::new(exe)),
    Err(e) => {
        tracing::warn!(error = %e, "update: cannot resolve own binary; self-update disabled");
        None
    }
};
```

2. Two new match arms in the inbound loop. Handled INLINE (not spawned): chunk ordering matters and writes are fast; a helper keeps the arms thin. Add this helper above the loop (same file):

```rust
/// Turn an update step's Err into a CommandResult refusal frame.
fn update_refusal(command_id: &str, msg: &str) -> CommandResult {
    CommandResult {
        command_id: command_id.to_string(),
        ok: false,
        exit_code: 1,
        message: msg.to_string(),
    }
}
```

The arms:

```rust
Some(server_frame::Payload::Update(u)) => {
    tracing::info!(version = %u.version, issued_by = %u.issued_by, "update: announced");
    // Remember the correlation id for the chunks that follow.
    last_update_command_id = u.command_id.clone();
    let refusal = match updater.as_mut() {
        None => Some("self-update disabled: own path unresolved".to_string()),
        Some(up) => up
            .begin(&u.version, &u.sha256, u.total_bytes, &u.command_id)
            .err(),
    };
    if let Some(msg) = refusal {
        tracing::warn!(%msg, "update: refused");
        let _ = inbound_tx
            .send(AgentFrame {
                stream_id: frame.stream_id,
                payload: Some(agent_frame::Payload::CommandResult(update_refusal(
                    &u.command_id,
                    &msg,
                ))),
            })
            .await;
    }
}
Some(server_frame::Payload::UpdateChunk(c)) => {
    if let Some(up) = updater.as_mut() {
        match up.chunk(&c.data, c.last) {
            Ok(None) => {}
            Ok(Some(staged)) => {
                tracing::info!(version = %staged.version, "update: staged; re-exec");
                let _ = inbound_tx
                    .send(AgentFrame {
                        stream_id: frame.stream_id,
                        payload: Some(agent_frame::Payload::CommandResult(CommandResult {
                            command_id: staged.command_id.clone(),
                            ok: true,
                            exit_code: 0,
                            message: format!("staged {}", staged.version),
                        })),
                    })
                    .await;
                // Give the outbound task a moment to flush the result
                // before this process image is replaced.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let err = crate::update::reexec(&staged.exe);
                // Only reachable if exec itself failed. The staged (hash-
                // verified) binary is already in place on disk and .old is
                // beside it -- the next restart runs the new version. This
                // process keeps running the old image; just record it.
                tracing::error!(error = %err, "update: exec failed; staged binary will take effect on next restart");
            }
            Err(msg) => {
                tracing::warn!(%msg, "update: refused mid-stream");
                let _ = inbound_tx
                    .send(AgentFrame {
                        stream_id: frame.stream_id,
                        payload: Some(agent_frame::Payload::CommandResult(update_refusal(
                            &last_update_command_id,
                            &msg,
                        ))),
                    })
                    .await;
            }
        }
    }
}
```

Declare `let mut last_update_command_id = String::new();` beside `updater`. (Chunks don't carry the id — the stream is ordered and one update runs at a time, so the remembered id from the announce is the right one for mid-stream refusals.)

- [ ] **Step 8: Full agent gates**

Run: `cargo fmt --all && cargo clippy -p argus-agent --all-targets -- -D warnings && cargo test -p argus-agent`
Expected: clean; all agent tests pass (live_* remain ignored).

- [ ] **Step 9: Commit**

```bash
git add crates/proto/proto/argus.proto crates/agent
git commit -m "feat(update): proto additions + agent-side staging, verify, swap, re-exec"
```

---

### Task 2: Server — bundled binary, dispatch, endpoint, server-info

**Files:**
- Create: `crates/server/src/agent_binary.rs`
- Modify: `crates/server/src/main.rs` (add `mod agent_binary;`, load at boot into AppState)
- Modify: `crates/server/src/config.rs` (optional `agent_binary_path`)
- Modify: `crates/server/src/hub.rs` (two send fns)
- Modify: `crates/server/src/http.rs` (AppState field, two routes, handlers, tests)

**Interfaces:**
- Consumes: Task 1's proto (`UpdateAgent` fields 4-6, `server_frame::Payload::UpdateChunk`); existing `hub.register_pending` / `abandon_pending` / `repo::audit_command` / `repo::update_command_result` / `VerbResult` response struct (all in http.rs's `run_verb`, ~line 380 — mirror it).
- Produces (Tasks 3-4 rely on):
  - `AppState.agent_binary: Option<Arc<agent_binary::AgentBinary>>` where `pub struct AgentBinary { pub bytes: Vec<u8>, pub sha256_hex: String, pub version: &'static str, pub total_bytes: u64 }`
  - `POST /api/machines/{id}/agent-update` → same `VerbResult` JSON shape as the verb endpoints (200 completed / 202 pending / 409 offline / 409 arch / 503 unbundled)
  - `GET /api/server-info` → `{"version": "<semver>", "agent_update": {"version": "<semver>", "sha256": "<hex>"} | null}`
  - Env: `ARGUS_AGENT_BINARY` (optional path)

- [ ] **Step 1: Write the failing tests**

In `crates/server/src/http.rs` `mod tests` (style-match the neighbors; `auth_cookie`/`test_state` exist). `test_state` builds an AppState — it will gain the `agent_binary: None` field in Step 3; a second helper wraps a state with a fake binary:

```rust
#[sqlx::test]
async fn server_info_reports_bundle_state(pool: PgPool) -> anyhow::Result<()> {
    let cookie = auth_cookie(&pool).await?;
    // Without a bundle: agent_update is null.
    let app = router(test_state(pool.clone()));
    let res = app
        .oneshot(Request::get("/api/server-info").header("cookie", &cookie).body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["agent_update"].is_null());

    // With a bundle: version + sha surface.
    let mut state = test_state(pool);
    state.agent_binary = Some(std::sync::Arc::new(crate::agent_binary::AgentBinary::for_tests(
        b"fake-binary".to_vec(),
    )));
    let app = router(state);
    let res = app
        .oneshot(Request::get("/api/server-info").header("cookie", &cookie).body(Body::empty())?)
        .await?;
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await?)?;
    assert_eq!(body["agent_update"]["version"], env!("CARGO_PKG_VERSION"));
    let expected_hex: String = ring::digest::digest(&ring::digest::SHA256, b"fake-binary")
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(body["agent_update"]["sha256"], expected_hex);
    Ok(())
}

#[sqlx::test]
async fn agent_update_guards(pool: PgPool) -> anyhow::Result<()> {
    // Machine exists but agent is not connected; arch seeded as x86_64.
    let mid = sqlx::query_scalar!(
        r#"INSERT INTO machines (machine_id, hostname, status, arch)
           VALUES ('upd-guard', 'upd-host', 'online', 'x86_64') RETURNING id"#
    )
    .fetch_one(&pool)
    .await?;
    let cookie = auth_cookie(&pool).await?;

    // 503 when no binary is bundled.
    let app = router(test_state(pool.clone()));
    let res = app
        .oneshot(
            Request::post(format!("/api/machines/{mid}/agent-update"))
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

    // 409 when bundled but agent not connected.
    let mut state = test_state(pool.clone());
    state.agent_binary = Some(std::sync::Arc::new(crate::agent_binary::AgentBinary::for_tests(
        b"fake-binary".to_vec(),
    )));
    let app = router(state);
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/api/machines/{mid}/agent-update"))
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // 409 on arch mismatch (aarch64 machine), even before connectivity.
    let mid_arm = sqlx::query_scalar!(
        r#"INSERT INTO machines (machine_id, hostname, status, arch)
           VALUES ('upd-arm', 'upd-arm-host', 'online', 'aarch64') RETURNING id"#
    )
    .fetch_one(&pool)
    .await?;
    let res = app
        .oneshot(
            Request::post(format!("/api/machines/{mid_arm}/agent-update"))
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::CONFLICT);
    Ok(())
}
```

In `crates/server/src/hub.rs` `mod tests` (or alongside existing hub tests), the frame-sequence test — this is the seam test proving what actually goes down the wire:

```rust
#[tokio::test]
async fn agent_update_streams_announce_then_chunks_matching_hash() {
    let hub = Hub::default();
    let machine = Uuid::new_v4();
    let (tx, mut rx) = mpsc::channel(64);
    hub.register(machine, tx); // match the existing registration helper's real name/signature
    let bin = crate::agent_binary::AgentBinary::for_tests(vec![7u8; 600_000]);

    hub.send_agent_update(machine, "cmd-1".into(), "tester".into(), &bin)
        .await
        .expect("dispatch");

    // First frame: the announce, carrying size + sha.
    let first = rx.recv().await.unwrap().unwrap();
    let Some(server_frame::Payload::Update(u)) = first.payload else {
        panic!("expected UpdateAgent first");
    };
    assert_eq!(u.total_bytes, 600_000);
    assert_eq!(u.command_id, "cmd-1");
    assert!(u.url.is_empty(), "url is superseded and must stay unset");

    // Then chunks: concatenate, verify size, last flag, and hash.
    let mut got = Vec::new();
    let mut saw_last = false;
    while let Some(Ok(frame)) = rx.recv().await {
        let Some(server_frame::Payload::UpdateChunk(c)) = frame.payload else {
            panic!("expected only chunks after announce");
        };
        got.extend_from_slice(&c.data);
        if c.last {
            saw_last = true;
            break;
        }
        assert_eq!(c.data.len(), 256 * 1024, "non-final chunks are full-size");
    }
    assert!(saw_last);
    assert_eq!(got.len(), 600_000);
    let d = ring::digest::digest(&ring::digest::SHA256, &got);
    let hex: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, bin.sha256_hex);
}
```

Adjust the registration call to the Hub's REAL register fn (grep `fn register` in hub.rs; existing hub tests show the exact idiom — mirror them).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-server agent_update -- --nocapture` and `cargo test -p argus-server server_info`
Expected: compile errors (`agent_binary` module, `send_agent_update` missing) — the expected RED signal.

- [ ] **Step 3: Implement**

`crates/server/src/agent_binary.rs`:

```rust
// The bundled agent binary the control plane can push to machines
// (ARGUS_AGENT_BINARY). Loaded once at boot; the version is the workspace
// version by construction -- server and agent are released together.
use anyhow::Context;

pub struct AgentBinary {
    pub bytes: Vec<u8>,
    pub sha256_hex: String,
    pub version: &'static str,
    pub total_bytes: u64,
}

impl AgentBinary {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
        Ok(Self::from_bytes(bytes))
    }

    fn from_bytes(bytes: Vec<u8>) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let sha256_hex = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let total_bytes = bytes.len() as u64;
        AgentBinary { bytes, sha256_hex, version: env!("CARGO_PKG_VERSION"), total_bytes }
    }

    #[cfg(test)]
    pub fn for_tests(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}
```

`config.rs`: add `pub agent_binary_path: Option<String>,` to `Config`, read in `from_env` via the same optional-env accessor the OIDC fields use (`std::env::var("ARGUS_AGENT_BINARY").ok()` if no helper fits).

`main.rs`: after config load, `let agent_binary = config.agent_binary_path.as_deref().map(|p| agent_binary::AgentBinary::load(p)).transpose().unwrap_or_else(|e| { tracing::error!(error = %e, "agent binary configured but unreadable; self-update disabled"); None }).map(Arc::new);` — pass into AppState. (Boot proceeds either way; the endpoint 503s, matching the spec.)

`hub.rs` — one public fn, chunking loop included so the frame sequence is testable at this seam:

```rust
/// Announce + stream a bundled agent binary down the machine's Session.
/// The outbound channel's bounded capacity is the backpressure: `send`
/// awaits when the stream is congested, so a slow agent link just slows
/// this loop rather than ballooning memory.
pub async fn send_agent_update(
    &self,
    machine_id: Uuid,
    command_id: String,
    issued_by: String,
    bin: &crate::agent_binary::AgentBinary,
) -> Result<(), DispatchError> {
    const CHUNK: usize = 256 * 1024;
    let (tx, stream_id) = self.conn_slot(machine_id)?;
    let announce = ServerFrame {
        stream_id,
        payload: Some(server_frame::Payload::Update(UpdateAgent {
            url: String::new(), // superseded: chunks follow on this stream
            version: bin.version.to_string(),
            sha256: bin.sha256_hex.clone(),
            total_bytes: bin.total_bytes,
            command_id,
            issued_by,
        })),
    };
    tx.send(Ok(announce)).await.map_err(|_| DispatchError::NotConnected)?;
    let mut off = 0usize;
    while off < bin.bytes.len() {
        let end = (off + CHUNK).min(bin.bytes.len());
        let last = end == bin.bytes.len();
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::UpdateChunk(UpdateChunk {
                data: bin.bytes[off..end].to_vec(),
                last,
            })),
        };
        tx.send(Ok(frame)).await.map_err(|_| DispatchError::NotConnected)?;
        off = end;
    }
    Ok(())
}
```

(Import `UpdateAgent`/`UpdateChunk` alongside the existing proto imports; reuse `conn_slot` exactly as `send_command` does.)

`http.rs` — AppState gains `pub agent_binary: Option<Arc<crate::agent_binary::AgentBinary>>,` (update `test_state` and `main.rs` construction). Routes in the authed api block:

```rust
.route("/api/server-info", get(server_info))
.route("/api/machines/{id}/agent-update", post(agent_update))
```

Handlers (mirror `run_verb`'s audit-then-dispatch ordering and response mapping exactly — same `VerbResult` struct):

```rust
#[derive(serde::Serialize)]
struct ServerInfoDto {
    version: &'static str,
    agent_update: Option<AgentUpdateInfoDto>,
}

#[derive(serde::Serialize)]
struct AgentUpdateInfoDto {
    version: &'static str,
    sha256: String,
}

/// Fixed at boot; the UI caches it like enrollment-config.
async fn server_info(State(state): State<AppState>) -> Json<ServerInfoDto> {
    Json(ServerInfoDto {
        version: env!("CARGO_PKG_VERSION"),
        agent_update: state.agent_binary.as_ref().map(|b| AgentUpdateInfoDto {
            version: b.version,
            sha256: b.sha256_hex.clone(),
        }),
    })
}

/// Push the bundled agent binary to this machine and wait (bounded) for the
/// agent's staged/refused CommandResult. Mirrors run_verb's ordering: the
/// audit row exists before dispatch, fail-closed on audit failure.
async fn agent_update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    crate::auth::AuthUser(identity): crate::auth::AuthUser,
) -> Response {
    let Some(bin) = state.agent_binary.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no agent binary bundled").into_response();
    };
    // Arch guard: we bundle x86_64-musl only. NULL arch (agent predates
    // inventory) is treated as a mismatch -- refuse rather than guess.
    match repo::machine_arch(&state.pool, id).await {
        Ok(Some(arch)) if arch == "x86_64" => {}
        Ok(_) => return (StatusCode::CONFLICT, "unsupported or unknown arch").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "agent-update: arch lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let command_id = Uuid::new_v4();
    let cid = command_id.to_string();
    let rx = state.hub.register_pending(cid.clone(), id);
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(&identity),
        "agent.update",
        Some(id),
        bin.version,
        command_id,
        "dispatched",
    )
    .await
    {
        state.hub.abandon_pending(&cid);
        tracing::error!(error = %e, "agent-update: dispatched audit write failed; not dispatching");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to record audit entry").into_response();
    }

    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_agent_update(
            id,
            cid.clone(),
            repo::Actor::User(&identity).as_str().into_owned(),
            &bin,
        )
        .await
    {
        state.hub.abandon_pending(&cid);
        if let Err(e) = repo::update_command_result(&state.pool, command_id, id, "denied").await {
            tracing::error!(error = %e, "agent-update: denied audit update failed");
        }
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    match tokio::time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(result)) => Json(VerbResult {
            command_id: cid,
            ok: Some(result.ok),
            message: Some(result.message),
            status: "completed",
        })
        .into_response(),
        Ok(Err(_)) => (StatusCode::INTERNAL_SERVER_ERROR, "result channel closed").into_response(),
        Err(_) => {
            state.hub.abandon_pending(&cid);
            (
                StatusCode::ACCEPTED,
                Json(VerbResult { command_id: cid, ok: None, message: None, status: "pending" }),
            )
                .into_response()
        }
    }
}
```

Adaptation notes for the implementer (verify against the real file, don't guess): the exact `AuthUser`/identity extraction and the `Actor::User(...)` + `issued_by` string idiom must be copied from `run_verb`/`send_command`'s call sites — if `run_verb` derives the issued_by string differently (e.g. `Actor::User(identity).as_str()`), use that exact form; if there's no existing `repo::machine_arch`, add it beside `machine_detail` as `pub async fn machine_arch(exec: impl PgExecutor<'_>, id: Uuid) -> Result<Option<String>> { Ok(sqlx::query_scalar!("SELECT arch FROM machines WHERE id = $1", id).fetch_optional(exec).await?.flatten()) }` — that IS a new query string, so the sqlx prepare rule from Global Constraints applies.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p argus-server agent_update && cargo test -p argus-server server_info && cargo test -p argus-server agent_update_streams`
Expected: PASS. If the new `machine_arch` query was added: run the sqlx prepare + `--check` pair from Global Constraints and stage `.sqlx/`.

- [ ] **Step 5: Full server gates**

Run: `cargo fmt --all && cargo clippy -p argus-server --all-targets -- -D warnings && cargo test -p argus-server`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/server .sqlx
git commit -m "feat(update): bundled agent binary, chunked dispatch, agent-update + server-info endpoints"
```

---

### Task 3: Dockerfile.server bundles the musl agent

**Files:**
- Modify: `deploy/docker/Dockerfile.server`
- Modify: `docs/DEV.md` (dev-run note + rollback one-liner)

**Interfaces:**
- Consumes: Task 2's `ARGUS_AGENT_BINARY` env contract.
- Produces: server image carrying `/usr/local/lib/argus/argus-agent` with `ENV ARGUS_AGENT_BINARY` pre-set.

- [ ] **Step 1: Extend the builder stage**

In `deploy/docker/Dockerfile.server`, the builder stage already ends with `RUN cargo build --release -p argus-server`. Add the musl toolchain to the SHARED `chef` stage (mirroring Dockerfile.agent's lines exactly):

```dockerfile
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
```

and after the server build line in the builder stage:

```dockerfile
# The matching agent binary, bundled so the control plane can push
# self-updates (ARGUS_AGENT_BINARY). Same workspace version by construction.
RUN cargo build --release -p argus-agent --target x86_64-unknown-linux-musl
```

- [ ] **Step 2: Extend the runtime stage**

After the existing server COPY:

```dockerfile
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/argus-agent /usr/local/lib/argus/argus-agent
ENV ARGUS_AGENT_BINARY=/usr/local/lib/argus/argus-agent
```

(Place both before `USER argus`; the file is root-owned and world-readable, which is fine — the server only reads it.)

- [ ] **Step 3: Verify the image builds**

Run: `docker build -f deploy/docker/Dockerfile.server -t argus:selfupdate-test . 2>&1 | tail -5`
Expected: successful build. Then verify the bundle:
`docker run --rm --entrypoint ls argus:selfupdate-test -la /usr/local/lib/argus/` shows `argus-agent`.
(This build takes several minutes; that's expected. If the builder OOMs or the environment can't run docker builds, report DONE_WITH_CONCERNS with the failure output instead of guessing at fixes.)

- [ ] **Step 4: DEV.md notes**

Append to the self-update-relevant part of `docs/DEV.md` (new `##` section "Agent self-update — dev notes", before the verification sections' tail):

```markdown
## Agent self-update — dev notes

Dev control plane: point `ARGUS_AGENT_BINARY` at any locally built agent
(e.g. `target/debug/argus-agent`, or the musl build for prod parity) and
restart the server; `GET /api/server-info` then reports `agent_update`.
The pushed binary's advertised version is the SERVER's workspace version,
not anything probed from the file — in dev those can differ; push what you
mean to push.

Manual rollback after a bad update (the previous binary survives beside the
new one):

    sudo mv /path/to/argus-agent.old /path/to/argus-agent && sudo systemctl restart argus-agent

(dev equivalent: kill the agent and re-run it — the binary path is what was
swapped, the env/config are untouched.)
```

- [ ] **Step 5: Commit**

```bash
git add deploy/docker/Dockerfile.server docs/DEV.md
git commit -m "feat(update): bundle the musl agent in the server image"
```

---

### Task 4: Frontend — server-info, update button, outdated badges

**Files:**
- Modify: `frontend/src/api.ts` (ServerInfo types + getServerInfo + updateAgent)
- Modify: `frontend/src/lib/queries.ts` (`qk.serverInfo`, `useServerInfo`, `useAgentUpdate`)
- Modify: `frontend/src/pages/MachineDetailPage.tsx` (badge + button + confirm dialog + error alert)
- Modify: `frontend/src/pages/FleetPage.tsx` (outdated text in StatusCell)

**Interfaces:**
- Consumes: Task 2's endpoints; existing `StatusBadge`, `describeError`, AlertDialog idiom (see RowActions.tsx's confirm dialog), `VerbOutcome`-style response type already used by `containerAction`/`unitAction` in api.ts (reuse that exact type — grep its name).
- Produces: UI behavior only.

- [ ] **Step 1: api.ts**

```ts
export type ServerInfo = {
  version: string;
  agent_update: { version: string; sha256: string } | null;
};

export async function getServerInfo(): Promise<ServerInfo> {
  const r = unauthenticatedOr(await fetch("/api/server-info"));
  if (!r.ok) throw new Error(`server-info ${r.status}`);
  return r.json();
}

export async function updateAgent(id: string): Promise<ActionOutcome> {
  const r = unauthenticatedOr(await fetch(`/api/machines/${id}/agent-update`, { method: "POST" }));
  if (!r.ok) {
    const text = await r.text();
    throw new Error(text.trim() !== "" ? text : `agent update failed: ${r.status}`);
  }
  return r.json();
}
```

`ActionOutcome` here stands for whatever api.ts's existing verb-response type is actually named (the `{command_id, ok, message, status}` shape `containerAction` returns) — reuse it verbatim, do not mint a duplicate type.

- [ ] **Step 2: queries.ts**

```ts
// in qk:
serverInfo: ["server-info"] as const,

/** Fixed at control-plane boot — cache like enrollment-config. */
export function useServerInfo() {
  return useQuery({ queryKey: qk.serverInfo, queryFn: getServerInfo, staleTime: Infinity });
}

export function useAgentUpdate(id: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => updateAgent(id),
    onSuccess: () => {
      // The version change lands with the agent's next Hello; the machine
      // poll picks it up. Invalidate so the badge clears as soon as it does.
      void qc.invalidateQueries({ queryKey: qk.machine(id) });
      void qc.invalidateQueries({ queryKey: qk.fleet });
    },
  });
}
```

Match the surrounding hooks' exact style (mirror `useUnitAction`'s mutation/invalidation idiom).

- [ ] **Step 3: MachineDetailPage**

Where the header buttons live (Edit + Audit, ~line 316), add the update affordance gated on the spec's condition. Follow the existing patterns in the file for state and dialogs (the Edit dialog shows the controlled-Dialog idiom):

```tsx
const serverInfo = useServerInfo();
const agentUpdate = useAgentUpdate(id);
const [updateOpen, setUpdateOpen] = useState(false);

const bundled = serverInfo.data?.agent_update ?? null;
const updateAvailable =
  bundled !== null &&
  machine.status === "online" &&
  machine.arch === "x86_64" &&
  machine.agent_version !== null &&
  machine.agent_version !== bundled.version;
```

Header addition (beside Audit):

```tsx
{updateAvailable && (
  <>
    <StatusBadge tone="warn" label={`agent v${bundled.version} available`} />
    <Button variant="outline" size="sm" onClick={() => setUpdateOpen(true)}>
      Update agent
    </Button>
  </>
)}
```

Confirm dialog (AlertDialog, same shape as RowActions' protected-verb confirm — Cancel + action; action disabled while pending with Spinner):

```tsx
<AlertDialog open={updateOpen} onOpenChange={(open) => { if (!open) setUpdateOpen(false); }}>
  <AlertDialogContent>
    <AlertDialogHeader>
      <AlertDialogTitle>
        Update agent to <span className="font-mono">v{bundled?.version}</span>?
      </AlertDialogTitle>
      <AlertDialogDescription>
        Replaces the agent binary and re-execs it — this machine drops and re-establishes its
        connection. The previous binary is kept beside it as argus-agent.old.
      </AlertDialogDescription>
    </AlertDialogHeader>
    {agentUpdate.error != null && (
      <Alert variant="destructive">
        <AlertTitle>Update failed</AlertTitle>
        <AlertDescription>{describeError(agentUpdate.error)}</AlertDescription>
      </Alert>
    )}
    <AlertDialogFooter>
      <AlertDialogCancel disabled={agentUpdate.isPending}>Cancel</AlertDialogCancel>
      <AlertDialogAction
        disabled={agentUpdate.isPending}
        onClick={() => {
          agentUpdate.mutate(undefined, { onSuccess: () => setUpdateOpen(false) });
        }}
      >
        {agentUpdate.isPending ? (
          <>
            <Spinner className="size-3.5" />
            Updating…
          </>
        ) : (
          "Update agent"
        )}
      </AlertDialogAction>
    </AlertDialogFooter>
  </AlertDialogContent>
</AlertDialog>
```

After a successful mutate, no success banner: the existing `reconnecting…`/status machinery covers the visible gap, and the badge disappears when the new version lands on poll.

- [ ] **Step 4: FleetPage outdated text**

In `StatusCell`, after the failed-units badge:

```tsx
{serverInfo?.agent_update != null &&
  row.agent_version !== null &&
  row.agent_version !== serverInfo.agent_update.version && (
    <StatusBadge tone="warn" label="agent outdated" />
  )}
```

`StatusCell` is a top-level component — pass `serverInfo` down as a prop from `FleetPage` (which calls `useServerInfo()`), or call the hook inside `StatusCell`; prefer the prop (one subscription). Note this deliberately does NOT gate on arch: a non-x86_64 machine with an old agent still IS outdated — it just can't be updated from here.

- [ ] **Step 5: Gates**

Run: `pnpm --dir frontend run typecheck && pnpm --dir frontend run lint && pnpm --dir frontend run fmt:check && pnpm --dir frontend run build`
Expected: clean (run `pnpm --dir frontend run fmt` first if fmt:check complains).

- [ ] **Step 6: Commit**

```bash
git add frontend/src
git commit -m "feat(update): update-agent button, confirm dialog, outdated badges"
```

---

### Task 5: Full gate run

**Files:** none (verification only).

- [ ] **Step 1: Backend gates**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: clean. NEVER add `-- --ignored`.

- [ ] **Step 2: sqlx cache check**

Run: `DATABASE_URL='postgres://postgres:argus@localhost:5432/argus' cargo sqlx prepare --workspace --check -- --all-targets`
Expected: clean.

- [ ] **Step 3: Frontend build + embed check**

Run: `pnpm --dir frontend run build && cargo check -p argus-server`
Expected: clean.

- [ ] **Step 4: Commit anything the gates changed**

```bash
git add -u && git commit -m "chore(update): gate-run formatting"
```

Only if the gates modified files; otherwise nothing. The live E2E (patch-bumped binary → Update → same-pid re-exec → new version in Hello → `.old` on disk → rollback one-liner) is performed by the controller against the dev stack after this plan completes, and recorded in DEV.md.
