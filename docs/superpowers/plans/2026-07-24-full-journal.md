# Full System Journal + Log Filters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read the whole system journal for a machine — not just one unit — filtered by severity and time window, with the same live tail and load-older behaviour the per-unit view already has.

**Architecture:** A new `journal:@system` source maps to `Source::JournalAll` (omit `-u`). Three additive proto fields carry the filters to the agent, which turns them into `journalctl` argv. Because `journalctl` **rejects `--since` together with `--cursor`**, the time window is an argv flag on live tails but a **timestamp cutoff applied in `finalize_page`** on paginated reads — which makes the existing short-page `reached_start` rule mean "start of the window" with no new signalling.

**Tech Stack:** Rust (tonic/prost, tokio, axum), React + TS, `@e412/rnui-react` (Rnui) on Tailwind v4, `@melloware/react-logviewer`.

## Global Constraints

- **Design of record:** `docs/superpowers/specs/2026-07-24-full-journal-design.md`. Read it before starting.
- **Proto is additive only.** Add fields 6, 7, 8 to `LogTailRequest`. Never renumber or reuse a field number.
- **`max_priority` is a severity ceiling in syslog numbering, where lower is more severe.** It becomes `journalctl -p <n>`, returning entries with numeric priority **≤ n**. `max_priority = 3` (err) returns err, crit, alert, emerg. **Do not invert this.**
- **Zero means unset for all three filter fields** (`max_priority = 0`, `since_ms = 0`, `current_boot = false`), so a default-valued request reproduces today's behaviour exactly.
- **`--since` must NEVER be emitted on a cursor-anchored (page) read** — `journalctl` exits with "Please specify only one of --since=, --cursor=, --cursor-file=, and --after-cursor=". `-p` and `-b` DO compose with `--cursor`.
- **`since_ms` is absolute** (unix epoch ms), resolved by the server at request time. Never send a relative window.
- **`window` → proto mapping** (the only definition): `boot` → `current_boot=true, since_ms=0`; `1h` → `current_boot=false, since_ms=now-3_600_000`; `24h` → `current_boot=false, since_ms=now-86_400_000`; `all` → `current_boot=false, since_ms=0`. The two are never set together.
- **Defaults differ per surface, deliberately:** Logs tab = priority all, window **boot**. Per-unit dialog = priority all, window **all** (unchanged). Defaulting per-unit to boot would cap scroll-back at the last reboot — a regression.
- **Agent stays lean and musl-static.** No new dependencies. `journalctl` is spawned argv-only, never through a shell.
- **Every verb goes through the audit log.** Audit ordering stays as the pagination fix-wave left it: dispatch first, then audit, then collect — so a 409/504 leaves no misleading `ok` row.
- **Host-dependent tests MUST be named `live_*`** — CI runs `cargo test --workspace -- --ignored --skip live_`, so any other name will run in the container and fail.
- **UI uses Rnui.** `Select` is verified to work as `<Select value={v} onValueChange={(v: string | null) => …}>` with `SelectTrigger size="sm"` / `SelectValue` / `SelectContent` / `SelectItem value=…`.
- **Frontend must build before the server** (`rust-embed` embeds `frontend/dist`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/proto/proto/argus.proto` | +3 additive fields on `LogTailRequest` |
| `crates/agent/src/logs.rs` | `Source::JournalAll`, `JournalFilters`, both argv builders, cutoff in `finalize_page`, spawn-failure marker |
| `crates/agent/src/session.rs` | pass filters from the request into `run_tail` |
| `crates/server/src/hub.rs` | `LogFilters` struct; `send_log_start` carries it |
| `crates/server/src/http.rs` | query params, validation, `window`→fields resolution, audit target |
| `frontend/src/api.ts` | `LogWindow`, `LogFilters`, filter params on both log URLs |
| `frontend/src/components/LogFilterBar.tsx` | **new** — priority + window selects |
| `frontend/src/components/LogViewer.tsx` | filters in URLs, reset on change, window-aware end-of-history |
| `frontend/src/components/LogDialog.tsx` | filter bar in the per-unit dialog |
| `frontend/src/pages/MachineDetailPage.tsx` | Logs tab + `?priority=`/`?window=` URL state |

---

### Task 1: Proto fields, whole-journal source, filter-aware argv

**Files:**
- Modify: `crates/proto/proto/argus.proto` (`LogTailRequest`)
- Modify: `crates/agent/src/logs.rs` (`Source`, `parse_source`, argv builders, `run_journal`, `run_journal_page`, `run_tail`)

**Interfaces:**
- Produces:
  - `Source::JournalAll` variant; proto fields `max_priority: u32` (6), `since_ms: u64` (7), `current_boot: bool` (8)
  - `pub struct JournalFilters { pub max_priority: u32, pub since_ms: u64, pub current_boot: bool }` (derives `Default`)
  - `pub fn journal_tail_argv(unit: Option<&str>, tail_lines: u32, follow: bool, f: &JournalFilters) -> Vec<String>`
  - `pub fn journal_page_argv(unit: Option<&str>, before_cursor: &str, limit: u32, f: &JournalFilters) -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

In `crates/agent/src/logs.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn parse_source_maps_the_system_sentinel_to_journal_all() {
        assert_eq!(parse_source("journal:@system"), Ok(Source::JournalAll));
    }

    #[test]
    fn parse_source_still_treats_a_normal_unit_as_a_unit() {
        // The sentinel must not swallow ordinary units, including template
        // units which legitimately contain '@' after the template name.
        assert_eq!(
            parse_source("journal:nginx.service"),
            Ok(Source::Journal("nginx.service".into()))
        );
        assert_eq!(
            parse_source("journal:systemd-fsck@dev-sda1.service"),
            Ok(Source::Journal("systemd-fsck@dev-sda1.service".into()))
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-agent parse_source_maps_the_system_sentinel`
Expected: FAIL — `no variant named JournalAll found for enum Source`.

- [ ] **Step 3: Add the proto fields**

In `crates/proto/proto/argus.proto`, replace the `LogTailRequest` message with:

```proto
message LogTailRequest {
  string request_id = 1;
  string source = 2;       // "docker:<id>" | "journal:<unit>" | "journal:@system"
  uint32 tail_lines = 3;
  bool follow = 4;
  // Journal pagination: read the page of entries BEFORE this journald cursor.
  // Empty = a normal tail from the end. Only meaningful with follow=false.
  string before_cursor = 5;
  // Severity ceiling in syslog numbering (lower = more severe): becomes
  // `journalctl -p <n>`, returning entries with priority <= n. 0 = unset.
  uint32 max_priority = 6;
  // Absolute unix-ms cutoff, resolved by the server. 0 = unset. NOTE: journalctl
  // rejects --since together with --cursor, so on a page read this is applied as
  // a timestamp cutoff by the agent instead of as an argv flag.
  uint64 since_ms = 7;
  // `journalctl -b` — current boot only. Composes with --cursor.
  bool current_boot = 8;
}
```

- [ ] **Step 4: Add the `JournalAll` variant and sentinel parsing**

In `crates/agent/src/logs.rs`, change the `Source` enum (around line 18) to:

```rust
pub enum Source {
    /// One systemd unit: `journalctl -u <unit>`.
    Journal(String),
    /// The whole system journal: no `-u` at all.
    JournalAll,
    Docker(String),
}
```

In `parse_source`, inside the `"journal" =>` arm, insert the sentinel check **before** the charset check:

```rust
        "journal" => {
            // The whole-journal sentinel. Checked before the unit charset so it
            // is unambiguous: systemd rejects any unit name beginning with '@',
            // so this can never collide with a real unit (template units are
            // `name@instance`, with the '@' after a non-empty template name).
            if target == "@system" {
                return Ok(Source::JournalAll);
            }
            if !target.chars().all(is_unit_char) {
                return Err(SourceError::IllegalCharacter);
            }
            Ok(Source::Journal(target.to_string()))
        }
```

- [ ] **Step 5: Write the failing argv tests**

Add to `mod tests` in `crates/agent/src/logs.rs`:

```rust
    fn no_filters() -> JournalFilters {
        JournalFilters { max_priority: 0, since_ms: 0, current_boot: false }
    }

    #[test]
    fn tail_argv_omits_unit_for_the_whole_journal() {
        let argv = journal_tail_argv(None, 200, true, &no_filters());
        assert!(!argv.iter().any(|a| a == "-u"), "whole journal has no -u");
        assert_eq!(argv, vec!["-n", "200", "-o", "json", "-f"]);
    }

    #[test]
    fn tail_argv_emits_every_filter() {
        let f = JournalFilters { max_priority: 4, since_ms: 1_600_000_000_000, current_boot: true };
        let argv = journal_tail_argv(Some("nginx.service"), 200, false, &f);
        assert_eq!(
            argv,
            vec![
                "-u", "nginx.service", "-n", "200", "-o", "json",
                "-p", "4", "-b", "--since", "@1600000000",
            ]
        );
    }

    #[test]
    fn page_argv_keeps_priority_and_boot_but_never_since() {
        // journalctl rejects --since together with --cursor. -p and -b compose.
        let f = JournalFilters { max_priority: 3, since_ms: 1_600_000_000_000, current_boot: true };
        let argv = journal_page_argv(Some("nginx.service"), "s=abc;i=9", 500, &f);
        assert!(
            !argv.iter().any(|a| a == "--since"),
            "--since must never ride a cursor-anchored read"
        );
        assert!(argv.iter().any(|a| a == "-p"), "priority still applies");
        assert!(argv.iter().any(|a| a == "-b"), "boot still applies");
        assert_eq!(
            argv,
            vec![
                "-u", "nginx.service", "--cursor", "s=abc;i=9", "--reverse",
                "-n", "501", "-o", "json", "-p", "3", "-b",
            ]
        );
    }

    #[test]
    fn zero_valued_filters_emit_nothing() {
        let argv = journal_page_argv(None, "s=abc", 10, &no_filters());
        assert_eq!(
            argv,
            vec!["--cursor", "s=abc", "--reverse", "-n", "11", "-o", "json"]
        );
    }
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test -p argus-agent tail_argv`
Expected: FAIL — `cannot find struct JournalFilters` / `cannot find function journal_tail_argv`.

- [ ] **Step 7: Implement the struct and both builders**

Replace `journal_page_argv` in `crates/agent/src/logs.rs` with:

```rust
/// The journal filters carried on a `LogTailRequest`. Zero means unset for every
/// field, so a default-valued request reproduces the unfiltered behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalFilters {
    /// Severity ceiling in syslog numbering, LOWER IS MORE SEVERE: becomes
    /// `-p <n>`, returning entries with priority <= n. 0 = unset.
    pub max_priority: u32,
    /// Absolute unix-ms cutoff. 0 = unset.
    pub since_ms: u64,
    /// `-b`, current boot only.
    pub current_boot: bool,
}

impl JournalFilters {
    /// The filter flags that are legal on ANY read. `--since` is deliberately
    /// absent: it is rejected alongside `--cursor`, so it is added only by
    /// `journal_tail_argv`, never by `journal_page_argv`.
    fn common_flags(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if self.max_priority > 0 {
            argv.push("-p".into());
            argv.push(self.max_priority.to_string());
        }
        if self.current_boot {
            argv.push("-b".into());
        }
        argv
    }
}

/// The argv for a live/backlog tail: newest `tail_lines` entries, optionally
/// following. `unit` is `None` for the whole system journal (no `-u`).
pub fn journal_tail_argv(
    unit: Option<&str>,
    tail_lines: u32,
    follow: bool,
    f: &JournalFilters,
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(u) = unit {
        argv.push("-u".into());
        argv.push(u.into());
    }
    argv.extend([
        "-n".into(),
        tail_lines.to_string(),
        "-o".into(),
        "json".into(),
    ]);
    if follow {
        argv.push("-f".into());
    }
    argv.extend(f.common_flags());
    // Only a tail may use --since: it is mutually exclusive with --cursor.
    if f.since_ms > 0 {
        argv.push("--since".into());
        argv.push(format!("@{}", f.since_ms / 1000));
    }
    argv
}

/// The argv for a backward page read: the `limit` entries *before*
/// `before_cursor`, newest-first (`finalize_page` re-orders them). `--cursor` is
/// inclusive of the anchor entry and `-n limit+1` fetches it so it can be
/// dropped, so a page never duplicates the boundary line the client already
/// holds. Never follows. `unit` is `None` for the whole system journal.
///
/// Emits `-p`/`-b` but NEVER `--since` — journalctl exits with "Please specify
/// only one of --since=, --cursor=, ..." if both are given. The time window is
/// applied to a page by `finalize_page` instead.
pub fn journal_page_argv(
    unit: Option<&str>,
    before_cursor: &str,
    limit: u32,
    f: &JournalFilters,
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(u) = unit {
        argv.push("-u".into());
        argv.push(u.into());
    }
    argv.extend([
        "--cursor".into(),
        before_cursor.into(),
        "--reverse".into(),
        "-n".into(),
        (limit.saturating_add(1)).to_string(),
        "-o".into(),
        "json".into(),
    ]);
    argv.extend(f.common_flags());
    argv
}
```

Update the existing test `journal_page_argv_reads_backward_from_the_cursor` to call `journal_page_argv(Some("nginx.service"), "s=abc;i=9", 500, &no_filters())`, and `live_journal_page_reads_older_than_a_cursor` to call `journal_page_argv(Some("ssh.service"), &newest_cursor, 10, &no_filters())`.

- [ ] **Step 8: Make `run_tail`/`run_journal` compile against the new signatures**

`run_tail`'s `match source` is now non-exhaustive and `run_journal` must accept `Option<&str>`. Change `run_journal`'s first parameter to `unit: Option<&str>`, and in its body build the tail command from the new builder:

```rust
    let mut cmd = Command::new("journalctl");
    // argv only — nothing is ever interpolated into a shell command line.
    for arg in journal_tail_argv(unit, tail_lines, follow, &JournalFilters::default()) {
        cmd.arg(arg);
    }
```

Have `run_journal_page` take `unit: Option<&str>` too and call `journal_page_argv(unit, before_cursor, limit, &JournalFilters::default())`. In `run_tail`, pass `Some(&unit)` for `Source::Journal(unit)` and add the whole-journal arm:

```rust
        Source::JournalAll => {
            run_journal(
                None, tail_lines, follow, &before_cursor,
                &mut batcher, &out, &request_id, stream_id,
            )
            .await
        }
```

The `JournalFilters::default()` placeholders here are replaced by the real request filters in Task 2 — they exist only so this task compiles and its tests run; no behaviour changes yet.

- [ ] **Step 9: Run the tests**

Run: `cargo test -p argus-agent` then `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: all pass, no warnings.

- [ ] **Step 10: Commit**

```bash
git add crates/proto/proto/argus.proto crates/agent/src/logs.rs
git commit -m "feat(agent): whole-journal source and filter-aware journal argv"
```

---

### Task 2: Cutoff in `finalize_page`, spawn marker, wire filters through

**Files:**
- Modify: `crates/agent/src/logs.rs` (`finalize_page`, `run_tail`, `run_journal`, `run_journal_page`)
- Modify: `crates/agent/src/session.rs` (`LogTailStart` arm)

**Interfaces:**
- Consumes: `JournalFilters`, `journal_tail_argv`, `journal_page_argv` (Task 1).
- Produces: `pub fn finalize_page(records: Vec<LogLine>, before_cursor: &str, since_ms: u64) -> Vec<LogLine>`; `run_tail(source, tail_lines, follow, before_cursor, filters: JournalFilters, docker, out, request_id, stream_id)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn finalize_page_drops_entries_older_than_the_window() {
        // --since cannot ride a cursor read, so the window is enforced here.
        let records = vec![
            line_with_cursor("s=anchor", 300),
            line_with_cursor("s=c", 250),
            line_with_cursor("s=b", 150), // older than the cutoff
            line_with_cursor("s=a", 100), // older than the cutoff
        ];
        let page = finalize_page(records, "s=anchor", 200);
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(ts, vec![250], "only entries at/after the cutoff survive");
    }

    #[test]
    fn finalize_page_with_no_window_keeps_everything() {
        let records = vec![
            line_with_cursor("s=anchor", 300),
            line_with_cursor("s=b", 150),
            line_with_cursor("s=a", 100),
        ];
        let page = finalize_page(records, "s=anchor", 0);
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(ts, vec![100, 150], "since_ms = 0 means unset");
    }
```

Update the two existing `finalize_page` tests to pass `0` as the new third argument.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-agent finalize_page`
Expected: FAIL — `this function takes 2 arguments but 3 arguments were supplied`.

- [ ] **Step 3: Add the cutoff**

Replace `finalize_page` in `crates/agent/src/logs.rs` with:

```rust
/// Turn a raw `--reverse` page (newest-first, starting at the anchor) into the
/// display page: drop the anchor entry the client already has, drop anything
/// older than the window, and re-order oldest-first so lines arrive in reading
/// order. Called by `run_journal_page`.
///
/// The `since_ms` cutoff lives here rather than in the argv because journalctl
/// rejects `--since` alongside `--cursor`. A truncated page comes back short,
/// so the server's existing `reached_start = lines.len() < limit` rule fires
/// unchanged and correctly means "start of the window".
pub fn finalize_page(
    records: Vec<LogLine>,
    before_cursor: &str,
    since_ms: u64,
) -> Vec<LogLine> {
    let mut kept: Vec<LogLine> = records
        .into_iter()
        .filter(|l| l.cursor.as_deref() != Some(before_cursor))
        .filter(|l| since_ms == 0 || l.ts >= since_ms as i64)
        .collect();
    kept.reverse();
    kept
}
```

- [ ] **Step 4: Thread filters through the runners**

In `crates/agent/src/logs.rs`:

`run_tail` — add a `filters: JournalFilters` parameter after `before_cursor` and update the match:

```rust
    let result = match source {
        Source::Journal(unit) => {
            run_journal(
                Some(&unit), tail_lines, follow, &before_cursor, &filters,
                &mut batcher, &out, &request_id, stream_id,
            )
            .await
        }
        Source::JournalAll => {
            run_journal(
                None, tail_lines, follow, &before_cursor, &filters,
                &mut batcher, &out, &request_id, stream_id,
            )
            .await
        }
        Source::Docker(id) => {
            run_docker(
                &docker, &id, tail_lines, follow,
                &mut batcher, &out, &request_id, stream_id,
            )
            .await
        }
    };
```

`run_journal` — add a `filters: &JournalFilters` parameter (the `unit: Option<&str>` change landed in Task 1) and replace the `JournalFilters::default()` placeholders with it, forwarding to the page path:

```rust
    if !before_cursor.is_empty() {
        return run_journal_page(
            unit, before_cursor, tail_lines, filters,
            batcher, out, request_id, stream_id,
        )
        .await;
    }
    let mut cmd = Command::new("journalctl");
    // argv only — nothing is ever interpolated into a shell command line.
    for arg in journal_tail_argv(unit, tail_lines, follow, filters) {
        cmd.arg(arg);
    }
```

`run_journal_page` — take `unit: Option<&str>` and `filters: &JournalFilters`, and pass the cutoff to `finalize_page`:

```rust
    for arg in journal_page_argv(unit, before_cursor, limit, filters) {
        cmd.arg(arg);
    }
```
```rust
    for line in finalize_page(records, before_cursor, filters.since_ms) {
```

Both now exceed clippy's argument threshold; keep the existing `#[allow(clippy::too_many_arguments)]` on each and extend the adjacent comment to say the filter struct was added rather than more scalars.

- [ ] **Step 5: Emit a marker when journalctl cannot be spawned**

In `run_journal` and `run_journal_page`, replace `let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;` with:

```rust
    // A missing journalctl (a non-systemd guest, e.g. Alpine) otherwise fails
    // at spawn and shows the operator an empty view with no explanation.
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            batcher.push(LogLine {
                ts: now_ms(),
                level: Some(3),
                ident: None,
                msg: format!("journalctl could not be started: {e}"),
                marker: true,
                cursor: None,
            });
            flush_ready(batcher, out, request_id, stream_id);
            return Ok(());
        }
    };
```

- [ ] **Step 6: Pass the filters from the session**

In `crates/agent/src/session.rs`, in the `LogTailStart` arm, build the filters from the request and pass them to `run_tail` (after `req.before_cursor`):

```rust
                                    let filters = crate::logs::JournalFilters {
                                        max_priority: req.max_priority,
                                        since_ms: req.since_ms,
                                        current_boot: req.current_boot,
                                    };
```
```rust
                                        crate::logs::run_tail(
                                            source,
                                            req.tail_lines,
                                            req.follow,
                                            req.before_cursor,
                                            filters,
                                            docker,
                                            out,
                                            rid,
                                            stream_id,
                                        )
```

- [ ] **Step 7: Add the live host test**

Add to `mod tests` (name MUST start with `live_` so CI skips it):

```rust
    /// A filtered whole-journal page read against the local journal.
    #[tokio::test]
    #[ignore = "needs a live journal; run under sudo"]
    async fn live_filtered_whole_journal_page_reads_older_than_a_cursor() {
        let f = JournalFilters { max_priority: 6, since_ms: 0, current_boot: true };
        let out = tokio::process::Command::new("journalctl")
            .args(journal_tail_argv(None, 5, false, &f))
            .output()
            .await
            .expect("journalctl");
        let newest_cursor = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(journal_record_to_line)
            .next_back()
            .and_then(|l| l.cursor)
            .expect("a cursor from the whole-journal tail");

        let page_out = tokio::process::Command::new("journalctl")
            .args(journal_page_argv(None, &newest_cursor, 10, &f))
            .output()
            .await
            .expect("journalctl page");
        assert!(page_out.status.success(), "-p and -b must compose with --cursor");
        let records: Vec<LogLine> = String::from_utf8_lossy(&page_out.stdout)
            .lines()
            .filter_map(journal_record_to_line)
            .collect();
        let page = finalize_page(records, &newest_cursor, 0);
        assert!(
            page.iter().all(|l| l.cursor.as_deref() != Some(newest_cursor.as_str())),
            "the anchor is never in its own page"
        );
        assert!(page.iter().all(|l| l.level.is_none_or(|p| p <= 6)), "priority ceiling honoured");
    }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p argus-agent` and `cargo clippy -p argus-agent --all-targets -- -D warnings`
Expected: all pass (the `live_*` test stays ignored), no warnings.

Then on the real host:
```bash
cargo test -p argus-agent --no-run
sudo -n ./target/debug/deps/argus_agent-<hash> --ignored --test-threads=1
```
Expected: all live tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/agent/src/logs.rs crates/agent/src/session.rs
git commit -m "feat(agent): window cutoff on page reads, filters wired through"
```

---

### Task 3: Server — params, validation, passthrough, audit

**Files:**
- Modify: `crates/server/src/hub.rs` (`send_log_start`)
- Modify: `crates/server/src/http.rs` (`LogStreamQuery`, `LogPageQuery`, `log_stream`, `logs_page`)

**Interfaces:**
- Consumes: proto fields (Task 1).
- Produces: `pub struct LogFilters { pub max_priority: u32, pub since_ms: u64, pub current_boot: bool }` in `hub.rs`; `send_log_start(machine_id, request_id, source, tail_lines, follow, before_cursor, filters: LogFilters)`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/server/src/http.rs`:

```rust
    #[sqlx::test]
    async fn logs_page_rejects_an_out_of_range_priority(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-prio', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&priority=9"))
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn log_stream_rejects_an_out_of_range_priority(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('stream-prio', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let (state, _hub) = app_state_with_hub(pool.clone());
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/stream?source=journal:ssh.service&priority=8"))
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    #[sqlx::test]
    async fn logs_page_forwards_filters_and_audits_them(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('page-filt', 'h', 'online') RETURNING id"
        )
        .fetch_one(&pool).await?.id;
        let (state, hub) = app_state_with_hub(pool.clone());
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);
        hub.register(machine_id, tx);
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = agent_rx.recv().await {
                if let Some(server_frame::Payload::LogTailStart(req)) = frame.payload {
                    assert_eq!(req.max_priority, 4, "priority reaches the agent");
                    assert!(req.current_boot, "boot window reaches the agent");
                    assert_eq!(req.since_ms, 0, "boot and since are never both set");
                    hub2.deliver_chunk(&req.request_id, machine_id, argus_proto::v1::LogChunk {
                        request_id: req.request_id, data: Vec::new(), eof: true,
                    });
                }
            }
        });
        let resp = router(state)
            .oneshot(Request::builder()
                .uri(format!("/api/machines/{machine_id}/logs/page?source=journal:ssh.service&before=s%3Dx&priority=4&window=boot"))
                .body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let row = sqlx::query!(
            "SELECT target FROM audit_log WHERE machine_id = $1 AND action = 'logs.page'",
            machine_id,
        ).fetch_one(&pool).await?;
        let target = row.target.unwrap_or_default();
        assert!(target.contains("journal:ssh.service"), "source in the audit target");
        assert!(target.contains("p<=4"), "priority recorded: {target}");
        assert!(target.contains("boot"), "window recorded: {target}");
        Ok(())
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p argus-server logs_page_rejects_an_out_of_range_priority`
Expected: FAIL — returns `200`/`409` rather than `400`, since `priority` is not parsed yet.

- [ ] **Step 3: Add `LogFilters` to the hub**

In `crates/server/src/hub.rs`, above `send_log_start`:

```rust
/// Journal filters carried on a log request. Zero means unset for every field,
/// so a default value reproduces the unfiltered behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogFilters {
    /// Severity ceiling, syslog numbering (lower = more severe). 0 = unset.
    pub max_priority: u32,
    /// Absolute unix-ms cutoff. 0 = unset.
    pub since_ms: u64,
    /// Current boot only.
    pub current_boot: bool,
}
```

Change `send_log_start` to take `filters: LogFilters` as its last parameter and set the three new fields on the `LogTailRequest` literal:

```rust
            payload: Some(server_frame::Payload::LogTailStart(LogTailRequest {
                request_id,
                source,
                tail_lines,
                follow,
                before_cursor,
                max_priority: filters.max_priority,
                since_ms: filters.since_ms,
                current_boot: filters.current_boot,
            })),
```

- [ ] **Step 4: Parse, validate and resolve the window**

In `crates/server/src/http.rs`, add the three params to both query structs:

```rust
struct LogStreamQuery {
    source: String,
    tail: Option<u32>,
    follow: Option<bool>,
    priority: Option<u32>,
    window: Option<String>,
}
```
```rust
struct LogPageQuery {
    source: String,
    before: Option<String>,
    limit: Option<u32>,
    priority: Option<u32>,
    window: Option<String>,
}
```

Add this helper next to `source_is_valid`:

```rust
/// Resolve the UI's single `window` value plus `priority` into concrete filters.
/// `window` is one of `boot | 1h | 24h | all`; `boot` and a relative window are
/// alternative answers to the same question and are never combined. `since_ms`
/// is made ABSOLUTE here so the tail and every later page share one cutoff — a
/// relative window would drift between requests and pages would not line up.
/// Returns `None` when the input is invalid, which the caller turns into a 400.
fn resolve_log_filters(priority: Option<u32>, window: Option<&str>) -> Option<LogFilters> {
    let max_priority = match priority {
        None => 0,
        Some(p) if p <= 7 => p,
        Some(_) => return None,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    let (since_ms, current_boot) = match window.unwrap_or("all") {
        "all" => (0, false),
        "boot" => (0, true),
        "1h" => (now_ms.saturating_sub(3_600_000), false),
        "24h" => (now_ms.saturating_sub(86_400_000), false),
        _ => return None,
    };
    Some(LogFilters { max_priority, since_ms, current_boot })
}

/// The audit `target` for a log read: the source plus whatever narrowed it. A
/// filtered read and a full read are different disclosures, so the trail records
/// what was actually read.
fn audit_target(source: &str, f: &LogFilters) -> String {
    let mut s = source.to_string();
    if f.max_priority > 0 {
        s.push_str(&format!(" p<={}", f.max_priority));
    }
    if f.current_boot {
        s.push_str(" boot");
    } else if f.since_ms > 0 {
        s.push_str(&format!(" since={}", f.since_ms));
    }
    s
}
```

Import `LogFilters` alongside the existing hub imports.

In **`log_stream`**, after the existing source validation:

```rust
    let Some(filters) = resolve_log_filters(q.priority, q.window.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "invalid priority or window").into_response();
    };
```
pass `filters` as the last argument to `send_log_start`, and change the audit call's target argument from `&q.source` to `&audit_target(&q.source, &filters)`.

In **`logs_page`**, add the same `resolve_log_filters` guard after the `before` check, pass `filters` to `send_log_start`, and use `&audit_target(&q.source, &filters)` as the audit target. Leave the dispatch-then-audit-then-collect ordering exactly as it is.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p argus-server` then `cargo clippy -p argus-server --all-targets -- -D warnings`
Expected: all pass, no warnings. If `.sqlx` complains, run `cargo sqlx prepare --workspace -- --all-targets`.

- [ ] **Step 6: Commit**

```bash
git add crates/server/src/hub.rs crates/server/src/http.rs .sqlx
git commit -m "feat(server): journal filter params, validation and audit target"
```

---

### Task 4: Frontend API + `LogFilterBar`

**Files:**
- Modify: `frontend/src/api.ts`
- Create: `frontend/src/components/LogFilterBar.tsx`

**Interfaces:**
- Produces:
  - `export type LogWindow = "boot" | "1h" | "24h" | "all";`
  - `export type LogFilters = { priority: number; window: LogWindow };`
  - `export const ALL_LOGS: LogFilters` and `export const BOOT_LOGS: LogFilters`
  - `logStreamUrl(id, source, filters, tail?, follow?)`, `fetchLogPage(id, source, before, filters, limit?)`
  - `<LogFilterBar value={filters} onChange={(f: LogFilters) => void} />`

- [ ] **Step 1: Add the types and thread filters into both URLs**

In `frontend/src/api.ts`, add above `logStreamUrl`:

```ts
/** Time window for a journal read. Maps to journalctl `-b` / `--since`. */
export type LogWindow = "boot" | "1h" | "24h" | "all";

/** `priority` is a severity ceiling (lower = more severe); 0 means no filter. */
export type LogFilters = { priority: number; window: LogWindow };

/** Per-unit default: unfiltered, so behaviour is unchanged from before filters. */
export const ALL_LOGS: LogFilters = { priority: 0, window: "all" };
/** Whole-journal default: current boot, the cheapest and most relevant read. */
export const BOOT_LOGS: LogFilters = { priority: 0, window: "boot" };

/** The whole system journal — no `-u`. */
export const SYSTEM_JOURNAL = "journal:@system";

/** Shared query params for both log endpoints. */
function filterParams(f: LogFilters): Record<string, string> {
  const p: Record<string, string> = { window: f.window };
  if (f.priority > 0) p.priority = String(f.priority);
  return p;
}
```

Change `logStreamUrl` to accept filters and merge them:

```ts
export function logStreamUrl(
  id: string,
  source: LogSource,
  filters: LogFilters = ALL_LOGS,
  tail = 200,
  follow = true,
): string {
  const params = new URLSearchParams({
    source,
    tail: String(tail),
    follow: String(follow),
    ...filterParams(filters),
  });
  return `/api/machines/${id}/logs/stream?${params.toString()}`;
}
```

Change `fetchLogPage` the same way:

```ts
export async function fetchLogPage(
  id: string,
  source: string,
  before: string,
  filters: LogFilters = ALL_LOGS,
  limit = 500,
): Promise<LogPage> {
  const params = new URLSearchParams({
    source,
    before,
    limit: String(limit),
    ...filterParams(filters),
  });
  const r = await fetch(`/api/machines/${id}/logs/page?${params.toString()}`);
  if (!r.ok) throw new Error(`log page ${r.status}`);
  return (await r.json()) as LogPage;
}
```

Verify the existing `follow` line in `logStreamUrl` is not duplicated — it was already there; keep exactly one.

- [ ] **Step 2: Create the filter bar**

Create `frontend/src/components/LogFilterBar.tsx`:

```tsx
// Priority + time-window controls, shared by the Logs tab and the per-unit
// dialog so there is one control and one code path for both journal surfaces.
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@e412/rnui-react";
import type { LogFilters, LogWindow } from "../api";

/** Syslog severities. Lower is MORE severe: `-p 4` returns 4,3,2,1,0. */
const PRIORITIES: { value: string; label: string }[] = [
  { value: "0", label: "all severities" },
  { value: "3", label: "err and worse" },
  { value: "4", label: "warning and worse" },
  { value: "5", label: "notice and worse" },
  { value: "6", label: "info and worse" },
];

const WINDOWS: { value: LogWindow; label: string }[] = [
  { value: "boot", label: "current boot" },
  { value: "1h", label: "last hour" },
  { value: "24h", label: "last 24h" },
  { value: "all", label: "all history" },
];

export default function LogFilterBar({
  value,
  onChange,
}: {
  value: LogFilters;
  onChange: (next: LogFilters) => void;
}) {
  return (
    <div className="flex items-center gap-2 pb-2">
      <Select
        value={String(value.priority)}
        onValueChange={(v: string | null) =>
          onChange({ ...value, priority: Number(v ?? "0") })
        }
      >
        <SelectTrigger size="sm" className="w-48 font-mono text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {PRIORITIES.map((p) => (
            <SelectItem key={p.value} value={p.value}>
              {p.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      <Select
        value={value.window}
        onValueChange={(v: string | null) =>
          onChange({ ...value, window: (v ?? "all") as LogWindow })
        }
      >
        <SelectTrigger size="sm" className="w-40 font-mono text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {WINDOWS.map((w) => (
            <SelectItem key={w.value} value={w.value}>
              {w.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
```

- [ ] **Step 3: Typecheck and build**

Run: `cd frontend && node_modules/.bin/tsc --noEmit && npm run build`
Expected: exit 0, build succeeds.

- [ ] **Step 4: Commit**

```bash
git add frontend/src/api.ts frontend/src/components/LogFilterBar.tsx
git commit -m "feat(frontend): log filter types, URL params and filter bar"
```

---

### Task 5: `LogViewer` honours filters

**Files:**
- Modify: `frontend/src/components/LogViewer.tsx`

**Interfaces:**
- Consumes: `LogFilters`, `ALL_LOGS`, `logStreamUrl`, `fetchLogPage` (Task 4).
- Produces: `<LogViewer machineId source filters height? />`.

- [ ] **Step 1: Accept filters and rebuild the stream when they change**

In `frontend/src/components/LogViewer.tsx`, import `type LogFilters` and `ALL_LOGS` from `../api`, and add `filters` to the props:

```tsx
export default function LogViewer({
  machineId,
  source,
  filters = ALL_LOGS,
  height,
}: {
  machineId: string;
  source: string;
  filters?: LogFilters;
  height?: number;
}) {
```

Pass the filters into the EventSource URL and add them to the effect's dependencies, so a filter change tears the stream down and resets the buffer exactly the way a source change already does:

```tsx
    const es = new EventSource(logStreamUrl(machineId, source, filters));
```
```tsx
  }, [machineId, source, filters.priority, filters.window]);
```

Depend on the two **primitive fields**, not the `filters` object — a caller that rebuilds the object each render would otherwise reopen the stream on every render.

- [ ] **Step 2: Pass filters to the page fetch**

In `loadOlder`, change the fetch call so pages are filtered identically to the tail:

```tsx
      const page = await fetchLogPage(machineId, source, oldest, filters);
```

- [ ] **Step 3: Make the end-of-history wording window-aware**

Replace the status-row expression:

```tsx
          {reachedStart
            ? filters.window === "all"
              ? "— beginning of journal —"
              : "— beginning of window —"
            : loadingOlder
              ? "loading older…"
              : "scroll up to load older"}
```

- [ ] **Step 4: Typecheck and build**

Run: `cd frontend && node_modules/.bin/tsc --noEmit && npm run build`
Expected: exit 0, build succeeds.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/components/LogViewer.tsx
git commit -m "feat(frontend): log viewer honours priority and window filters"
```

---

### Task 6: Logs tab + dialog filters + URL state

**Files:**
- Modify: `frontend/src/api.ts` (adds `filtersFromParams`)
- Modify: `frontend/src/pages/MachineDetailPage.tsx`
- Modify: `frontend/src/components/LogDialog.tsx`

**Interfaces:**
- Consumes: `LogFilterBar` (Task 4), `LogViewer` with `filters` (Task 5), `SYSTEM_JOURNAL`, `ALL_LOGS`, `BOOT_LOGS`.

- [ ] **Step 1: Add a shared URL-state helper**

In `frontend/src/api.ts`, add:

```ts
/**
 * Read filters out of the URL, falling back to `fallback` for anything missing
 * or invalid — the same forgiving guard `?tab=typo` gets, so a bad link renders
 * the default view instead of nothing.
 */
export function filtersFromParams(
  params: URLSearchParams,
  fallback: LogFilters,
): LogFilters {
  const rawP = Number(params.get("priority"));
  const priority = Number.isInteger(rawP) && rawP >= 0 && rawP <= 7 ? rawP : fallback.priority;
  const rawW = params.get("window");
  const window: LogWindow =
    rawW === "boot" || rawW === "1h" || rawW === "24h" || rawW === "all"
      ? rawW
      : fallback.window;
  return { priority, window };
}
```

- [ ] **Step 2: Add the Logs tab**

In `frontend/src/pages/MachineDetailPage.tsx`, extend `TABS`:

```tsx
const TABS: { key: string; label: string }[] = [
  { key: "overview", label: "Overview" },
  { key: "containers", label: "Containers" },
  { key: "units", label: "Units" },
  { key: "logs", label: "Logs" },
];
```

Import the pieces:

```tsx
import LogFilterBar from "../components/LogFilterBar";
import LogViewer from "../components/LogViewer";
import { BOOT_LOGS, SYSTEM_JOURNAL, filtersFromParams } from "../api";
import type { LogFilters } from "../api";
```

Derive the filters from the URL and render the panel alongside the other `tab === …` blocks:

```tsx
  const logFilters = filtersFromParams(searchParams, BOOT_LOGS);
  function setLogFilters(next: LogFilters) {
    const p = new URLSearchParams(searchParams);
    p.set("window", next.window);
    if (next.priority > 0) p.set("priority", String(next.priority));
    else p.delete("priority");
    setSearchParams(p, { replace: true });
  }
```
```tsx
      {tab === "logs" && (
        <section
          role="tabpanel"
          id="panel-logs"
          aria-labelledby="tab-logs"
          className="flex h-[70vh] min-h-0 flex-col"
        >
          <LogFilterBar value={logFilters} onChange={setLogFilters} />
          <div className="min-h-0 flex-1">
            <LogViewer
              machineId={id}
              source={SYSTEM_JOURNAL}
              filters={logFilters}
            />
          </div>
        </section>
      )}
```

If `searchParams`/`setSearchParams` are not already in scope in this component, obtain them from the existing `useSearchParams()` call that backs `?tab=`.

- [ ] **Step 3: Add the filter bar to the per-unit dialog**

In `frontend/src/components/LogDialog.tsx`, default to `ALL_LOGS` so per-unit behaviour is unchanged, and drive the same URL params:

```tsx
import { ALL_LOGS, filtersFromParams } from "../api";
import type { LogFilters } from "../api";
import LogFilterBar from "./LogFilterBar";
```
```tsx
  const filters = filtersFromParams(searchParams, ALL_LOGS);
  function setFilters(next: LogFilters) {
    const p = new URLSearchParams(searchParams);
    p.set("window", next.window);
    if (next.priority > 0) p.set("priority", String(next.priority));
    else p.delete("priority");
    setSearchParams(p, { replace: true });
  }
```

Render the bar above the viewer and pass the filters down, but **only for journal sources** — docker has no priority or window:

```tsx
        <div className="flex min-h-0 flex-1 flex-col p-4">
          {source?.startsWith("journal:") && (
            <LogFilterBar value={filters} onChange={setFilters} />
          )}
          {open && id !== undefined && source !== null && (
            <div className="min-h-0 flex-1">
              <LogViewer machineId={id} source={source} filters={filters} />
            </div>
          )}
        </div>
```

Also clear `priority`/`window` in the existing `close()` so a closed dialog leaves no stray filter params:

```tsx
    next.delete("logs");
    next.delete("priority");
    next.delete("window");
```

- [ ] **Step 4: Typecheck and build**

Run: `cd frontend && node_modules/.bin/tsc --noEmit && npm run build`
Expected: exit 0, build succeeds.

- [ ] **Step 5: Commit**

```bash
git add frontend/src/pages/MachineDetailPage.tsx frontend/src/components/LogDialog.tsx frontend/src/api.ts
git commit -m "feat(frontend): system journal Logs tab with URL-backed filters"
```

---

### Task 7: Verification, DEV.md, PR (controller-run)

**Files:**
- Modify: `docs/DEV.md`

- [ ] **Step 1: Full static gates**

```bash
npm --prefix frontend run build
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace -- --ignored --skip live_
```
Expected: all clean.

- [ ] **Step 2: Live host tests**

```bash
cargo test -p argus-agent --no-run
sudo -n ./target/debug/deps/argus_agent-<hash> --ignored --test-threads=1
```
Expected: all `live_*` tests pass.

- [ ] **Step 3: Live endpoint E2E**

With the control plane and a root agent running, confirm each of:
- `GET …/logs/page?source=journal:@system&before=<cursor>&window=boot&priority=4` returns only entries with `level <= 4`.
- The same request with `window=1h` returns a page whose oldest entry is within the last hour, and `reached_start:true` once the window is exhausted.
- `priority=9` → **400**; `window=nonsense` → **400**.
- `audit_log` rows for `logs.page` carry the filters in `target` (e.g. `journal:@system p<=4 boot`).
- With the agent stopped, the same request → **409** and **no** new `logs.page` row.

- [ ] **Step 4: Browser pass**

- Logs tab streams the whole journal and defaults to current boot.
- Changing priority or window resets the buffer and re-streams.
- Scroll up loads older pages; at the window edge the row reads "— beginning of window —", and with `all` it reads "— beginning of journal —".
- The per-unit dialog still defaults to unfiltered and can page back past a reboot.
- A docker source shows **no** filter bar.

- [ ] **Step 5: Record in DEV.md and commit**

Add a "Full system journal + log filters" section to `docs/DEV.md` recording the checks above and the `--since`/`--cursor` constraint.

```bash
git add docs/DEV.md
git commit -m "docs: record full-journal manual verification"
```

- [ ] **Step 6: Open the PR**

```bash
fj pr create "feat(logs): full system journal with priority and time filters" \
  --base main --head full-journal-slice --body-file <file>
```
