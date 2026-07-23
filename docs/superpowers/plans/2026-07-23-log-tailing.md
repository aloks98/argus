# Log Tailing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An operator opens a systemd unit's journal or a container's logs from the machine page, sees a live tail streamed from the agent, and closing the view stops the tail on the agent — with access audit-logged.

**Architecture:** Slice 5 of 6 (PRD §8), on the rails the previous three slices laid. The agent spawns a `journalctl -o json` subprocess (or a bollard log stream), maps each record to an NDJSON line, batches ~100ms, and emits `LogChunk` frames on the existing Session stream. The server routes chunks by `request_id` into an SSE response; when the browser disconnects, a `Drop` guard sends `LogTailStop`. The browser renders with `@melloware/react-logviewer`, which owns the `EventSource`, virtualization and search.

**Tech Stack:** Rust (tonic/axum/sqlx), `tokio::process` + `serde_json` on the agent, `bollard` for Docker logs, axum 0.8 SSE on the server, React 19 + `@melloware/react-logviewer` on the frontend.

**Design of record:** `docs/superpowers/specs/2026-07-23-log-tailing-design.md`

## Global Constraints

- **Proto is frozen for this slice.** `LogTailRequest { request_id, source, tail_lines, follow }`, `LogTailStop { request_id }`, `LogChunk { request_id, data, eof }`, `ServerFrame.log_tail_start` / `log_tail_stop`, and `AgentFrame.log_chunk` all already exist. Do not edit the proto.
- **No migration.** Nothing about logs is persisted. Only the `logs.open` audit row is written, via the existing `audit_log` table.
- **`LogChunk.data` carries NDJSON** — one JSON object per line, `\n`-separated:
  `{"ts":1784812931123,"level":3,"ident":"nginx","msg":"connect() failed"}`
  `ts` = ms since epoch; `level` = syslog priority `0..=7` or `null`; `ident` = syslog identifier / container name or `null`; `msg` = the line. A drop marker adds `"marker":true` with `level:4`.
- **Logs are best-effort; the Session is not.** Chunk sends use `try_send` and drop on a full channel, counting the loss. Never `await` a full Session channel from a log task — a flooding unit must not delay heartbeats into the 45s offline sweeper.
- **The subprocess is spawned with argv only** (`Command::arg`), never through a shell. There is no string interpolation into a command line anywhere in this slice.
- **`source` is validated twice** — server-side before dispatch and agent-side before use. Neither side trusts the other. Unit names: `A-Za-z0-9:_.@-`. Container refs: `A-Za-z0-9_.-`. Both capped at 256 chars.
- **`tail_lines` is clamped server-side to 1000.**
- **The agent must keep building static for musl.** Verify with `CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl` → `file` must report `static-pie linked`.
- **`ring` only.** No `aws-lc-rs`, no openssl, no cmake.
- **Rust tests need Postgres.** `.cargo/config.toml` exports `DATABASE_URL`; the dev DB is the `argus-pg` container. `#[sqlx::test]` provisions a fresh schema per test.
- **`argus-server` is a bin-only crate** (no `src/lib.rs`): use `cargo test -p argus-server --bin argus <filter>`, never `--lib`.
- **Regenerate the sqlx offline cache if you add or change any `sqlx::query!`** (including in tests), then commit `.sqlx/`. CI builds with `SQLX_OFFLINE=true`; a live `DATABASE_URL` from `.cargo/config.toml` masks a stale cache locally. This broke CI once already:
  ```bash
  DATABASE_URL="postgres://postgres:argus@localhost:5432/argus" cargo sqlx prepare --workspace -- --all-targets
  ```
  Prove it with `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`.
- **Never hold a `std::sync::Mutex` guard across an `.await`** — `Hub::send_command` shows the required pattern (extract, drop the guard, then await).
- **Frontend has no test runner** and this slice does not add one. Gates are `npm --prefix frontend run typecheck` and `run build`. Pure helpers are still exported so a runner can pick them up later.
- **`cargo fmt --all --check` is a hard CI gate.** Run `cargo fmt` before committing.
- **Theming the log viewer is explicitly out of scope.** Ship `LazyLog`'s dark-terminal default; matching it to the design tokens is a follow-up.

### Verified during planning — use exactly these shapes

Both probes compiled against this workspace and were then deleted.

**Agent — journalctl subprocess** (`tokio::process`, already available via tokio's `full` features):

```rust
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

let mut cmd = Command::new("journalctl");
cmd.arg("-u").arg(unit).arg("-n").arg(tail.to_string()).arg("-o").arg("json");
if follow { cmd.arg("-f"); }
let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
let stdout = child.stdout.take().expect("piped");
let mut lines = BufReader::new(stdout).lines();
while let Some(line) = lines.next_line().await? { /* ... */ }
child.start_kill()?;   // this is what stops `journalctl -f`
```

Journal record fields are **strings, not numbers**: `PRIORITY` is `"3"`, `__REALTIME_TIMESTAMP` is microseconds-as-string. `MESSAGE` is usually a string but systemd renders non-UTF8 messages as an **array of byte numbers** — both forms must be handled.

**Server — axum 0.8 SSE with a disconnect guard:**

```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

struct StopGuard { /* ... */ }
impl Drop for StopGuard { fn drop(&mut self) { /* send LogTailStop */ } }

pub fn sse(rx: mpsc::Receiver<Vec<u8>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let guard = StopGuard { /* ... */ };
    let stream = ReceiverStream::new(rx).map(move |chunk| {
        let _ = &guard;                 // guard lives as long as the stream
        Ok(Event::default().data(String::from_utf8_lossy(&chunk).to_string()))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
```

**Also verified during planning, so do not re-derive these:**
- `bollard::query_parameters::LogsOptionsBuilder::default()` (not `new()`), with `.follow(bool).stdout(bool).stderr(bool).tail(&str).build()`; `LogOutput` implements `Display`.
- rnui's `Drawer` wraps **vaul**, whose `Root` takes `open?: boolean` and `onOpenChange?: (open: boolean) => void`.
- `PageHeader` takes `{ title: ReactNode; meta?: ReactNode; className?: string }`.

**Frontend — `@melloware/react-logviewer` (installed, typechecks against React 19):**

```tsx
<LazyLog
  url={url}
  eventsource
  follow
  enableSearch
  eventsourceOptions={{ reconnect: true, formatMessage: (m: unknown) => string }}
  formatPart={(text: string) => ReactNode}
/>
```

`formatMessage` must return a `string`; `formatPart` receives only that string, not the original object — so severity survives as a fixed-width prefix token that `formatPart` parses back off.

---

### Task 1: Build gate — serde_json, journalctl, SSE

**Status: ALREADY COMPLETE** — done during planning, committed as `50bc62a`. Verify it still holds.

**Files:**
- Modify: `crates/agent/Cargo.toml` (already done)

- [ ] **Step 1: Confirm the agent still builds static**

Run:
```bash
CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `... static-pie linked, ... stripped`

- [ ] **Step 2: Confirm journalctl exists on this host**

Run: `journalctl --version | head -1`
Expected: `systemd 2xx (...)`. If absent, the live tests in Task 3 cannot run — report it.

No commit; this task's commit already exists.

---

### Task 2: Agent `logs.rs` — pure source parsing and record mapping

Everything here is testable without a subprocess or a daemon.

**Files:**
- Create: `crates/agent/src/logs.rs`
- Modify: `crates/agent/src/main.rs` (add `mod logs;` after `mod info;`)

**Interfaces:**
- Consumes: nothing.
- Produces, for Tasks 3–5:
  - `pub enum Source { Journal(String), Docker(String) }`
  - `pub fn parse_source(raw: &str) -> Result<Source, SourceError>`
  - `pub struct LogLine { pub ts: i64, pub level: Option<u8>, pub ident: Option<String>, pub msg: String, pub marker: bool }`
  - `pub fn line_to_ndjson(line: &LogLine) -> String` (no trailing newline)
  - `pub fn journal_record_to_line(json: &str) -> Option<LogLine>`
  - `pub fn docker_line(raw: &str, ident: &str) -> LogLine`
  - `pub fn drop_marker(dropped: u64, now_ms: i64) -> LogLine`

- [ ] **Step 1: Write the failing tests**

Create `crates/agent/src/logs.rs` with ONLY the module doc, imports, types, and this test module:

```rust
//! Log tailing (log slice): journal via a `journalctl` subprocess, Docker via
//! bollard. Parsing, validation and record mapping are pure functions so they
//! are testable without a subprocess or a daemon — same shape as `docker.rs`
//! and `systemd.rs`.

use serde::Serialize;

/// Where a tail reads from. Parsed from the wire `source` string, which is
/// browser-supplied — see `parse_source` for why validation lives here as well
/// as on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Journal(String),
    Docker(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    UnknownScheme,
    Empty,
    IllegalCharacter,
    TooLong,
}

/// One rendered log line. Serialized as NDJSON into `LogChunk.data`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogLine {
    pub ts: i64,
    pub level: Option<u8>,
    pub ident: Option<String>,
    pub msg: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub marker: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_source_accepts_both_schemes() {
        assert_eq!(
            parse_source("journal:nginx.service"),
            Ok(Source::Journal("nginx.service".into()))
        );
        assert_eq!(
            parse_source("docker:deadbeef123"),
            Ok(Source::Docker("deadbeef123".into()))
        );
    }

    #[test]
    fn parse_source_rejects_shell_and_path_metacharacters() {
        // These never reach a shell (we spawn with argv), but a unit name is a
        // tightly-defined charset and anything else is a client bug or an
        // attack — reject at the boundary rather than forwarding it.
        for bad in [
            "journal:nginx.service;rm -rf /",
            "journal:../../etc/passwd",
            "journal:nginx service",
            "journal:$(id)",
            "journal:a\nb",
            "docker:abc/def",
        ] {
            assert_eq!(
                parse_source(bad),
                Err(SourceError::IllegalCharacter),
                "must reject {bad}"
            );
        }
    }

    #[test]
    fn parse_source_rejects_empty_target_and_unknown_scheme() {
        assert_eq!(parse_source("journal:"), Err(SourceError::Empty));
        assert_eq!(parse_source("docker:"), Err(SourceError::Empty));
        assert_eq!(parse_source("syslog:foo"), Err(SourceError::UnknownScheme));
        assert_eq!(parse_source("nginx.service"), Err(SourceError::UnknownScheme));
    }

    #[test]
    fn parse_source_rejects_an_overlong_target() {
        let long = format!("journal:{}", "a".repeat(300));
        assert_eq!(parse_source(&long), Err(SourceError::TooLong));
    }

    #[test]
    fn parse_source_accepts_a_template_unit() {
        assert_eq!(
            parse_source("journal:getty@tty1.service"),
            Ok(Source::Journal("getty@tty1.service".into()))
        );
    }

    #[test]
    fn journal_record_maps_the_four_fields() {
        let raw = r#"{"PRIORITY":"3","__REALTIME_TIMESTAMP":"1784812931123456","SYSLOG_IDENTIFIER":"nginx","MESSAGE":"connect() failed"}"#;
        let line = journal_record_to_line(raw).expect("parses");
        assert_eq!(line.level, Some(3));
        // microseconds -> milliseconds
        assert_eq!(line.ts, 1784812931123);
        assert_eq!(line.ident.as_deref(), Some("nginx"));
        assert_eq!(line.msg, "connect() failed");
        assert!(!line.marker);
    }

    #[test]
    fn journal_record_tolerates_missing_optional_fields() {
        let raw = r#"{"MESSAGE":"bare message"}"#;
        let line = journal_record_to_line(raw).expect("parses");
        assert_eq!(line.msg, "bare message");
        assert_eq!(line.level, None);
        assert_eq!(line.ident, None);
    }

    #[test]
    fn journal_record_decodes_an_array_form_message() {
        // systemd renders a non-UTF8 MESSAGE as an array of byte values.
        let raw = r#"{"MESSAGE":[104,105]}"#;
        let line = journal_record_to_line(raw).expect("parses");
        assert_eq!(line.msg, "hi");
    }

    #[test]
    fn journal_record_rejects_malformed_json_without_panicking() {
        assert!(journal_record_to_line("not json").is_none());
        assert!(journal_record_to_line("").is_none());
        // Valid JSON but no MESSAGE is not a log line.
        assert!(journal_record_to_line(r#"{"PRIORITY":"3"}"#).is_none());
    }

    #[test]
    fn ndjson_round_trips_and_omits_marker_when_false() {
        let line = LogLine {
            ts: 42,
            level: Some(6),
            ident: Some("nginx".into()),
            msg: "up".into(),
            marker: false,
        };
        let s = line_to_ndjson(&line);
        assert!(!s.contains('\n'), "must not embed a newline: {s}");
        assert!(!s.contains("marker"), "marker omitted when false: {s}");
        let back: serde_json::Value = serde_json::from_str(&s).expect("valid json");
        assert_eq!(back["ts"], 42);
        assert_eq!(back["level"], 6);
        assert_eq!(back["msg"], "up");
    }

    #[test]
    fn a_message_containing_a_newline_stays_one_ndjson_record() {
        // NDJSON is newline-framed, so an embedded newline would split one log
        // line into two malformed records on the client.
        let line = LogLine {
            ts: 1,
            level: None,
            ident: None,
            msg: "line one\nline two".into(),
            marker: false,
        };
        let s = line_to_ndjson(&line);
        assert_eq!(s.matches('\n').count(), 0);
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["msg"], "line one\nline two");
    }

    #[test]
    fn drop_marker_is_a_warning_line_carrying_the_count() {
        let m = drop_marker(2481, 99);
        assert!(m.marker);
        assert_eq!(m.level, Some(4));
        assert_eq!(m.ts, 99);
        assert!(m.msg.contains("2481"), "must state how many: {}", m.msg);
    }

    #[test]
    fn docker_line_has_no_severity() {
        let line = docker_line("hello from container", "web");
        assert_eq!(line.level, None, "docker logs carry no syslog priority");
        assert_eq!(line.ident.as_deref(), Some("web"));
        assert_eq!(line.msg, "hello from container");
    }

    #[test]
    fn docker_line_strips_a_trailing_newline() {
        assert_eq!(docker_line("hello\n", "web").msg, "hello");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add the module declaration. In `crates/agent/src/main.rs`, change:

```rust
mod info;
```

to:

```rust
mod info;
mod logs;
```

Run: `cargo test -p argus-agent logs`
Expected: FAIL — `cannot find function 'parse_source' in this scope` and similar.

- [ ] **Step 3: Write the implementation**

Insert above the `#[cfg(test)]` block in `crates/agent/src/logs.rs`:

```rust
/// Longest accepted unit name / container reference.
const MAX_TARGET: usize = 256;

/// systemd's unit-name charset. A unit name can never contain a path separator,
/// whitespace, or a shell metacharacter.
fn is_unit_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\')
}

/// Docker's own name/id charset.
fn is_docker_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Parse the wire `source` string into a typed target.
///
/// The server validates this too. Both sides validate on purpose: the agent
/// must not depend on a caller having sanitised its input, because this value
/// becomes a subprocess argument.
pub fn parse_source(raw: &str) -> Result<Source, SourceError> {
    let (scheme, target) = raw.split_once(':').ok_or(SourceError::UnknownScheme)?;
    if target.is_empty() {
        return Err(SourceError::Empty);
    }
    if target.len() > MAX_TARGET {
        return Err(SourceError::TooLong);
    }
    match scheme {
        "journal" => {
            if !target.chars().all(is_unit_char) {
                return Err(SourceError::IllegalCharacter);
            }
            Ok(Source::Journal(target.to_string()))
        }
        "docker" => {
            if !target.chars().all(is_docker_char) {
                return Err(SourceError::IllegalCharacter);
            }
            Ok(Source::Docker(target.to_string()))
        }
        _ => Err(SourceError::UnknownScheme),
    }
}

/// Serialize one line as a single NDJSON record. `serde_json` escapes any
/// newline inside `msg`, so one log line is always exactly one output line —
/// the framing the client relies on.
pub fn line_to_ndjson(line: &LogLine) -> String {
    serde_json::to_string(line).unwrap_or_else(|_| {
        // A LogLine is plain data and cannot fail to serialize; if it somehow
        // did, drop the line rather than the whole tail.
        String::from(r#"{"ts":0,"level":4,"ident":null,"msg":"<unserializable log line>"}"#)
    })
}

/// Map one `journalctl -o json` record. Returns `None` for anything that isn't
/// a usable log line, so a malformed record is skipped rather than killing the
/// tail.
pub fn journal_record_to_line(json: &str) -> Option<LogLine> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let msg = journal_message(v.get("MESSAGE")?)?;
    // journald renders these as STRINGS, not numbers.
    let level = v
        .get("PRIORITY")
        .and_then(|p| p.as_str())
        .and_then(|p| p.parse::<u8>().ok())
        .filter(|p| *p <= 7);
    let ts = v
        .get("__REALTIME_TIMESTAMP")
        .and_then(|t| t.as_str())
        .and_then(|t| t.parse::<i64>().ok())
        .map(|micros| micros / 1000)
        .unwrap_or(0);
    let ident = v
        .get("SYSLOG_IDENTIFIER")
        .and_then(|i| i.as_str())
        .map(|i| i.to_string());
    Some(LogLine {
        ts,
        level,
        ident,
        msg,
        marker: false,
    })
}

/// `MESSAGE` is normally a string, but systemd emits an array of byte values
/// when the message isn't valid UTF-8.
fn journal_message(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let arr = v.as_array()?;
    let bytes: Vec<u8> = arr
        .iter()
        .filter_map(|b| b.as_u64())
        .map(|b| b as u8)
        .collect();
    Some(String::from_utf8_lossy(&bytes).to_string())
}

/// A Docker log line. Docker has no syslog priority, so `level` is always
/// `None` and the viewer renders it without a severity colour.
pub fn docker_line(raw: &str, ident: &str) -> LogLine {
    LogLine {
        ts: now_ms(),
        level: None,
        ident: Some(ident.to_string()),
        msg: raw.trim_end_matches(['\n', '\r']).to_string(),
        marker: false,
    }
}

/// The line injected when chunks had to be dropped, so a gap is always visible
/// rather than silently changing what the operator is reading.
pub fn drop_marker(dropped: u64, now_ms: i64) -> LogLine {
    LogLine {
        ts: now_ms,
        level: Some(4),
        ident: None,
        msg: format!("—— {dropped} lines dropped (stream saturated) ——"),
        marker: true,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
```


- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-agent logs`
Expected: PASS — 12 tests.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: clean. If it fails only with dead-code warnings on the new public items (nothing calls them until Task 3), add a module-level `#![allow(dead_code)]` with a comment saying the next task removes it — and say so in your report.

Run: `cargo fmt --all --check`

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/logs.rs crates/agent/src/main.rs
git commit -m "feat(agent): pure log source parsing and record mapping

Source validation (rejecting anything outside systemd's and docker's own
charsets), journalctl-JSON to NDJSON mapping including the array-form MESSAGE
systemd emits for non-UTF8 lines, and the drop marker — all pure, all tested
without a subprocess."
```

---

### Task 3: Agent `logs.rs` — the tailers and batching

**Files:**
- Modify: `crates/agent/src/logs.rs`

**Interfaces:**
- Consumes from Task 2: `Source`, `parse_source`, `LogLine`, `line_to_ndjson`, `journal_record_to_line`, `docker_line`, `drop_marker`.
- Produces, for Task 4:
  - `pub struct Batcher` with `pub fn new(now_ms: i64) -> Batcher`, `pub fn push(&mut self, line: LogLine)`, `pub fn take_if_ready(&mut self, now_ms: i64) -> Option<Vec<u8>>`, `pub fn note_dropped(&mut self, lines: u64)`, `pub fn take(&mut self, now_ms: i64) -> Option<Vec<u8>>`
  - `pub const FLUSH_INTERVAL: Duration` and `pub const MAX_BATCH_BYTES: usize`
  - `pub async fn run_tail(source: Source, tail_lines: u32, follow: bool, docker: crate::docker::DockerClient, out: tokio::sync::mpsc::Sender<argus_proto::v1::AgentFrame>, request_id: String, stream_id: u64)`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/agent/src/logs.rs`:

```rust
    fn line(msg: &str) -> LogLine {
        LogLine {
            ts: 1,
            level: Some(6),
            ident: None,
            msg: msg.to_string(),
            marker: false,
        }
    }

    #[test]
    fn batcher_holds_lines_until_the_interval_elapses() {
        let mut b = Batcher::new(0);
        b.push(line("a"));
        assert!(
            b.take_if_ready(50).is_none(),
            "must not flush before the interval"
        );
        let out = b.take_if_ready(150).expect("flushes after the interval");
        assert_eq!(String::from_utf8(out).unwrap(), "{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"a\"}\n");
    }

    #[test]
    fn batcher_flushes_early_once_it_is_large() {
        let mut b = Batcher::new(0);
        // Push enough to cross MAX_BATCH_BYTES well before the interval.
        let big = "x".repeat(1024);
        for _ in 0..(MAX_BATCH_BYTES / 1024 + 2) {
            b.push(line(&big));
        }
        assert!(
            b.take_if_ready(1).is_some(),
            "a large batch must flush without waiting for the timer"
        );
    }

    #[test]
    fn batcher_is_empty_when_nothing_was_pushed() {
        let mut b = Batcher::new(0);
        assert!(b.take_if_ready(10_000).is_none());
        assert!(b.take(10_000).is_none());
    }

    #[test]
    fn batcher_emits_a_marker_for_dropped_lines_then_forgets_them() {
        let mut b = Batcher::new(0);
        b.note_dropped(2481);
        b.push(line("after"));
        let out = String::from_utf8(b.take(200).expect("flushes")).unwrap();
        assert!(out.contains("2481 lines dropped"), "got: {out}");
        assert!(out.contains("after"));
        // The count resets, so the next batch doesn't re-report the same gap.
        b.push(line("later"));
        let out2 = String::from_utf8(b.take(400).expect("flushes")).unwrap();
        assert!(!out2.contains("dropped"), "gap must be reported once: {out2}");
    }

    #[test]
    fn batcher_puts_the_marker_before_the_lines_that_followed_the_gap() {
        let mut b = Batcher::new(0);
        b.push(line("before"));
        b.note_dropped(5);
        b.push(line("after"));
        let out = String::from_utf8(b.take(200).unwrap()).unwrap();
        let marker_at = out.find("dropped").expect("marker present");
        let after_at = out.find("after").expect("later line present");
        assert!(marker_at < after_at, "marker must precede the resumed lines");
    }

    #[test]
    fn every_batch_ends_with_a_newline_so_records_stay_framed() {
        let mut b = Batcher::new(0);
        b.push(line("a"));
        b.push(line("b"));
        let out = String::from_utf8(b.take(200).unwrap()).unwrap();
        assert!(out.ends_with('\n'));
        assert_eq!(out.lines().count(), 2);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-agent logs`
Expected: FAIL — `cannot find struct 'Batcher' in this scope`.

- [ ] **Step 3: Write the implementation**

Add to the top imports of `crates/agent/src/logs.rs`:

```rust
use argus_proto::v1::{agent_frame, AgentFrame, LogChunk};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
```

Add above the `#[cfg(test)]` block:

```rust
/// How often a batch is flushed. One frame per interval instead of one per
/// line is what keeps a chatty unit from monopolising the Session stream.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Flush early once a batch reaches this size, so a burst doesn't build an
/// unbounded buffer waiting for the timer.
pub const MAX_BATCH_BYTES: usize = 64 * 1024;

/// Accumulates lines and hands back a framed NDJSON payload. Pure and
/// clock-injected (`now_ms`) so the timing is testable without sleeping.
pub struct Batcher {
    buf: Vec<u8>,
    last_flush_ms: i64,
    dropped: u64,
}

impl Batcher {
    pub fn new(now_ms: i64) -> Batcher {
        Batcher {
            buf: Vec::new(),
            last_flush_ms: now_ms,
            dropped: 0,
        }
    }

    pub fn push(&mut self, line: LogLine) {
        // A pending drop count is flushed as a marker FIRST, so the gap appears
        // before the lines that came after it.
        if self.dropped > 0 {
            let marker = drop_marker(self.dropped, line.ts);
            self.dropped = 0;
            self.write(&marker);
        }
        self.write(&line);
    }

    fn write(&mut self, line: &LogLine) {
        self.buf.extend_from_slice(line_to_ndjson(line).as_bytes());
        self.buf.push(b'\n');
    }

    /// Record that `lines` were lost. Reported on the next flush.
    pub fn note_dropped(&mut self, lines: u64) {
        self.dropped = self.dropped.saturating_add(lines);
    }

    /// Flush if the interval has elapsed or the batch is already large.
    pub fn take_if_ready(&mut self, now_ms: i64) -> Option<Vec<u8>> {
        let due = now_ms.saturating_sub(self.last_flush_ms) >= FLUSH_INTERVAL.as_millis() as i64;
        if due || self.buf.len() >= MAX_BATCH_BYTES {
            self.take(now_ms)
        } else {
            None
        }
    }

    /// Flush unconditionally. `None` when there is nothing pending.
    pub fn take(&mut self, now_ms: i64) -> Option<Vec<u8>> {
        if self.dropped > 0 {
            let marker = drop_marker(self.dropped, now_ms);
            self.dropped = 0;
            self.write(&marker);
        }
        self.last_flush_ms = now_ms;
        if self.buf.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.buf))
    }
}

/// Send one batch on the Session, never blocking. A full channel means the
/// Session is congested; logs yield to heartbeats, metrics and verb results
/// rather than delaying them. Returns false when the batch was dropped.
fn try_emit(
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
    data: Vec<u8>,
    eof: bool,
) -> bool {
    let frame = AgentFrame {
        stream_id,
        payload: Some(agent_frame::Payload::LogChunk(LogChunk {
            request_id: request_id.to_string(),
            data,
            eof,
        })),
    };
    out.try_send(frame).is_ok()
}

/// Run one tail to completion, emitting `LogChunk` frames until the source ends
/// or the task is cancelled. Cancellation happens by aborting this task; the
/// journal child is killed on drop because `Command` is configured with
/// `kill_on_drop`.
pub async fn run_tail(
    source: Source,
    tail_lines: u32,
    follow: bool,
    docker: crate::docker::DockerClient,
    out: mpsc::Sender<AgentFrame>,
    request_id: String,
    stream_id: u64,
) {
    let mut batcher = Batcher::new(now_ms());
    let result = match source {
        Source::Journal(unit) => {
            run_journal(&unit, tail_lines, follow, &mut batcher, &out, &request_id, stream_id).await
        }
        Source::Docker(id) => {
            run_docker(&docker, &id, tail_lines, follow, &mut batcher, &out, &request_id, stream_id)
                .await
        }
    };
    if let Err(e) = result {
        let err = LogLine {
            ts: now_ms(),
            level: Some(3),
            ident: None,
            msg: format!("log tail ended: {e}"),
            marker: true,
        };
        batcher.push(err);
    }
    // Final flush + EOF so the browser learns the tail is over rather than
    // hanging on an open stream.
    let data = batcher.take(now_ms()).unwrap_or_default();
    try_emit(&out, &request_id, stream_id, data, true);
}

async fn run_journal(
    unit: &str,
    tail_lines: u32,
    follow: bool,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("journalctl");
    // argv only — nothing is ever interpolated into a shell command line.
    cmd.arg("-u")
        .arg(unit)
        .arg("-n")
        .arg(tail_lines.to_string())
        .arg("-o")
        .arg("json");
    if follow {
        cmd.arg("-f");
    }
    // Without this an aborted task would leave `journalctl -f` running forever.
    cmd.kill_on_drop(true);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let stdout = child.stdout.take().expect("stdout piped above");
    let mut lines = BufReader::new(stdout).lines();

    while let Some(raw) = lines.next_line().await? {
        if let Some(line) = journal_record_to_line(&raw) {
            batcher.push(line);
        }
        flush_ready(batcher, out, request_id, stream_id);
    }
    Ok(())
}

async fn run_docker(
    docker: &crate::docker::DockerClient,
    id: &str,
    tail_lines: u32,
    follow: bool,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    let mut stream = docker.logs(id, tail_lines, follow)?;
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        let raw = item?;
        batcher.push(docker_line(&raw, id));
        flush_ready(batcher, out, request_id, stream_id);
    }
    Ok(())
}

/// Flush a ready batch, recording the loss when the Session is congested.
fn flush_ready(
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) {
    let now = now_ms();
    if let Some(data) = batcher.take_if_ready(now) {
        let lines = data.iter().filter(|b| **b == b'\n').count() as u64;
        if !try_emit(out, request_id, stream_id, data, false) {
            batcher.note_dropped(lines);
        }
    }
}
```

- [ ] **Step 4: Add `DockerClient::logs` in `crates/agent/src/docker.rs`**

Append to the `impl DockerClient` block:

```rust
    /// A log stream for one container as plain lines. `bollard` yields framed
    /// stdout/stderr output; both are flattened, because the browser shows one
    /// interleaved log exactly as `docker logs` does.
    pub fn logs(
        &self,
        id: &str,
        tail: u32,
        follow: bool,
    ) -> anyhow::Result<impl futures_util::Stream<Item = anyhow::Result<String>>> {
        use bollard::query_parameters::LogsOptionsBuilder;
        use futures_util::StreamExt;
        let docker = self
            .inner
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("docker daemon not available on this host"))?
            .clone();
        // `default()`, not `new()` — this is the shape bollard itself uses in
        // its own `From<LogsOptions>` impl. `.tail` takes a `&str`.
        let opts = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .follow(follow)
            .tail(&tail.to_string())
            .build();
        // `LogOutput` implements Display, which flattens the stdout/stderr
        // framing to the message text — the interleaved view `docker logs` gives.
        Ok(docker
            .logs(id, Some(opts))
            .map(|r| r.map(|out| out.to_string()).map_err(anyhow::Error::from)))
    }
```

Add `futures-util` to `crates/agent/Cargo.toml` only if it is not already there — it is (the systemd slice added it for `StreamExt`).

- [ ] **Step 5: Run the tests**

Run: `cargo test -p argus-agent logs`
Expected: PASS — 18 tests.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings` and `cargo fmt --all --check`
Expected: clean.

- [ ] **Step 6: Verify the musl build**

Run:
```bash
CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 7: Commit**

```bash
git add crates/agent/src/logs.rs crates/agent/src/docker.rs
git commit -m "feat(agent): journal and docker tailers with drop-safe batching

Batcher accumulates lines and flushes on a 100ms interval or 64KB, whichever
comes first. Chunk sends use try_send: a congested Session drops the batch and
counts it rather than blocking, so a chatty unit can never delay a heartbeat
into the offline sweeper. The gap is reported once, ahead of the lines that
followed it. journalctl is spawned with argv and kill_on_drop so an aborted
tail cannot outlive its task."
```

---

### Task 4: Agent `session.rs` — wire the tail registry

**Files:**
- Modify: `crates/agent/src/session.rs`

**Interfaces:**
- Consumes from Tasks 2–3: `logs::{parse_source, run_tail}`.
- Produces: no new API — behavioral only.

- [ ] **Step 1: Add the registry and the inbound arms**

In `crates/agent/src/session.rs`, extend the proto import to include the log types. Change:

```rust
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, CommandResult, DockerState, Heartbeat, Hello,
    SystemdState, Verb,
};
```

to:

```rust
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, CommandResult, DockerState, Heartbeat, Hello, LogChunk,
    SystemdState, Verb,
};
```

Immediately after the `inbound_systemd` / `sender_systemd` clones, add the registry:

```rust
    // request_id -> the task running that tail. A tail must be cancellable by
    // LogTailStop and must not survive the session that requested it, so every
    // entry is aborted when this function returns.
    let tails: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let inbound_tails = tails.clone();
    let inbound_docker_logs = docker.clone();
```

- [ ] **Step 2: Handle `LogTailStart` and `LogTailStop`**

In the inbound loop, the existing `if let Some(server_frame::Payload::Command(cmd)) = frame.payload` becomes a `match`. Replace that `if let` with:

```rust
                    match frame.payload {
                        Some(server_frame::Payload::Command(cmd)) => {
```

…keeping the existing command body unchanged, then closing that arm and adding:

```rust
                        }
                        Some(server_frame::Payload::LogTailStart(req)) => {
                            let stream_id = frame.stream_id;
                            let out = inbound_tx.clone();
                            let docker = inbound_docker_logs.clone();
                            let tails = inbound_tails.clone();
                            let request_id = req.request_id.clone();
                            match crate::logs::parse_source(&req.source) {
                                Ok(source) => {
                                    let rid = request_id.clone();
                                    let handle = tokio::spawn(async move {
                                        crate::logs::run_tail(
                                            source,
                                            req.tail_lines,
                                            req.follow,
                                            docker,
                                            out,
                                            rid,
                                            stream_id,
                                        )
                                        .await;
                                    });
                                    tails
                                        .lock()
                                        .unwrap()
                                        .insert(request_id, handle.abort_handle());
                                }
                                Err(e) => {
                                    // The server validates too, so this is a bug
                                    // or an attack rather than ordinary input.
                                    tracing::warn!(source = %req.source, error = ?e, "log tail: rejected source");
                                    let _ = inbound_tx
                                        .try_send(AgentFrame {
                                            stream_id,
                                            payload: Some(agent_frame::Payload::LogChunk(LogChunk {
                                                request_id,
                                                data: Vec::new(),
                                                eof: true,
                                            })),
                                        });
                                }
                            }
                        }
                        Some(server_frame::Payload::LogTailStop(stop)) => {
                            if let Some(handle) =
                                inbound_tails.lock().unwrap().remove(&stop.request_id)
                            {
                                handle.abort();
                            }
                        }
                        _ => {}
                    }
```

Note the previous trailing comment (`// HelloAck / Ping / other ServerFrames remain no-ops for this slice.`) is replaced by the `_ => {}` arm.

- [ ] **Step 3: Cancel every tail when the session ends**

Immediately after the existing `sender.abort();` line, add:

```rust
    // A tail belongs to the session that asked for it. Without this an ended
    // session would leave `journalctl -f` running until the agent restarts.
    for (_, handle) in tails.lock().unwrap().drain() {
        handle.abort();
    }
```

- [ ] **Step 4: Verify**

Run: `cargo test -p argus-agent`
Expected: PASS — existing tests plus the log tests.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings` and `cargo fmt --all --check`
Expected: clean. If Task 2 added a module-level `#![allow(dead_code)]` to `logs.rs`, remove it now — the module is wired — and confirm clippy still passes.

Run the musl build as in Task 3 Step 6.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/session.rs crates/agent/src/logs.rs
git commit -m "feat(agent): run log tails from the Session, cancellable by request_id

LogTailStart spawns a tail and registers its AbortHandle; LogTailStop aborts
exactly one; session teardown aborts all of them, so a dropped connection can
never leave journalctl -f running. A source the agent rejects returns an
immediate eof rather than silence, so the browser's stream closes."
```

---

### Task 5: Server `hub.rs` — the tail registry

**Files:**
- Modify: `crates/server/src/hub.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces, for Tasks 6–7:
  - `pub fn Hub::open_tail(&self, machine_id: Uuid) -> (String, mpsc::Receiver<LogChunk>)`
  - `pub fn Hub::close_tail(&self, request_id: &str)`
  - `pub fn Hub::deliver_chunk(&self, request_id: &str, machine_id: Uuid, chunk: LogChunk)`
  - `pub async fn Hub::send_log_start(&self, machine_id: Uuid, request_id: String, source: String, tail_lines: u32, follow: bool) -> Result<(), DispatchError>`
  - `pub async fn Hub::send_log_stop(&self, machine_id: Uuid, request_id: String) -> Result<(), DispatchError>`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/server/src/hub.rs`:

```rust
    fn chunk(request_id: &str, body: &str, eof: bool) -> LogChunk {
        LogChunk {
            request_id: request_id.into(),
            data: body.as_bytes().to_vec(),
            eof,
        }
    }

    #[tokio::test]
    async fn open_tail_then_deliver_reaches_the_receiver() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (rid, mut rx) = hub.open_tail(m);
        hub.deliver_chunk(&rid, m, chunk(&rid, "hello", false));
        let got = rx.recv().await.expect("chunk delivered");
        assert_eq!(got.data, b"hello");
    }

    #[tokio::test]
    async fn a_foreign_machine_cannot_deliver_into_another_machines_tail() {
        // Same trust boundary as command results: the tail belongs to the
        // machine it was opened against, and any other authenticated agent
        // must not be able to inject into it.
        let hub = Hub::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let (rid, mut rx) = hub.open_tail(owner);
        hub.deliver_chunk(&rid, other, chunk(&rid, "spoof", false));
        assert!(rx.try_recv().is_err(), "foreign machine must not deliver");
        hub.deliver_chunk(&rid, owner, chunk(&rid, "real", false));
        assert_eq!(rx.recv().await.expect("owner delivers").data, b"real");
    }

    #[tokio::test]
    async fn an_eof_chunk_closes_the_stream() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (rid, mut rx) = hub.open_tail(m);
        hub.deliver_chunk(&rid, m, chunk(&rid, "last", true));
        assert_eq!(rx.recv().await.expect("final chunk").data, b"last");
        assert!(rx.recv().await.is_none(), "eof must close the channel");
    }

    #[tokio::test]
    async fn close_tail_drops_the_sink_and_ends_the_stream() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (rid, mut rx) = hub.open_tail(m);
        hub.close_tail(&rid);
        assert!(rx.recv().await.is_none());
        // Delivering after close is a no-op, not a panic.
        hub.deliver_chunk(&rid, m, chunk(&rid, "late", false));
    }

    #[tokio::test]
    async fn each_open_tail_gets_a_distinct_request_id() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (a, _ra) = hub.open_tail(m);
        let (b, _rb) = hub.open_tail(m);
        assert_ne!(a, b, "two viewers of the same source must not share a tail");
    }

    #[tokio::test]
    async fn send_log_start_emits_a_request_on_a_nonzero_stream() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(4);
        hub.register(m, tx);
        hub.send_log_start(m, "r1".into(), "journal:nginx.service".into(), 200, true)
            .await
            .expect("dispatch");
        let frame = rx.recv().await.unwrap().unwrap();
        assert_ne!(frame.stream_id, 0);
        match frame.payload {
            Some(server_frame::Payload::LogTailStart(r)) => {
                assert_eq!(r.request_id, "r1");
                assert_eq!(r.source, "journal:nginx.service");
                assert_eq!(r.tail_lines, 200);
                assert!(r.follow);
            }
            other => panic!("expected LogTailStart, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_log_stop_to_an_absent_machine_errors() {
        let hub = Hub::new();
        let res = hub.send_log_stop(Uuid::new_v4(), "r1".into()).await;
        assert!(matches!(res, Err(DispatchError::NotConnected)));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-server --bin argus hub`
Expected: FAIL — `no method named 'open_tail'`.

- [ ] **Step 3: Write the implementation**

Extend the proto import at the top of `crates/server/src/hub.rs`:

```rust
use argus_proto::v1::{
    server_frame, Command, CommandResult, Container, LogChunk, LogTailRequest, LogTailStop,
    ServerFrame, Unit, Verb,
};
```

Add a field to `Hub`:

```rust
    /// request_id -> the SSE sink for that tail, with the machine it belongs to.
    /// A stream sink, unlike `pending`'s one-shot.
    tails: Mutex<HashMap<String, (Uuid, mpsc::Sender<LogChunk>)>>,
```

Add these methods to `impl Hub`:

```rust
    /// Register a new tail and return its id plus the receiving end for the SSE
    /// response. The buffer is generous because the agent already batches; a
    /// full buffer here means the browser is slower than the log, and dropping
    /// is handled agent-side where the count can be reported.
    pub fn open_tail(&self, machine_id: Uuid) -> (String, mpsc::Receiver<LogChunk>) {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(64);
        self.tails
            .lock()
            .unwrap()
            .insert(request_id.clone(), (machine_id, tx));
        (request_id, rx)
    }

    /// Drop a tail's sink, ending the SSE stream.
    pub fn close_tail(&self, request_id: &str) {
        self.tails.lock().unwrap().remove(request_id);
    }

    /// Deliver a chunk, but only from the machine the tail was opened against —
    /// the same trust boundary `complete()` enforces for command results. An
    /// `eof` chunk is delivered and then closes the sink.
    pub fn deliver_chunk(&self, request_id: &str, machine_id: Uuid, chunk: LogChunk) {
        // Extract the sender under the lock, then send after dropping the guard.
        let sender = {
            let tails = self.tails.lock().unwrap();
            match tails.get(request_id) {
                Some((owner, tx)) if *owner == machine_id => tx.clone(),
                _ => return,
            }
        };
        let eof = chunk.eof;
        let _ = sender.try_send(chunk);
        if eof {
            self.close_tail(request_id);
        }
    }

    pub async fn send_log_start(
        &self,
        machine_id: Uuid,
        request_id: String,
        source: String,
        tail_lines: u32,
        follow: bool,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::LogTailStart(LogTailRequest {
                request_id,
                source,
                tail_lines,
                follow,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    pub async fn send_log_stop(
        &self,
        machine_id: Uuid,
        request_id: String,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::LogTailStop(LogTailStop { request_id })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// The outbound channel plus a fresh non-zero sub-stream id. Factored out
    /// because three senders now need the same "extract under the lock, then
    /// await outside it" dance.
    fn conn_slot(
        &self,
        machine_id: Uuid,
    ) -> Result<(mpsc::Sender<Result<ServerFrame, Status>>, u64), DispatchError> {
        let conns = self.conns.lock().unwrap();
        let handle = conns.get(&machine_id).ok_or(DispatchError::NotConnected)?;
        let stream_id = handle.next_stream_id.fetch_add(1, Ordering::Relaxed);
        Ok((handle.tx.clone(), stream_id))
    }
```

Refactor the existing `send_command` to use `conn_slot` instead of its inline block, so the three senders share one implementation:

```rust
        let (tx, stream_id) = self.conn_slot(machine_id)?;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p argus-server --bin argus hub`
Expected: PASS — the 7 new tests plus the existing ones.

Run: `cargo clippy -p argus-server --all-targets -- -D warnings` and `cargo fmt --all --check`.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/hub.rs
git commit -m "feat(server): tail registry in the Hub

request_id -> SSE sink, scoped to the machine the tail was opened against so a
foreign agent cannot inject into another machine's stream. send_command,
send_log_start and send_log_stop now share one conn_slot helper for the
extract-under-lock-then-await pattern."
```

---

### Task 6: Server `grpc.rs` — route LogChunk frames

**Files:**
- Modify: `crates/server/src/grpc.rs`

**Interfaces:**
- Consumes from Task 5: `Hub::deliver_chunk`.
- Produces: no new API.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/server/src/grpc.rs`:

```rust
    /// A LogChunk must reach the tail's sink, keyed by the authenticated
    /// machine_id, and must NOT refresh last_seen_at — a streaming log is not
    /// evidence that the agent's heartbeat path is alive, and treating it as
    /// such would let a busy log mask a wedged agent.
    #[sqlx::test]
    async fn handle_agent_frame_log_chunk_reaches_the_tail(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-logs-1".to_string(),
                hostname: "log-host".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (rid, mut rx) = hub.open_tail(machine_id);
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        let before = sqlx::query!("SELECT last_seen_at FROM machines WHERE id = $1", machine_id)
            .fetch_one(&pool)
            .await?
            .last_seen_at;

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 9,
                payload: Some(agent_frame::Payload::LogChunk(argus_proto::v1::LogChunk {
                    request_id: rid.clone(),
                    data: b"{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"hi\"}\n".to_vec(),
                    eof: false,
                })),
            },
            &tx,
        )
        .await?;

        let got = rx.recv().await.expect("chunk reached the sink");
        assert!(String::from_utf8_lossy(&got.data).contains("hi"));

        let after = sqlx::query!("SELECT last_seen_at FROM machines WHERE id = $1", machine_id)
            .fetch_one(&pool)
            .await?
            .last_seen_at;
        assert_eq!(before, after, "a log chunk must not refresh last_seen_at");

        Ok(())
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p argus-server --bin argus handle_agent_frame_log_chunk`
Expected: FAIL — the frame falls through, so `rx.recv()` never yields and the test hangs or panics on the expect.

- [ ] **Step 3: Write the implementation**

In `handle_agent_frame`, after the `SystemdState` arm, add:

```rust
        Some(agent_frame::Payload::LogChunk(chunk)) => {
            // Deliberately no `touch_last_seen` here: a log tail is not evidence
            // that the agent's heartbeat path is healthy, and refreshing on log
            // traffic would let a busy log mask a wedged agent.
            hub.deliver_chunk(&chunk.request_id.clone(), machine_id, chunk);
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p argus-server --bin argus`
Expected: PASS — no regressions.

Run clippy and fmt as before.

- [ ] **Step 5: Regenerate the sqlx cache**

The new test adds `sqlx::query!` calls. Run:

```bash
DATABASE_URL="postgres://postgres:argus@localhost:5432/argus" cargo sqlx prepare --workspace -- --all-targets
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```
Expected: the second command is clean. If it reports "no cached data for this query", the first did not run — do not proceed.

- [ ] **Step 6: Commit**

```bash
git add crates/server/src/grpc.rs .sqlx
git commit -m "feat(server): route LogChunk frames to their tail

Scoped to the authenticated machine_id. Deliberately does not touch
last_seen_at — a streaming log is not evidence the heartbeat path is alive."
```

---

### Task 7: Server `http.rs` — the SSE endpoint

**Files:**
- Modify: `crates/server/src/http.rs`

**Interfaces:**
- Consumes from Task 5: `Hub::{open_tail, close_tail, send_log_start, send_log_stop}`.
- Produces: `GET /api/machines/{id}/logs/stream?source=&tail=&follow=`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/server/src/http.rs`:

```rust
    #[sqlx::test]
    async fn log_stream_rejects_a_bad_source(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        for bad in [
            "syslog:foo",
            "journal:",
            "journal:nginx%20service",
            "journal:..%2F..%2Fetc%2Fpasswd",
            "docker:abc%2Fdef",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/api/machines/{}/logs/stream?source={bad}",
                            Uuid::new_v4()
                        ))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "source {bad} must be rejected"
            );
        }
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_returns_409_when_the_agent_is_offline(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-offline', 'h', 'offline') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, _hub) = app_state_with_hub(pool.clone());
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_opens_audits_and_streams(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: on LogTailStart, push one chunk then eof.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    hub2.deliver_chunk(
                        &req.request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id.clone(),
                            data: b"{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"hello\"}\n".to_vec(),
                            eof: false,
                        },
                    );
                    hub2.deliver_chunk(
                        &req.request_id,
                        machine_id,
                        argus_proto::v1::LogChunk {
                            request_id: req.request_id,
                            data: Vec::new(),
                            eof: true,
                        },
                    );
                }
            }
        });

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service&tail=50"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("hello"), "SSE body must carry the chunk: {text}");

        let row = sqlx::query!(
            "SELECT action, target_ref, result FROM audit_log WHERE machine_id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.action, "logs.open");
        assert_eq!(row.target_ref.as_deref(), Some("journal:nginx.service"));
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_clamps_an_oversized_tail(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('logs-clamp', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;

        let (state, hub) = app_state_with_hub(pool.clone());
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<u32>();
        tokio::spawn(async move {
            let mut seen_tx = Some(seen_tx);
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    if let Some(s) = seen_tx.take() {
                        let _ = s.send(req.tail_lines);
                    }
                }
            }
        });

        let app = router(state);
        let _ = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/stream?source=journal:nginx.service&tail=999999"
                    ))
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(seen_rx.await?, MAX_TAIL_LINES, "tail must be clamped");
        Ok(())
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-server --bin argus log_stream`
Expected: FAIL — the routes 404 rather than returning 400/409/200.

- [ ] **Step 3: Write the implementation**

Add imports to `crates/server/src/http.rs`:

```rust
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
```

Add near `VERB_TIMEOUT`:

```rust
/// Hard ceiling on the backlog a client may ask the agent to render.
pub const MAX_TAIL_LINES: u32 = 1000;

/// Default backlog when the client doesn't ask for one.
const DEFAULT_TAIL_LINES: u32 = 200;

/// Query params for `GET /api/machines/{id}/logs/stream`.
#[derive(serde::Deserialize)]
struct LogStreamQuery {
    source: String,
    tail: Option<u32>,
    follow: Option<bool>,
}

/// Server-side source validation. The agent validates independently — neither
/// side trusts the other, because this value becomes a subprocess argument.
fn source_is_valid(raw: &str) -> bool {
    let Some((scheme, target)) = raw.split_once(':') else {
        return false;
    };
    if target.is_empty() || target.len() > 256 {
        return false;
    }
    match scheme {
        "journal" => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\')),
        "docker" => target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
        _ => false,
    }
}

/// Sends `LogTailStop` when the SSE response is dropped — i.e. the browser
/// navigated away, closed the tab, or lost its connection. This is the only
/// thing that stops a `journalctl -f` from outliving the view that asked for
/// it, so it must stay owned by the stream.
struct TailGuard {
    hub: Arc<Hub>,
    machine_id: Uuid,
    request_id: String,
}

impl Drop for TailGuard {
    fn drop(&mut self) {
        self.hub.close_tail(&self.request_id);
        let hub = self.hub.clone();
        let machine_id = self.machine_id;
        let request_id = self.request_id.clone();
        // Drop is sync; the stop is a send, so it needs a task.
        tokio::spawn(async move {
            if let Err(e) = hub.send_log_stop(machine_id, request_id).await {
                tracing::debug!(error = ?e, "log tail: stop not delivered (agent gone)");
            }
        });
    }
}

/// `GET /api/machines/{id}/logs/stream?source=&tail=&follow=` — open a tail on
/// the agent and stream it to the browser as SSE.
async fn log_stream(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogStreamQuery>,
) -> Response {
    if !source_is_valid(&q.source) {
        return (StatusCode::BAD_REQUEST, "invalid source").into_response();
    }
    let tail = q.tail.unwrap_or(DEFAULT_TAIL_LINES).min(MAX_TAIL_LINES);
    let follow = q.follow.unwrap_or(true);

    // Reading logs is not a mutation, but it can expose secrets, so who read
    // what is recorded — the PRD already treats terminal.open the same way.
    // There is no result to update later, so the row is written once as `ok`.
    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool,
        "anonymous",
        "logs.open",
        Some(id),
        &q.source,
        command_id,
        "ok",
    )
    .await
    {
        tracing::error!(error = %e, "log stream: audit write failed; not opening");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to record audit entry",
        )
            .into_response();
    }

    let (request_id, rx) = state.hub.open_tail(id);
    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_log_start(id, request_id.clone(), q.source.clone(), tail, follow)
        .await
    {
        state.hub.close_tail(&request_id);
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    let guard = TailGuard {
        hub: state.hub.clone(),
        machine_id: id,
        request_id,
    };
    let stream = ReceiverStream::new(rx).map(move |chunk| {
        // The guard is owned by the closure, so it drops with the stream.
        let _ = &guard;
        Ok::<Event, Infallible>(Event::default().data(String::from_utf8_lossy(&chunk.data).to_string()))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}
```

Register the route in `router`, after the units route:

```rust
        .route("/api/machines/{id}/logs/stream", get(log_stream))
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p argus-server --bin argus`
Expected: PASS — the 4 new tests plus everything existing.

Run clippy and fmt.

- [ ] **Step 5: Regenerate the sqlx cache**

The new tests add `sqlx::query!` calls. Run:

```bash
DATABASE_URL="postgres://postgres:argus@localhost:5432/argus" cargo sqlx prepare --workspace -- --all-targets
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/server/src/http.rs .sqlx
git commit -m "feat(server): SSE log stream endpoint

Validates the source independently of the agent, clamps tail to 1000, audits
logs.open before opening, and returns 409 when the agent is offline. A Drop
guard sends LogTailStop when the browser disconnects, which is what stops a
journalctl -f from outliving the view that asked for it."
```

---

### Task 8: Frontend — the log data layer

**Files:**
- Modify: `frontend/src/api.ts`
- Create: `frontend/src/lib/logs.ts`

**Interfaces:**
- Consumes from Task 7: `GET /api/machines/:id/logs/stream`.
- Produces, for Task 9:
  - `type LogSource = string`
  - `logStreamUrl(id: string, source: string, tail?: number, follow?: boolean): string`
  - `type LogLine = { ts: number; level: number | null; ident: string | null; msg: string; marker?: boolean }`
  - `formatLogMessage(raw: unknown): string`
  - `parseLogParts(text: string): { ts: string; level: number | null; ident: string; msg: string }`
  - `levelTone(level: number | null): Tone`

- [ ] **Step 1: Add the URL builder**

Append to `frontend/src/api.ts`:

```ts
/** A log source: `journal:<unit>` or `docker:<container>`. */
export type LogSource = string;

/**
 * The SSE URL for a tail. `LazyLog` opens the EventSource itself, so this
 * returns a URL rather than a fetch — see components/LogViewer.tsx.
 */
export function logStreamUrl(
  id: string,
  source: LogSource,
  tail = 200,
  follow = true,
): string {
  const params = new URLSearchParams({
    source,
    tail: String(tail),
    follow: String(follow),
  });
  return `/api/machines/${id}/logs/stream?${params.toString()}`;
}
```

- [ ] **Step 2: Add the pure log helpers**

Create `frontend/src/lib/logs.ts`:

```ts
// The two halves of the LazyLog seam, kept pure and out of the component.
//
// `formatMessage` must return a string and `formatPart` receives only that
// string — never the original object — so severity has to survive as text.
// These two functions are the encoder and decoder of that hop; they must agree
// on the prefix, which is why they live together.
import type { Tone } from "./status";

export type LogLine = {
  ts: number;
  level: number | null;
  ident: string | null;
  msg: string;
  marker?: boolean;
};

/** Width of the encoded level field, so the decoder can split by index. */
const LEVEL_WIDTH = 1;

/**
 * NDJSON payload -> the single display line LazyLog stores.
 * Layout: `<level><ts-iso> <ident>\t<msg>` where level is one char (`0`-`7`,
 * or `-` when the source has no severity).
 */
export function formatLogMessage(raw: unknown): string {
  let line: LogLine;
  try {
    line = JSON.parse(String(raw)) as LogLine;
  } catch {
    return `-       \t${String(raw)}`;
  }
  const level = line.level === null || line.level === undefined ? "-" : String(line.level);
  const time = new Date(line.ts).toISOString().slice(11, 19);
  const ident = line.ident ?? "";
  return `${level}${time} ${ident}\t${line.msg}`;
}

/** The inverse: split a display line back into its parts for rendering. */
export function parseLogParts(text: string): {
  ts: string;
  level: number | null;
  ident: string;
  msg: string;
} {
  const levelChar = text.slice(0, LEVEL_WIDTH);
  const level = levelChar === "-" ? null : Number(levelChar);
  const rest = text.slice(LEVEL_WIDTH);
  const tab = rest.indexOf("\t");
  if (tab === -1) return { ts: "", level, ident: "", msg: rest };
  const head = rest.slice(0, tab);
  const msg = rest.slice(tab + 1);
  const space = head.indexOf(" ");
  const ts = space === -1 ? head : head.slice(0, space);
  const ident = space === -1 ? "" : head.slice(space + 1);
  return { ts, level, ident, msg };
}

/**
 * syslog priority -> a design-system tone. 0-3 are emerg..err, 4 is warning,
 * 5-7 are notice..debug. `null` (docker, which has no severity) is neutral.
 */
export function levelTone(level: number | null): Tone {
  if (level === null) return "idle";
  if (level <= 3) return "fail";
  if (level === 4) return "warn";
  return "idle";
}
```

- [ ] **Step 3: Verify**

Run: `npm --prefix frontend run typecheck`
Expected: no errors.

Run: `npm --prefix frontend run build`
Expected: builds.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/api.ts frontend/src/lib/logs.ts
git commit -m "feat(frontend): log stream URL builder and the LazyLog seam helpers

formatLogMessage and parseLogParts are the encoder/decoder of the string hop
LazyLog forces between formatMessage and formatPart; they live together because
they must agree on the prefix layout."
```

---

### Task 9: Frontend — the log viewer, drawer, and route

**Files:**
- Create: `frontend/src/components/LogViewer.tsx`
- Create: `frontend/src/components/LogDrawer.tsx`
- Create: `frontend/src/pages/MachineLogsPage.tsx`
- Modify: `frontend/src/app/routes.tsx`
- Modify: `frontend/src/components/UnitsCard.tsx`
- Modify: `frontend/src/components/ContainersCard.tsx`
- Modify: `frontend/src/pages/MachineDetailPage.tsx`

**Interfaces:**
- Consumes from Task 8: `logStreamUrl`, `formatLogMessage`, `parseLogParts`, `levelTone`.
- Produces: `<LogViewer machineId source />`, `<LogDrawer machineId />`

- [ ] **Step 1: Create the viewer**

Create `frontend/src/components/LogViewer.tsx`:

```tsx
// The shared log view, used by both the drawer and the full-page route.
//
// Built on LazyLog, which owns the EventSource, virtualization, search and
// follow. We keep only the rendering: `formatMessage` maps one NDJSON payload
// to a display line, and `formatPart` turns that line back into our own JSX so
// severity uses the design tokens rather than ANSI escapes.
//
// Theming is deliberately out of scope for this slice — LazyLog's dark-terminal
// default ships as-is; matching it to the palette (and to light mode) is a
// follow-up.
import { LazyLog } from "@melloware/react-logviewer";
import { logStreamUrl } from "../api";
import { formatLogMessage, levelTone, parseLogParts } from "../lib/logs";
import { statusTextVariants } from "../lib/status";

export default function LogViewer({
  machineId,
  source,
  height,
}: {
  machineId: string;
  source: string;
  height?: number;
}) {
  return (
    <LazyLog
      key={`${machineId}:${source}`}
      url={logStreamUrl(machineId, source)}
      eventsource
      follow
      enableSearch
      enableSearchNavigation
      caseInsensitive
      height={height}
      eventsourceOptions={{
        reconnect: true,
        formatMessage: (message: unknown) => formatLogMessage(message),
      }}
      formatPart={(text: string) => {
        const { ts, level, ident, msg } = parseLogParts(text);
        return (
          <span className="font-mono text-xs">
            <span className="text-muted-foreground">{ts} </span>
            {ident !== "" && <span className="text-muted-foreground">{ident} </span>}
            <span className={statusTextVariants({ tone: levelTone(level) })}>{msg}</span>
          </span>
        );
      }}
    />
  );
}
```

- [ ] **Step 2: Create the drawer**

Create `frontend/src/components/LogDrawer.tsx`:

```tsx
// Logs as an overlay on the machine page. The open source lives in the URL
// (`?logs=journal:nginx.service`) so it survives a reload and is linkable,
// matching the `?tab=` convention. Closing removes the param.
import { Link, useParams, useSearchParams } from "react-router-dom";
import {
  Button,
  Drawer,
  DrawerContent,
  DrawerDescription,
  DrawerHeader,
  DrawerTitle,
} from "@e412/rnui-react";
import LogViewer from "./LogViewer";

export default function LogDrawer() {
  const { id } = useParams<{ id: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const source = searchParams.get("logs");
  const open = source !== null && id !== undefined;

  function close() {
    const next = new URLSearchParams(searchParams);
    next.delete("logs");
    setSearchParams(next, { replace: true });
  }

  return (
    <Drawer open={open} onOpenChange={(next: boolean) => !next && close()}>
      <DrawerContent className="h-[80vh]">
        <DrawerHeader className="flex-row items-baseline justify-between gap-3">
          <div>
            <DrawerTitle className="font-mono text-sm">{source ?? ""}</DrawerTitle>
            <DrawerDescription className="font-mono text-[11px]">
              Live tail — closing this stops it on the agent.
            </DrawerDescription>
          </div>
          {open && (
            <Button size="sm" variant="outline" render={<Link to={`/machines/${id}/logs?source=${encodeURIComponent(source)}`} />}>
              Expand
            </Button>
          )}
        </DrawerHeader>
        <div className="min-h-0 flex-1 px-4 pb-4">
          {open && <LogViewer machineId={id} source={source} />}
        </div>
      </DrawerContent>
    </Drawer>
  );
}
```

- [ ] **Step 3: Create the full-page route**

Create `frontend/src/pages/MachineLogsPage.tsx`:

```tsx
// The full-page log view. Note the parameter is `?source=` here, not `?logs=`:
// on the machine page `?logs=` means "overlay this source", while here the
// source IS the page's subject. One name for both would make the machine page
// ambiguous about which surface should render.
import { Link, useParams, useSearchParams } from "react-router-dom";
import { Alert, AlertDescription, AlertTitle } from "@e412/rnui-react";
import PageHeader from "../components/PageHeader";
import LogViewer from "../components/LogViewer";

export default function MachineLogsPage() {
  const { id } = useParams<{ id: string }>();
  const [searchParams] = useSearchParams();
  const source = searchParams.get("source");

  if (id === undefined || source === null) {
    return (
      <Alert variant="destructive">
        <AlertTitle>No log source</AlertTitle>
        <AlertDescription>
          This page needs a `source` query parameter, e.g.
          <code className="font-mono"> ?source=journal:nginx.service</code>.
        </AlertDescription>
      </Alert>
    );
  }

  return (
    <>
      <Link to={`/machines/${id}`} className="text-sm text-muted-foreground hover:underline">
        ← Machine
      </Link>
      <PageHeader title={source} meta="Live tail. Leaving this page stops it on the agent." />
      <div className="h-[70vh] border-2 border-border">
        <LogViewer machineId={id} source={source} />
      </div>
    </>
  );
}
```

- [ ] **Step 4: Register the route**

In `frontend/src/app/routes.tsx`, add the import and the route entry (no `nav`, so it does not appear in the sidebar — it is a drill-down):

```tsx
import MachineLogsPage from "../pages/MachineLogsPage";
```

and inside `ROUTES`, after the machine detail entry:

```tsx
  { path: "/machines/:id/logs", element: <MachineLogsPage /> },
```

- [ ] **Step 5: Add the Logs action to both tables**

In `frontend/src/components/UnitsCard.tsx`, add to the imports:

```tsx
import { Link } from "react-router-dom";
```

and add a Logs button as the first entry inside the row's `<ButtonGroup>`:

```tsx
                          <Button
                            size="sm"
                            variant="outline"
                            render={<Link to={`?tab=units&logs=${encodeURIComponent(`journal:${u.name}`)}`} />}
                          >
                            Logs
                          </Button>
```

In `frontend/src/components/ContainersCard.tsx`, the same, using the container id and keeping the containers tab:

```tsx
                          <Button
                            size="sm"
                            variant="outline"
                            render={<Link to={`?tab=containers&logs=${encodeURIComponent(`docker:${c.id}`)}`} />}
                          >
                            Logs
                          </Button>
```

Add `import { Link } from "react-router-dom";` to `ContainersCard.tsx` as well.

- [ ] **Step 6: Mount the drawer on the machine page**

In `frontend/src/pages/MachineDetailPage.tsx`, add the import:

```tsx
import LogDrawer from "../components/LogDrawer";
```

and render it once, immediately before the closing `</>` of the component's main return:

```tsx
      <LogDrawer />
```

- [ ] **Step 7: Verify**

Run: `npm --prefix frontend run typecheck`
Expected: no errors. If `Drawer`'s props differ from the shape used above, check `frontend/node_modules/@e412/rnui-react/dist/index.d.ts` for the real signature and adapt — do not guess.

Run: `npm --prefix frontend run build`
Expected: builds.

- [ ] **Step 8: Commit**

```bash
git add frontend/src
git commit -m "feat(frontend): log viewer, drawer, and full-page route

One LazyLog-based viewer serves both surfaces. The drawer's source lives in
?logs= on the machine page; the full page uses ?source= because there the
source is the page's subject rather than an overlay on it. Logs actions added
to both the Units and Containers rows."
```

---

### Task 10: Full verification and manual E2E

No new code. This is the gate before the PR.

**Files:**
- Modify: `docs/DEV.md`

- [ ] **Step 1: Run the whole workspace**

```bash
npm --prefix frontend run build      # rust-embed embeds frontend/dist — build FIRST
npm --prefix frontend run typecheck
cargo test --workspace
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```
Expected: all pass. Postgres (`argus-pg`) must be up.

- [ ] **Step 2: Confirm the musl gate**

```bash
CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 3: Manual E2E against a real agent**

Follow `docs/DEV.md`'s run recipe. The agent must run as **root** (journal access for other units' logs, and Docker socket access). Verify and record actual output:

1. `curl -N "localhost:8080/api/machines/<id>/logs/stream?source=journal:ssh.service&tail=20&follow=false"` → SSE `data:` lines carrying NDJSON, ending promptly (follow=false must terminate).
2. The same with `follow=true` → the stream stays open; `sudo systemctl restart argus-verify-test.service` (recreate the disposable unit from the systemd slice's notes) and confirm new lines arrive live.
3. **Verify the tail actually dies on disconnect** — this is the slice's central lifecycle claim:
   ```bash
   curl -N ".../logs/stream?source=journal:ssh.service&follow=true" & sleep 3; kill %1
   sleep 2; pgrep -af 'journalctl -u ssh.service' || echo "no orphan — correct"
   ```
   A surviving `journalctl` process means the `Drop` guard is not firing.
4. `source=docker:<id>` against a real container → its logs stream.
5. Bad source (`source=journal:nginx%20service`) → **400**. Agent offline → **409**.
6. `psql`: `SELECT action, target_ref, result FROM audit_log WHERE action = 'logs.open';` → one row per open.
7. **Flood check**: point a tail at a unit producing thousands of lines/sec (e.g. a disposable unit running `while true; do echo x; done`) and confirm (a) the browser keeps up or shows a `lines dropped` marker, and (b) the machine stays `online` — heartbeats were not starved. This is the backpressure design's whole justification; if the machine flaps offline, the `try_send` path is wrong.
8. Browser: open a unit's Logs from the Units tab, confirm the drawer streams, search works, Expand navigates to the full page, and closing stops the tail.
9. Remove any disposable units created for the test and confirm `systemctl list-units --state=failed` is clean.

- [ ] **Step 4: Record the verification in DEV.md**

Append a "Log tailing slice — manual verification" section in the style of the existing slice sections: what was run, against which sources, and the actual observed results — including the orphan check and the flood check.

- [ ] **Step 5: Commit and open the PR**

```bash
git add docs/DEV.md
git commit -m "docs: record log tailing manual verification"
git push -u origin log-slice
```

Then open the PR with `fj` (conventional-commit title, as this repo requires):

```bash
fj pr create "feat(server): journal and docker log tailing over SSE" \
  --base main --head log-slice --body-file <path to a written body>
```

---

## Plan self-review

**Spec coverage** — every section of the design maps to a task:

| Spec section | Task |
|---|---|
| Build gate (serde_json, journalctl, SSE) | 1 (done, `50bc62a`) |
| NDJSON envelope, journal record mapping, drop marker | 2 |
| Batching, drop-on-full, journal + docker tailers | 3 |
| Agent registry, LogTailStart/Stop, session teardown | 4 |
| Hub tail registry, machine-scoped delivery | 5 |
| gRPC LogChunk routing (and no `touch_last_seen`) | 6 |
| SSE endpoint, source validation, tail clamp, audit, 409, disconnect guard | 7 |
| Frontend data layer + LazyLog seam helpers | 8 |
| Viewer, drawer, full-page route, Logs actions | 9 |
| Security (dual validation, argv-not-shell, clamp) | 2 (agent), 7 (server), enforced in Global Constraints |
| Testing | throughout, gated in 10 |

**Deliberate deviations, called out rather than absorbed:**
- The spec says `follow=false` ends with `eof`; Task 3's `run_tail` always emits a final `eof` chunk, which covers both cases — with `follow=true` it only fires when the tail is cancelled or the source ends.
- The spec's "no client-side ring buffer" is honoured by delegating to `LazyLog`; no cap appears anywhere in Task 8 or 9.
- Frontend tests remain absent (no runner), consistent with every prior slice; the helpers in `lib/logs.ts` are exported so a runner can pick them up.

**Interface consistency check:** `parse_source`/`Source`/`LogLine`/`line_to_ndjson`/`journal_record_to_line`/`docker_line`/`drop_marker` (Task 2) are used with those exact names in Task 3. `Batcher::{new,push,note_dropped,take_if_ready,take}` and `run_tail`'s full signature (Task 3) match Task 4's call site. `open_tail`/`close_tail`/`deliver_chunk`/`send_log_start`/`send_log_stop` (Task 5) match Tasks 6 and 7. `MAX_TAIL_LINES` is defined in Task 7 and asserted by Task 7's own clamp test. `logStreamUrl`/`formatLogMessage`/`parseLogParts`/`levelTone` (Task 8) match Task 9's imports exactly.

**Known ripples, handled explicitly:** Task 4 converts an `if let` into a `match` in `session.rs`, which changes indentation of the existing command body — the step says so rather than leaving it to be discovered. Tasks 6 and 7 both add `sqlx::query!` calls and therefore both carry an explicit `cargo sqlx prepare` step; this exact omission broke CI during the systemd slice.
