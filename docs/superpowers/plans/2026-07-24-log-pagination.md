# Log Load-Older Pagination Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An operator reading a systemd unit's journal can scroll up past the initial tail and the viewer auto-loads older entries, back to the start of that unit's journal, while the live tail keeps following at the bottom.

**Architecture:** Journal-only backward pagination on top of the shipped bounded-tail SSE viewer. journald's opaque `__CURSOR` gives exact backward anchors. A backward page reuses the existing `LogChunk` stream (`follow=false` + a new `before_cursor`), which a **separate non-SSE** `GET …/logs/page` endpoint collects into a batch. The frontend viewer moves off react-logviewer's append-only `eventsource` mode to controlled `text` mode, owning the `EventSource` and a line buffer so it can prepend fetched pages and hold scroll position with `scrollToLine`.

**Tech Stack:** Rust (tonic/axum/sqlx), `tokio::process` journalctl on the agent, `@melloware/react-logviewer` (`text` mode) + React 19.

**Design of record:** `docs/superpowers/specs/2026-07-24-log-pagination-design.md`

## Global Constraints

- **Docker is out of scope.** Pagination is journal-only. The `logs/page` endpoint returns `400` for a `docker:` source. Docker keeps the existing bounded live tail unchanged.
- **Proto change is additive only.** Add `string before_cursor = 5` to `LogTailRequest`. Do not renumber existing fields, do not touch other messages. The proto compiles protoc-free via `protox` at build time — a field add just needs a workspace rebuild.
- **`before_cursor` semantics:** empty = a normal tail from the end (unchanged); non-empty + `follow=false` = read the page of entries *before* that journald cursor.
- **The agent spawns journalctl argv-only, never a shell**, with `kill_on_drop(true)` — unchanged from the log slice. A page read is a short-lived non-follow process.
- **NDJSON envelope gains a `cursor` field**, `null` for docker lines and markers (they are never a paging anchor). Serialize with `skip_serializing_if` so existing docker/marker output is byte-unchanged.
- **`before` is always required on the page endpoint** (the first backlog comes from the live SSE tail); missing `before` is `400`. `limit` is clamped to `MAX_TAIL_LINES` (1000).
- **Opening a page is audited** as `logs.page`, written before the fetch, fail-closed — same rationale as `logs.open`.
- **The agent must keep building static for musl.** `CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl` must report `static-pie linked`. No new dependencies (serde_json is already present).
- **`ring` only; no openssl/cmake.**
- **`argus-server` is bin-only** — `cargo test -p argus-server --bin argus <filter>`, never `--lib`.
- **Regenerate the sqlx offline cache** whenever a `sqlx::query!` text is added/changed (the page test seeds rows + reads `audit_log`), then commit `.sqlx/`. CI runs `SQLX_OFFLINE=true`; a live `DATABASE_URL` from `.cargo/config.toml` masks a stale cache. Prove it with `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`.
- **Never hold a `std::sync::Mutex` guard across an `.await`.**
- **`cargo fmt --all --check`** is a hard gate.
- **Frontend has no test runner** (unchanged). Gates are `npm --prefix frontend run typecheck` + `run build`. Pure helpers stay exported for later testing.
- **Verify browser-rendered behaviour in a real browser, not curl.** The log slice's Critical (multi-line SSE render) was a curl-only miss. The scroll-anchor and follow-pause behaviours here are browser-only — Task 8 uses a browser.

### Verified react-logviewer API (checked against the installed package — use exactly these)

`LazyLog` in controlled **`text`** mode, from `dist/esm/components/LazyLog/index.d.ts`:

- `text?: string` — the controlled log content; `getDerivedStateFromProps` reacts to `text` changes.
- `follow?: boolean` — auto-scroll to bottom on content change; set `false` to pause.
- `scrollToLine?: number` — 1-based line to scroll to; changing its value re-triggers the scroll (used to hold the viewport after a prepend).
- `onScroll?(args: { scrollTop: number; scrollHeight: number; clientHeight: number }): void` — fires on user scroll; `scrollTop` near 0 = at top, `scrollHeight - scrollTop - clientHeight` near 0 = at bottom.
- `enableLineNumbers`, `selectableLines`, `enableSearch`, `enableSearchNavigation`, `caseInsensitive`, `height`, `formatPart` — as used today.

`eventsource`/`url`/`eventsourceOptions` are the *append-only* mode being replaced; the reworked viewer does not use them.

---

### Task 1: Proto — `before_cursor` on `LogTailRequest`

**Files:**
- Modify: `crates/proto/proto/argus.proto`

**Interfaces:**
- Produces: `LogTailRequest.before_cursor: String` (prost field), consumed by Tasks 3 and 4.

- [ ] **Step 1: Add the field**

In `crates/proto/proto/argus.proto`, change:

```proto
message LogTailRequest {
  string request_id = 1;
  string source = 2;       // "docker:<container-id>" | "journal:<unit>"
  uint32 tail_lines = 3;
  bool follow = 4;
}
```

to:

```proto
message LogTailRequest {
  string request_id = 1;
  string source = 2;       // "docker:<container-id>" | "journal:<unit>"
  uint32 tail_lines = 3;
  bool follow = 4;
  // Journal pagination: read the page of entries BEFORE this journald cursor.
  // Empty = a normal tail from the end. Only meaningful with follow=false.
  string before_cursor = 5;
}
```

- [ ] **Step 2: Verify the workspace rebuilds with the new field**

Run: `cargo check --workspace`
Expected: builds clean (existing constructors of `LogTailRequest` still compile — a new proto field defaults, but Rust struct literals do not, so if any existing literal breaks, add `before_cursor: String::new()` there; there is exactly one such literal today, in `hub.rs::send_log_start`, which Task 4 updates — for this task, if `cargo check` fails only on that literal, add `..Default::default()` is NOT valid for prost structs, so set `before_cursor: String::new()` inline to keep the build green).

Concretely, if `cargo check` reports a missing-field error in `crates/server/src/hub.rs`, add `before_cursor: String::new(),` to that `LogTailRequest { … }` literal now; Task 4 replaces it with the real value.

- [ ] **Step 3: Commit**

```bash
git add crates/proto/proto/argus.proto crates/server/src/hub.rs
git commit -m "feat(proto): before_cursor on LogTailRequest for journal pagination

Additive field 5. Empty = a normal tail from the end (unchanged); non-empty
with follow=false means read the page of journal entries before that cursor."
```

---

### Task 2: Agent `logs.rs` — cursor field and pure page helpers

**Files:**
- Modify: `crates/agent/src/logs.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces, for Task 3:
  - `LogLine.cursor: Option<String>`
  - `journal_record_to_line` also fills `cursor` from `__CURSOR`
  - `pub fn journal_page_argv(unit: &str, before_cursor: &str, limit: u32) -> Vec<String>` — the argv for a backward page read
  - `pub fn finalize_page(records: Vec<LogLine>, before_cursor: &str) -> Vec<LogLine>` — drop the anchor line, re-order oldest-first

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/agent/src/logs.rs`:

```rust
    #[test]
    fn journal_record_reads_the_cursor() {
        let raw = r#"{"__CURSOR":"s=abc;i=1;b=xyz","PRIORITY":"6","__REALTIME_TIMESTAMP":"1784812931123456","SYSLOG_IDENTIFIER":"nginx","MESSAGE":"up"}"#;
        let line = journal_record_to_line(raw).expect("parses");
        assert_eq!(line.cursor.as_deref(), Some("s=abc;i=1;b=xyz"));
    }

    #[test]
    fn a_record_without_a_cursor_still_parses_with_none() {
        let raw = r#"{"MESSAGE":"no cursor here"}"#;
        let line = journal_record_to_line(raw).expect("parses");
        assert_eq!(line.cursor, None);
    }

    #[test]
    fn ndjson_omits_cursor_when_absent_but_includes_it_when_present() {
        let bare = LogLine {
            ts: 1,
            level: Some(6),
            ident: None,
            msg: "x".into(),
            marker: false,
            cursor: None,
        };
        assert!(!line_to_ndjson(&bare).contains("cursor"), "docker/marker lines stay cursor-free");

        let with = LogLine {
            cursor: Some("s=abc".into()),
            ..bare
        };
        let s = line_to_ndjson(&with);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cursor"], "s=abc");
    }

    #[test]
    fn journal_page_argv_reads_backward_from_the_cursor() {
        let argv = journal_page_argv("nginx.service", "s=abc;i=9", 500);
        // -u <unit> --cursor <c> --reverse -n <limit+1> -o json ; no -f
        assert_eq!(
            argv,
            vec![
                "-u", "nginx.service",
                "--cursor", "s=abc;i=9",
                "--reverse",
                "-n", "501",
                "-o", "json",
            ]
        );
        assert!(!argv.iter().any(|a| a == "-f"), "a page read never follows");
    }

    fn line_with_cursor(cursor: &str, ts: i64) -> LogLine {
        LogLine {
            ts,
            level: Some(6),
            ident: None,
            msg: format!("line {ts}"),
            marker: false,
            cursor: Some(cursor.into()),
        }
    }

    #[test]
    fn finalize_page_drops_the_anchor_and_orders_oldest_first() {
        // journalctl --reverse returns newest-first, starting AT the anchor.
        // Records as received: [anchor(t=30), t=20, t=10].
        let records = vec![
            line_with_cursor("s=anchor", 30),
            line_with_cursor("s=b", 20),
            line_with_cursor("s=a", 10),
        ];
        let page = finalize_page(records, "s=anchor");
        // anchor removed, re-ordered oldest-first
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(ts, vec![10, 20], "anchor dropped, chronological order");
        assert!(
            !page.iter().any(|l| l.cursor.as_deref() == Some("s=anchor")),
            "the boundary line the client already has is never duplicated"
        );
    }

    #[test]
    fn finalize_page_handles_a_start_of_journal_short_read() {
        // Near the start there is no older entry than the anchor: only the
        // anchor comes back, so the page is empty.
        let records = vec![line_with_cursor("s=anchor", 30)];
        assert!(finalize_page(records, "s=anchor").is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p argus-agent logs`
Expected: FAIL — `LogLine` has no field `cursor`; `journal_page_argv` / `finalize_page` not found.

- [ ] **Step 3: Write the implementation**

In `crates/agent/src/logs.rs`, add `cursor` to `LogLine` (after `marker`):

```rust
pub struct LogLine {
    pub ts: i64,
    pub level: Option<u8>,
    pub ident: Option<String>,
    pub msg: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub marker: bool,
    /// journald's opaque `__CURSOR` — the exact backward-paging anchor. `None`
    /// for docker lines and markers, which are never a paging anchor. Omitted
    /// from the wire when `None` so those lines stay byte-unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
```

In `journal_record_to_line`, read the cursor and add it to the returned `LogLine`:

```rust
    let cursor = v
        .get("__CURSOR")
        .and_then(|c| c.as_str())
        .map(|c| c.to_string());
    Some(LogLine {
        ts,
        level,
        ident,
        msg,
        marker: false,
        cursor,
    })
```

Add `cursor: None` to the other `LogLine` constructors in this file — `docker_line` and `drop_marker` — and to the "unserializable" fallback is not a struct literal so it is unaffected. (Search the file for `marker:` to find each literal; there are exactly two besides `journal_record_to_line`: `docker_line` and `drop_marker`. Also the run-tail error marker inside `run_tail`/`run_journal` — add `cursor: None` there too.)

Add the two pure functions near the other pure helpers (above `#[cfg(test)]`):

```rust
/// The argv for a backward page read: the `limit` entries *before*
/// `before_cursor`, newest-first (`finalize_page` re-orders them). `--cursor` is
/// inclusive of the anchor entry and `-n limit+1` fetches it so it can be
/// dropped, so a page never duplicates the boundary line the client already
/// holds. Never follows.
pub fn journal_page_argv(unit: &str, before_cursor: &str, limit: u32) -> Vec<String> {
    vec![
        "-u".into(),
        unit.into(),
        "--cursor".into(),
        before_cursor.into(),
        "--reverse".into(),
        "-n".into(),
        (limit.saturating_add(1)).to_string(),
        "-o".into(),
        "json".into(),
    ]
}

/// Turn a raw `--reverse` page (newest-first, starting at the anchor) into the
/// display page: drop the anchor entry the client already has, and re-order
/// oldest-first so lines arrive in reading order.
pub fn finalize_page(records: Vec<LogLine>, before_cursor: &str) -> Vec<LogLine> {
    let mut kept: Vec<LogLine> = records
        .into_iter()
        .filter(|l| l.cursor.as_deref() != Some(before_cursor))
        .collect();
    kept.reverse();
    kept
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p argus-agent logs`
Expected: PASS.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings` and `cargo fmt --all --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/agent/src/logs.rs
git commit -m "feat(agent): journal cursor + pure page helpers

LogLine gains an optional cursor (journald __CURSOR), omitted from the wire when
absent so docker/marker lines are byte-unchanged. journal_page_argv builds the
backward --cursor --reverse -n(limit+1) read; finalize_page drops the inclusive
anchor entry and re-orders oldest-first. All pure, tested without a subprocess."
```

---

### Task 3: Agent — the page read path

**Files:**
- Modify: `crates/agent/src/logs.rs`, `crates/agent/src/session.rs`

**Interfaces:**
- Consumes from Task 2: `journal_page_argv`, `finalize_page`, `journal_record_to_line`.
- Consumes from Task 1: `LogTailRequest.before_cursor`.
- Produces: `run_tail` gains a `before_cursor: String` parameter; a non-empty `before_cursor` runs the page path.

- [ ] **Step 1: Add the live page test (ignored) and a wiring compile check**

Append to the `tests` module in `crates/agent/src/logs.rs`:

```rust
    /// Live page read against the local journal. Ignored like the repo's other
    /// live-journal tests; run with --ignored under sudo on a systemd host.
    #[tokio::test]
    #[ignore = "needs a live journal; run under sudo"]
    async fn live_journal_page_reads_older_than_a_cursor() {
        // Get a recent cursor from a normal tail first.
        let out = tokio::process::Command::new("journalctl")
            .args(["-u", "ssh.service", "-n", "5", "-o", "json", "--show-cursor"])
            .output()
            .await
            .expect("journalctl");
        let text = String::from_utf8_lossy(&out.stdout);
        let newest_cursor = text
            .lines()
            .filter_map(journal_record_to_line)
            .last()
            .and_then(|l| l.cursor)
            .expect("a cursor from the tail");

        // Now read the page before it.
        let argv = journal_page_argv("ssh.service", &newest_cursor, 10);
        let page_out = tokio::process::Command::new("journalctl")
            .args(&argv)
            .output()
            .await
            .expect("journalctl page");
        let records: Vec<LogLine> = String::from_utf8_lossy(&page_out.stdout)
            .lines()
            .filter_map(journal_record_to_line)
            .collect();
        let page = finalize_page(records, &newest_cursor);
        assert!(
            page.iter().all(|l| l.cursor.as_deref() != Some(newest_cursor.as_str())),
            "the anchor is never in its own page"
        );
        // Page is oldest-first.
        if page.len() >= 2 {
            assert!(page[0].ts <= page[page.len() - 1].ts, "chronological order");
        }
    }
```

- [ ] **Step 2: Run it to confirm it compiles and is ignored**

Run: `cargo test -p argus-agent logs`
Expected: compiles; the new test shows as `ignored`. (It fails to compile until Step 3 threads `before_cursor` — if the crate compiles and the non-ignored tests pass, proceed.)

- [ ] **Step 3: Thread `before_cursor` and add the page branch**

In `crates/agent/src/logs.rs`, change `run_tail`'s signature to add `before_cursor: String` (after `follow`):

```rust
pub async fn run_tail(
    source: Source,
    tail_lines: u32,
    follow: bool,
    before_cursor: String,
    docker: crate::docker::DockerClient,
    out: mpsc::Sender<AgentFrame>,
    request_id: String,
    stream_id: u64,
) {
```

Pass `&before_cursor` into `run_journal` (add the param there too), and leave `run_docker` unchanged (docker ignores pagination). In the `Source::Journal(unit)` arm:

```rust
        Source::Journal(unit) => {
            run_journal(
                &unit,
                tail_lines,
                follow,
                &before_cursor,
                &mut batcher,
                &out,
                &request_id,
                stream_id,
            )
            .await
        }
```

Change `run_journal`'s signature to accept `before_cursor: &str` (after `follow`), and branch at the top of its body — a non-empty cursor is a one-shot page read, not a follow tail:

```rust
async fn run_journal(
    unit: &str,
    tail_lines: u32,
    follow: bool,
    before_cursor: &str,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    if !before_cursor.is_empty() {
        return run_journal_page(unit, before_cursor, tail_lines, batcher, out, request_id, stream_id)
            .await;
    }
    // ...existing live-tail body unchanged...
```

Add `run_journal_page` below `run_journal`:

```rust
/// A one-shot backward page read: spawn `journalctl --cursor --reverse`, collect
/// the whole (bounded) page, drop the anchor, re-order oldest-first, and push it
/// to the batcher. No follow, no ticker — the process exits on its own and the
/// caller's final flush + eof close the request.
async fn run_journal_page(
    unit: &str,
    before_cursor: &str,
    limit: u32,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("journalctl");
    for arg in journal_page_argv(unit, before_cursor, limit) {
        cmd.arg(arg);
    }
    cmd.kill_on_drop(true);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let stdout = child.stdout.take().expect("stdout piped above");
    let mut lines = BufReader::new(stdout).lines();

    let mut records = Vec::new();
    while let Some(raw) = lines.next_line().await? {
        if let Some(line) = journal_record_to_line(&raw) {
            records.push(line);
        }
    }
    for line in finalize_page(records, before_cursor) {
        batcher.push(line);
        flush_ready(batcher, out, request_id, stream_id);
    }
    Ok(())
}
```

(The final flush + `eof` chunk already happen in `run_tail` after `run_journal` returns — the page path needs nothing extra there.)

In `crates/agent/src/session.rs`, pass `req.before_cursor` into `run_tail`. Find the `LogTailStart` arm's `run_tail(` call and insert the argument after `req.follow`:

```rust
                                        crate::logs::run_tail(
                                            source,
                                            req.tail_lines,
                                            req.follow,
                                            req.before_cursor,
                                            docker,
                                            out,
                                            rid,
                                            stream_id,
                                        )
```

- [ ] **Step 4: Verify**

Run: `cargo test -p argus-agent`
Expected: PASS (existing + the ignored live test compiled).

On a systemd host, run the live page test under sudo: build the test binary
(`cargo test -p argus-agent --no-run --message-format=json`, take the executable
path) and run it with `sudo -n <bin> live_journal_page -- --ignored --test-threads=1`.
Expected: PASS. Report the result.

Run: `cargo clippy -p argus-agent --all-targets -- -D warnings`, `cargo fmt --all --check`
Expected: clean.

- [ ] **Step 5: Verify the musl build**

Run:
```bash
CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 6: Commit**

```bash
git add crates/agent/src/logs.rs crates/agent/src/session.rs
git commit -m "feat(agent): journal backward-page read path

A non-empty before_cursor turns a LogTailStart into a one-shot page: spawn
journalctl --cursor --reverse, collect the bounded page, drop the anchor,
re-order oldest-first, stream it, and eof. The live tail path is unchanged;
docker ignores pagination."
```

---

### Task 4: Server — the page endpoint

**Files:**
- Modify: `crates/server/src/hub.rs`, `crates/server/src/http.rs`

**Interfaces:**
- Consumes from Task 1: `LogTailRequest.before_cursor`.
- Produces: `GET /api/machines/{id}/logs/page?source=&before=&limit=` → `{ lines, oldest_cursor, reached_start }`.

- [ ] **Step 1: Extend `send_log_start` with `before_cursor`**

In `crates/server/src/hub.rs`, change `send_log_start` to take a `before_cursor: String` parameter and set it on the `LogTailRequest`:

```rust
    pub async fn send_log_start(
        &self,
        machine_id: Uuid,
        request_id: String,
        source: String,
        tail_lines: u32,
        follow: bool,
        before_cursor: String,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::LogTailStart(LogTailRequest {
                request_id,
                source,
                tail_lines,
                follow,
                before_cursor,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }
```

Update the existing caller in `http.rs::log_stream` (the live tail) to pass `String::new()` for `before_cursor`, and the hub test `send_log_start_emits_a_request_on_a_nonzero_stream` to pass `String::new()` and assert `r.before_cursor == ""`.

- [ ] **Step 2: Write the failing page-endpoint tests**

Append to the `tests` module in `crates/server/src/http.rs`:

```rust
    #[sqlx::test]
    async fn logs_page_rejects_a_docker_source(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/page?source=docker:abc&before=s%3Dx",
                        Uuid::new_v4()
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_requires_a_before_cursor(pool: PgPool) -> anyhow::Result<()> {
        let (state, _hub) = app_state_with_hub(pool);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{}/logs/page?source=journal:ssh.service",
                        Uuid::new_v4()
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_returns_409_when_offline(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-offline', 'h', 'offline') RETURNING id"
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
                        "/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_collects_a_page_audits_and_reports_reached_start(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-online', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool)
        .await?
        .id;
        let (state, hub) = app_state_with_hub(pool.clone());

        // Fake agent: on a LogTailStart with a before_cursor, stream two page
        // lines then eof.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    assert_eq!(req.before_cursor, "s=x", "the cursor must reach the agent");
                    assert!(!req.follow, "a page read never follows");
                    let body = b"{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"older-a\",\"cursor\":\"s=a\"}\n{\"ts\":2,\"level\":6,\"ident\":null,\"msg\":\"older-b\",\"cursor\":\"s=b\"}\n".to_vec();
                    hub2.deliver_chunk(
                        &req.request_id,
                        machine_id,
                        argus_proto::v1::LogChunk { request_id: req.request_id.clone(), data: body, eof: false },
                    );
                    hub2.deliver_chunk(
                        &req.request_id,
                        machine_id,
                        argus_proto::v1::LogChunk { request_id: req.request_id, data: Vec::new(), eof: true },
                    );
                }
            }
        });

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&limit=500"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await?;
        let v: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(v["lines"].as_array().unwrap().len(), 2);
        assert_eq!(v["lines"][0]["msg"], "older-a");
        assert_eq!(v["oldest_cursor"], "s=a");
        assert_eq!(v["reached_start"], true, "a short page means the journal start");

        let row = sqlx::query!(
            "SELECT action, result FROM audit_log WHERE machine_id = $1 AND action = 'logs.page'",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p argus-server --bin argus logs_page`
Expected: FAIL — the route 404s (no handler yet).

- [ ] **Step 4: Implement the endpoint**

In `crates/server/src/http.rs`, add the query struct near `LogStreamQuery`:

```rust
/// Query params for `GET /api/machines/{id}/logs/page`.
#[derive(serde::Deserialize)]
struct LogPageQuery {
    source: String,
    before: Option<String>,
    limit: Option<u32>,
}

/// One page of older journal entries plus the anchor for the next page.
#[derive(serde::Serialize)]
struct LogPage {
    lines: Vec<serde_json::Value>,
    oldest_cursor: Option<String>,
    reached_start: bool,
}
```

Add a bounded wait constant near `VERB_TIMEOUT` (reuse the existing one if present; a page collection should be bounded like a verb):

```rust
/// A page read is a one-shot; bound the collection so a wedged agent can't hang
/// the request. journalctl returns a bounded page quickly.
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
```

Add the handler:

```rust
/// `GET /api/machines/{id}/logs/page?source=&before=&limit=` — one backward page
/// of a unit's journal, collected from a short-lived non-follow tail. Journal
/// only; docker has no cursor.
async fn logs_page(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<LogPageQuery>,
) -> Response {
    // Journal only: reuse the shared validator, then reject anything but a
    // journal source (docker paging is unsupported).
    if !source_is_valid(&q.source) || !q.source.starts_with("journal:") {
        return (StatusCode::BAD_REQUEST, "invalid or non-journal source").into_response();
    }
    let Some(before) = q.before.filter(|b| !b.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "missing `before` cursor").into_response();
    };
    let limit = q.limit.unwrap_or(DEFAULT_TAIL_LINES).min(MAX_TAIL_LINES);

    // Audit before opening, fail-closed — same posture as logs.open.
    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool, "anonymous", "logs.page", Some(id), &q.source, command_id, "ok",
    )
    .await
    {
        tracing::error!(error = %e, "logs page: audit write failed; not opening");
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to record audit entry").into_response();
    }

    let (request_id, mut rx) = state.hub.open_tail(id);
    if let Err(DispatchError::NotConnected) = state
        .hub
        .send_log_start(id, request_id.clone(), q.source.clone(), limit, false, before)
        .await
    {
        state.hub.close_tail(&request_id);
        return (StatusCode::CONFLICT, "agent not connected").into_response();
    }

    // Collect chunks until eof (or timeout). The agent sends the whole page then
    // an eof chunk.
    let mut buf: Vec<u8> = Vec::new();
    let collected = tokio::time::timeout(PAGE_TIMEOUT, async {
        while let Some(chunk) = rx.recv().await {
            buf.extend_from_slice(&chunk.data);
            if chunk.eof {
                break;
            }
        }
    })
    .await;
    state.hub.close_tail(&request_id);
    if collected.is_err() {
        return (StatusCode::GATEWAY_TIMEOUT, "agent did not return a page in time").into_response();
    }

    // Parse the NDJSON page. Lines are already oldest-first from the agent.
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&buf)
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    let oldest_cursor = lines
        .iter()
        .find_map(|l| l.get("cursor").and_then(|c| c.as_str()).map(|c| c.to_string()));
    let reached_start = (lines.len() as u32) < limit;

    Json(LogPage { lines, oldest_cursor, reached_start }).into_response()
}
```

Register the route in `router`, after the logs/stream route:

```rust
        .route("/api/machines/{id}/logs/page", get(logs_page))
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p argus-server --bin argus logs_page`
Expected: PASS — 4 tests.

Run: `cargo test -p argus-server --bin argus` — no regressions (the amended `send_log_start` hub test included).

- [ ] **Step 6: Regenerate the sqlx cache**

The new tests add `sqlx::query!` calls. Run:

```bash
DATABASE_URL="postgres://postgres:argus@localhost:5432/argus" cargo sqlx prepare --workspace -- --all-targets
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
```
Expected: the second command is clean. If it reports "no cached data", the first did not run — do not commit.

Run: `cargo fmt --all --check`.

- [ ] **Step 7: Commit**

```bash
git add crates/server/src/hub.rs crates/server/src/http.rs .sqlx
git commit -m "feat(server): GET /logs/page — one backward journal page

send_log_start carries before_cursor. The page endpoint validates journal-only,
requires `before`, clamps limit, audits logs.page fail-closed, opens a
short-lived non-follow tail via the existing hub machinery, collects LogChunks
until eof (bounded), and returns {lines, oldest_cursor, reached_start}. Docker
sources are 400."
```

---

### Task 5: Frontend data layer — page fetcher and buffer helpers

**Files:**
- Modify: `frontend/src/api.ts`, `frontend/src/lib/logs.ts`

**Interfaces:**
- Consumes from Task 4: `GET /api/machines/:id/logs/page`.
- Produces, for Tasks 6–7:
  - `type LogPage = { lines: LogLine[]; oldest_cursor: string | null; reached_start: boolean }`
  - `fetchLogPage(id, source, before, limit?): Promise<LogPage>`
  - `LogLine` gains `cursor?: string | null`
  - `parseNdjsonBatch(blob: string): LogLine[]`
  - `formatLogLine(line: LogLine): string` (the per-object display string; `formatLogMessage` becomes a thin wrapper for any remaining caller, or is removed)

- [ ] **Step 1: Add the page fetcher and type to `api.ts`**

Append to `frontend/src/api.ts`:

```ts
import type { LogLine } from "./lib/logs";

/** One backward page of journal entries plus the next anchor. */
export type LogPage = {
  lines: LogLine[];
  oldest_cursor: string | null;
  reached_start: boolean;
};

/**
 * Fetch the page of journal entries older than `before`. Journal only — the
 * server rejects docker sources. `before` is the oldest cursor the viewer
 * currently holds.
 */
export async function fetchLogPage(
  id: string,
  source: string,
  before: string,
  limit = 500,
): Promise<LogPage> {
  const params = new URLSearchParams({ source, before, limit: String(limit) });
  const r = await fetch(`/api/machines/${id}/logs/page?${params.toString()}`);
  if (!r.ok) throw new Error(`log page ${r.status}`);
  return r.json();
}
```

- [ ] **Step 2: Refactor `logs.ts` for object-based lines**

In `frontend/src/lib/logs.ts`, add `cursor` to `LogLine`:

```ts
export type LogLine = {
  ts: number;
  level: number | null;
  ident: string | null;
  msg: string;
  marker?: boolean;
  cursor?: string | null;
};
```

Extract the per-object formatter and add the batch parser. Replace `formatOneRecord` (which took a JSON string) with `formatLogLine` (takes a `LogLine`), and keep `formatLogMessage` as a wrapper only if any caller still needs the string→string form — after Task 6 the viewer uses the object path, so `formatLogMessage`'s `eventsource` caller goes away. Add:

```ts
/** Parse one SSE/page NDJSON blob into structured lines, dropping blanks and
 *  anything unparseable (a malformed record must not break the batch). */
export function parseNdjsonBatch(blob: string): LogLine[] {
  const out: LogLine[] = [];
  for (const record of String(blob).split("\n")) {
    if (record.length === 0) continue;
    try {
      out.push(JSON.parse(record) as LogLine);
    } catch {
      out.push({ ts: 0, level: null, ident: null, msg: record });
    }
  }
  return out;
}

/** A LogLine -> the single display line LazyLog stores in `text` mode.
 *  Layout: `<level><ts-iso> <ident>\t<msg>`; level is one char (`0`-`7` or `-`). */
export function formatLogLine(line: LogLine): string {
  const level = line.level === null || line.level === undefined ? "-" : String(line.level);
  const time = new Date(line.ts).toISOString().slice(11, 19);
  const ident = line.ident ?? "";
  // A multi-line MESSAGE is one record and must stay one display row.
  const msg = line.msg.replace(/\r?\n/g, " ⏎ ");
  return `${level}${time} ${ident}\t${msg}`;
}
```

Keep `parseLogParts` and `levelTone` exactly as they are. Remove `formatLogMessage` and the old `formatOneRecord` (Task 6 removes their only caller); if `tsc` reports them still imported anywhere, that import is updated in Task 6 — for this task, deleting them may leave a temporary unused-export, which is fine (no runtime effect), or keep `formatLogMessage` until Task 6. Choose: **keep `formatLogMessage`/`formatOneRecord` until Task 6**, so this task's build stays green; Task 6 deletes them.

- [ ] **Step 3: Verify**

Run: `npm --prefix frontend run typecheck`
Expected: no errors.

Run: `npm --prefix frontend run build`
Expected: builds.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/api.ts frontend/src/lib/logs.ts
git commit -m "feat(frontend): log page fetcher and object-based line helpers

fetchLogPage hits GET /logs/page. LogLine gains cursor; parseNdjsonBatch turns a
blob into structured lines and formatLogLine renders one — the object path the
controlled-text viewer needs, replacing the string-hop formatMessage seam."
```

---

### Task 6: Frontend — self-owned EventSource in controlled `text` mode

This is behaviour-preserving: the live tail must look and behave exactly as it does today (stream, follow, search, select, colours, alignment) but driven by our own buffer instead of LazyLog's `eventsource`. Task 7 adds pagination on top.

**Files:**
- Modify: `frontend/src/components/LogViewer.tsx`, `frontend/src/lib/logs.ts`

**Interfaces:**
- Consumes from Task 5: `parseNdjsonBatch`, `formatLogLine`, `LogLine`.
- Produces: a `LogViewer` that owns an `EventSource` and a `lines` buffer, rendered via LazyLog `text`.

- [ ] **Step 1: Rewrite `LogViewer` to controlled `text` mode**

Replace `frontend/src/components/LogViewer.tsx` with:

```tsx
// The shared log view. Drives LazyLog in controlled `text` mode: we own the
// EventSource and the line buffer, because the library's `eventsource` mode is
// append-only and Task 7 needs to PREPEND older pages. Behaviour of the live
// tail is unchanged — stream, follow, search, select.
import { useEffect, useMemo, useRef, useState } from "react";
import { LazyLog } from "@melloware/react-logviewer";
import { logStreamUrl } from "../api";
import type { LogLine } from "../lib/logs";
import { formatLogLine, levelTone, parseLogParts, parseNdjsonBatch } from "../lib/logs";
import type { Tone } from "../lib/status";

const LEVEL_TEXT: Record<Tone, string> = {
  ok: "text-[var(--ok-text)]",
  warn: "text-[var(--warn-text)]",
  fail: "text-[var(--fail-text)]",
  idle: "text-foreground",
};

/** Bound the buffer so a long session can't grow unbounded. */
const MAX_LINES = 50_000;

export default function LogViewer({
  machineId,
  source,
  height,
}: {
  machineId: string;
  source: string;
  height?: number;
}) {
  const [lines, setLines] = useState<LogLine[]>([]);

  // Own the EventSource so the buffer is ours to append to (and, in Task 7,
  // prepend to). Re-created when machine or source changes.
  useEffect(() => {
    setLines([]);
    const es = new EventSource(logStreamUrl(machineId, source));
    es.onmessage = (e) => {
      const batch = parseNdjsonBatch(e.data);
      if (batch.length === 0) return;
      setLines((prev) => {
        const next = prev.concat(batch);
        return next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
      });
    };
    // The browser's EventSource auto-reconnects; nothing to do on error.
    return () => es.close();
  }, [machineId, source]);

  const text = useMemo(() => lines.map(formatLogLine).join("\n"), [lines]);

  return (
    <LazyLog
      text={text}
      follow
      selectableLines
      enableSearch
      enableSearchNavigation
      caseInsensitive
      height={height}
      formatPart={(part: string) => {
        const { ts, level, ident, msg } = parseLogParts(part);
        return (
          <span className="font-mono text-xs">
            <span className="mr-2 inline-block text-muted-foreground tabular-nums">{ts}</span>
            <span className="mr-2 inline-block min-w-[12ch] text-muted-foreground">{ident}</span>
            <span className={LEVEL_TEXT[levelTone(level)]}>{msg}</span>
          </span>
        );
      }}
    />
  );
}
```

Note: line numbers stay on via LazyLog's default (`enableLineNumbers` defaults `true`, as the log slice left it) — do not pass the prop. Do not invent an `enableLines` prop; it does not exist.

- [ ] **Step 2: Remove the now-dead string-hop helpers**

In `frontend/src/lib/logs.ts`, delete `formatLogMessage` and `formatOneRecord` (their only caller, the old `eventsourceOptions.formatMessage`, is gone). Keep `parseNdjsonBatch`, `formatLogLine`, `parseLogParts`, `levelTone`.

- [ ] **Step 3: Verify**

Run: `npm --prefix frontend run typecheck`
Expected: no errors (no remaining references to `formatLogMessage`).

Run: `npm --prefix frontend run build`
Expected: builds.

Note: this is behaviour-preserving but only fully confirmable in a browser (Task 8). The static gates confirm it compiles and the wiring is sound.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/components/LogViewer.tsx frontend/src/lib/logs.ts
git commit -m "refactor(frontend): drive the log viewer from our own EventSource

Move LazyLog from append-only eventsource mode to controlled text mode: we own
the EventSource and a bounded line buffer, rendered via `text`. Behaviour of the
live tail is unchanged; this is the seam Task 7 prepends older pages into.
Removes the now-dead formatMessage string hop."
```

---

### Task 7: Frontend — load older on scroll-to-top

**Files:**
- Modify: `frontend/src/components/LogViewer.tsx`

**Interfaces:**
- Consumes from Task 5: `fetchLogPage`, `LogPage`.

- [ ] **Step 1: Add pagination state, scroll handling, and prepend**

Extend `LogViewer` (keeping the Task 6 EventSource effect). Add state and handlers:

```tsx
  const [reachedStart, setReachedStart] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  // `follow` pauses when the operator scrolls up and resumes at the bottom.
  const [following, setFollowing] = useState(true);
  // Target line to hold the viewport after a prepend (LazyLog scrollToLine).
  const [anchorLine, setAnchorLine] = useState<number | undefined>(undefined);
  const loadingRef = useRef(false);

  const isJournal = source.startsWith("journal:");

  async function loadOlder() {
    if (loadingRef.current || reachedStart || !isJournal) return;
    // The oldest cursor we hold — the first line with one (markers have none).
    const oldest = lines.find((l) => l.cursor)?.cursor;
    if (oldest === undefined || oldest === null) return;
    loadingRef.current = true;
    setLoadingOlder(true);
    try {
      const page = await fetchLogPage(machineId, source, oldest);
      if (page.lines.length > 0) {
        setLines((prev) => {
          const merged = page.lines.concat(prev);
          return merged.length > MAX_LINES ? merged.slice(0, MAX_LINES) : merged;
        });
        // The line that was at the top is now shifted down by page.lines.length;
        // scroll to it (1-based) so the viewport does not jump.
        setAnchorLine(page.lines.length + 1);
      }
      if (page.reached_start) setReachedStart(true);
    } catch {
      // Leave the buffer intact; a transient failure just means no older lines
      // loaded this time.
    } finally {
      loadingRef.current = false;
      setLoadingOlder(false);
    }
  }

  function onScroll({
    scrollTop,
    scrollHeight,
    clientHeight,
  }: {
    scrollTop: number;
    scrollHeight: number;
    clientHeight: number;
  }) {
    // Near the top: load older (journal only).
    if (scrollTop <= 40) void loadOlder();
    // Follow only while at the bottom.
    const atBottom = scrollHeight - scrollTop - clientHeight <= 40;
    setFollowing(atBottom);
  }
```

Reset the pagination state in the source-change effect (alongside `setLines([])`): `setReachedStart(false); setFollowing(true); setAnchorLine(undefined);`.

Pass the new props to `LazyLog`: replace `follow` with `follow={following}`, and add `onScroll={onScroll}` and `scrollToLine={anchorLine}`.

- [ ] **Step 2: Add the top affordance**

Wrap the `LazyLog` in a column so a status row can sit above it. Above the viewer, when journal:

```tsx
  return (
    <div className="flex h-full min-h-0 flex-col">
      {isJournal && (
        <div className="pb-1 text-center font-mono text-[10px] uppercase tracking-widest text-muted-foreground">
          {reachedStart
            ? "— beginning of journal —"
            : loadingOlder
              ? "loading older…"
              : "scroll up to load older"}
        </div>
      )}
      <div className="min-h-0 flex-1">
        <LazyLog /* …props as above… */ />
      </div>
    </div>
  );
```

- [ ] **Step 3: Verify**

Run: `npm --prefix frontend run typecheck` and `npm --prefix frontend run build`
Expected: both clean.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/components/LogViewer.tsx
git commit -m "feat(frontend): auto-load older journal entries on scroll-to-top

onScroll near the top fetches the page before the oldest held cursor, prepends
it, and uses scrollToLine to hold the viewport. Follow pauses when scrolled up
and resumes at the bottom. A top row shows loading / beginning-of-journal.
Docker (no cursor) shows none of this."
```

---

### Task 8: Full verification and manual E2E (in a browser)

**Files:**
- Modify: `docs/DEV.md`

- [ ] **Step 1: Run the whole workspace**

```bash
npm --prefix frontend run build      # embed FIRST
npm --prefix frontend run typecheck
cargo test --workspace
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```
Expected: all pass. Postgres (`argus-pg`) up.

- [ ] **Step 2: Musl gate**

```bash
CC_x86_64_unknown_linux_musl=musl-gcc cargo build -p argus-agent --release --target x86_64-unknown-linux-musl
file target/x86_64-unknown-linux-musl/release/argus-agent
```
Expected: `static-pie linked`.

- [ ] **Step 3: Endpoint checks (curl)**

Run the control plane + agent (agent as root for journal access; see `docs/DEV.md`). Pick a unit with a long journal (e.g. `ssh.service`). Record actual output:

1. Tail once to grab a cursor:
   `curl -sN ".../logs/stream?source=journal:ssh.service&tail=5&follow=false"` → note a `cursor` value in a line.
2. Page before it:
   `curl -s ".../logs/page?source=journal:ssh.service&before=<cursor>&limit=10" | jq '{n:(.lines|length), oldest_cursor, reached_start}'` → up to 10 older lines, an `oldest_cursor`, `reached_start` true only near the journal start.
3. `.../logs/page?source=docker:<id>&before=x` → **400**. `.../logs/page?source=journal:ssh.service` (no `before`) → **400**. Agent offline → **409**.
4. `audit_log`: a `logs.page` row per page fetch.

- [ ] **Step 4: Browser E2E — the behaviours static gates cannot confirm**

This is required, not optional — the scroll-anchor, follow-pause, and prepend are browser-only, and the log slice's Critical was a curl-only miss. Open the app, open a unit's logs, and verify:

1. The live tail streams and follows at the bottom (unchanged).
2. **Scroll up** → "loading older…" appears, older lines prepend, and **the viewport does not jump** (the line you were reading stays put).
3. Keep scrolling up → more pages load; at the journal start, "— beginning of journal —" shows and no further fetches fire.
4. **Follow pauses** while scrolled up (new live lines don't yank you down); scrolling back to the bottom **resumes** follow.
5. Text is still **selectable** and copyable (the dialog fix is intact); colours and column alignment unchanged.
6. A **docker** source shows the live tail with no "load older" row.

- [ ] **Step 5: Record verification in DEV.md**

Append a "Log pagination — manual verification" section to `docs/DEV.md` in the existing style: the endpoint checks and the browser behaviours, with actual observations (especially the no-jump prepend and follow-pause).

- [ ] **Step 6: Commit and open the PR**

```bash
git add docs/DEV.md
git commit -m "docs: record log pagination manual verification"
git push -u origin log-pagination-slice
fj pr create "feat(server): journal log load-older pagination" \
  --base main --head log-pagination-slice --body-file <a written body>
```

---

## Plan self-review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Proto `before_cursor` | 1 |
| NDJSON `cursor` field; journal-only rationale | 2 |
| Agent `__CURSOR` read + `journal_page_argv` + anchor-drop/reorder | 2 |
| Agent page read path (`--cursor --reverse`, no follow) | 3 |
| `send_log_start` before_cursor + session wiring | 3, 4 |
| Server `GET /logs/page` (journal-only, `before` required, clamp, audit, collect-until-eof, `{lines, oldest_cursor, reached_start}`) | 4 |
| Frontend page fetcher + object line helpers | 5 |
| Viewer → controlled `text` mode, own EventSource + buffer | 6 |
| Auto-load on scroll-to-top, prepend, scrollToLine anchor, follow pause/resume, states, docker shows none | 7 |
| Error handling (409/timeout leave buffer intact) | 7 (catch), 4 (endpoint) |
| Testing (agent pure + ignored live; server oneshot/sqlx; browser E2E) | throughout, gated in 8 |

**Deliberate deviations, flagged:** the frontend has no test runner (unchanged), so Task 7's prepend/anchor math is verified in a browser (Task 8) rather than by a unit test; the pure `parseNdjsonBatch`/`formatLogLine` are exported for a future runner.

**Type/interface consistency:** `before_cursor` is the proto/Rust name throughout (Tasks 1, 3, 4); the query param is `before` and the NDJSON/JSON field is `cursor` (Tasks 4, 5) — three deliberately distinct names, documented in the spec. `run_tail`'s new `before_cursor: String` param (Task 3) matches the session call site. `send_log_start`'s new `before_cursor: String` (Task 4 Step 1) matches its `log_stream` caller (passes `String::new()`) and the page handler (passes `before`). `LogPage`/`fetchLogPage` (Task 5) match Task 7's usage. `parseNdjsonBatch`/`formatLogLine` (Task 5) match Task 6's imports.

**Known ripple, handled:** Task 1 Step 2 and Task 4 Step 1 both address the one existing `LogTailRequest` struct literal (`hub.rs::send_log_start`) — Task 1 keeps the build green with `String::new()`, Task 4 replaces it with the real parameter. Task 6 Step 2 removes `formatLogMessage`/`formatOneRecord`, whose only caller Task 6 Step 1 deletes; Task 5 explicitly keeps them until then so no task has a broken build.
