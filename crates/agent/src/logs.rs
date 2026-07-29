//! Log tailing: journal via a `journalctl` subprocess, Docker via bollard.

use argus_proto::v1::{agent_frame, AgentFrame, LogChunk};
use serde::Serialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Where a tail reads from (parsed from the browser-supplied wire `source`
/// string; see `parse_source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// One systemd unit: `journalctl -u <unit>`.
    Journal(String),
    /// The whole system journal: no `-u` at all.
    JournalAll,
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
    /// journald's opaque `__CURSOR`, the paging anchor. `None` for docker
    /// lines and markers (never a paging anchor); omitted from the wire
    /// when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

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

/// Parses the wire `source` string into a typed target. The server
/// validates too, on purpose -- the agent must not depend on a caller
/// having sanitised input that becomes a subprocess argument.
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
            // Matched HERE, before the unit-charset check, so it doesn't
            // depend on systemd's naming rules: a real unit always carries a
            // `.<type>` suffix, so "@system" (no suffix) can never collide.
            if target == "@system" {
                return Ok(Source::JournalAll);
            }
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
        cursor: None,
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
        cursor: None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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

    pub fn take_if_ready(&mut self, now_ms: i64) -> Option<Vec<u8>> {
        // A backwards NTP step makes `now_ms < last_flush_ms`, so the
        // saturating subtraction never clears the `>=` threshold and
        // flushing would stay blocked -- treat a detected clock-step as due
        // explicitly.
        let elapsed = now_ms.saturating_sub(self.last_flush_ms);
        let due = elapsed >= FLUSH_INTERVAL.as_millis() as i64 || now_ms < self.last_flush_ms;
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

/// Runs one tail to completion, emitting `LogChunk` frames until the source
/// ends or the task is cancelled (aborted); the journal child dies on drop
/// via `kill_on_drop`.
///
/// 9 params, `#[allow(too_many_arguments)]`: a context struct would obscure
/// the straight-line handoff to `run_journal`/`run_docker` for no benefit.
#[allow(clippy::too_many_arguments)]
pub async fn run_tail(
    source: Source,
    tail_lines: u32,
    follow: bool,
    before_cursor: String,
    filters: JournalFilters,
    docker: crate::docker::DockerClient,
    out: mpsc::Sender<AgentFrame>,
    request_id: String,
    stream_id: u64,
) {
    // The server clamps too, but journalctl parses `--lines` as an `int`, so
    // an unclamped large `u32` would be a hard parse error there, not a
    // graceful clamp.
    let tail_lines = tail_lines.clamp(1, 1000);
    let mut batcher = Batcher::new(now_ms());
    let result = match source {
        Source::Journal(unit) => {
            run_journal(
                Some(&unit),
                tail_lines,
                follow,
                &before_cursor,
                &filters,
                &mut batcher,
                &out,
                &request_id,
                stream_id,
            )
            .await
        }
        Source::JournalAll => {
            run_journal(
                None,
                tail_lines,
                follow,
                &before_cursor,
                &filters,
                &mut batcher,
                &out,
                &request_id,
                stream_id,
            )
            .await
        }
        Source::Docker(id) => {
            run_docker(
                &docker,
                &id,
                tail_lines,
                follow,
                &mut batcher,
                &out,
                &request_id,
                stream_id,
            )
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
            cursor: None,
        };
        batcher.push(err);
    }
    // Final flush + EOF, retried unlike every other chunk: without an EOF
    // the browser's stream never closes. Sleeping (not awaiting a permit)
    // keeps this behind heartbeats rather than ahead of them.
    let mut data = batcher.take(now_ms()).unwrap_or_default();
    for attempt in 0..20 {
        if try_emit(
            &out,
            &request_id,
            stream_id,
            std::mem::take(&mut data),
            true,
        ) {
            return;
        }
        if attempt == 0 {
            tracing::debug!(request_id = %request_id, "log tail: session congested, retrying eof");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::warn!(request_id = %request_id, "log tail: gave up sending eof");
}

/// A missing journalctl (non-systemd guest, e.g. Alpine) fails at spawn.
/// Gives that a specific marker instead of `run_tail`'s generic wrapper
/// text; built once here since both `run_journal` and `run_journal_page`
/// need the identical line.
fn spawn_failure_marker(e: &std::io::Error) -> LogLine {
    LogLine {
        ts: now_ms(),
        level: Some(3),
        ident: None,
        msg: format!("journalctl could not be started: {e}"),
        marker: true,
        cursor: None,
    }
}

// Same 9-param shape as run_tail, same reason (see its doc).
#[allow(clippy::too_many_arguments)]
async fn run_journal(
    unit: Option<&str>,
    tail_lines: u32,
    follow: bool,
    before_cursor: &str,
    filters: &JournalFilters,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    if !before_cursor.is_empty() {
        return run_journal_page(
            unit,
            before_cursor,
            tail_lines,
            filters,
            batcher,
            out,
            request_id,
            stream_id,
        )
        .await;
    }
    let mut cmd = Command::new("journalctl");
    // argv only — nothing is ever interpolated into a shell command line.
    for arg in journal_tail_argv(unit, tail_lines, follow, filters) {
        cmd.arg(arg);
    }
    // Without this an aborted task would leave `journalctl -f` running forever.
    cmd.kill_on_drop(true);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            batcher.push(spawn_failure_marker(&e));
            flush_ready(batcher, out, request_id, stream_id);
            return Ok(());
        }
    };
    let stdout = child.stdout.take().expect("stdout piped above");
    let mut lines = BufReader::new(stdout).lines();

    // The ticker is what makes an idle tail flush: `journalctl -f` dumps its
    // backlog then blocks in follow mode for hours. Without this independent
    // timer a quiet unit's backlog sits buffered until it logs again -- do
    // NOT "simplify" this back to a flush at the loop's bottom.
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            next = lines.next_line() => match next? {
                Some(raw) => {
                    if let Some(line) = journal_record_to_line(&raw) {
                        batcher.push(line);
                    }
                }
                None => break,
            },
            _ = ticker.tick() => {}
        }
        flush_ready(batcher, out, request_id, stream_id);
    }

    // A non-zero exit (EACCES, an out-of-range `-n`, a rejected unit)
    // otherwise looks identical to a quiet, healthy unit: closed stream,
    // clean EOF, no diagnostic. Surface it as a visible marker instead.
    let status = child.wait().await?;
    if !status.success() {
        batcher.push(LogLine {
            ts: now_ms(),
            level: Some(3),
            ident: None,
            msg: format!("journalctl exited with {status}"),
            marker: true,
            cursor: None,
        });
    }
    Ok(())
}

/// A one-shot backward page read: spawn `journalctl --cursor --reverse`,
/// collect the bounded page, drop the anchor, reorder oldest-first, push to
/// the batcher. No follow/ticker -- the process exits on its own.
#[allow(clippy::too_many_arguments)]
async fn run_journal_page(
    unit: Option<&str>,
    before_cursor: &str,
    limit: u32,
    filters: &JournalFilters,
    batcher: &mut Batcher,
    out: &mpsc::Sender<AgentFrame>,
    request_id: &str,
    stream_id: u64,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("journalctl");
    for arg in journal_page_argv(unit, before_cursor, limit, filters) {
        cmd.arg(arg);
    }
    cmd.kill_on_drop(true);
    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        Ok(c) => c,
        Err(e) => {
            batcher.push(spawn_failure_marker(&e));
            flush_ready(batcher, out, request_id, stream_id);
            return Ok(());
        }
    };
    let stdout = child.stdout.take().expect("stdout piped above");
    let mut lines = BufReader::new(stdout).lines();

    let mut records = Vec::new();
    while let Some(raw) = lines.next_line().await? {
        if let Some(line) = journal_record_to_line(&raw) {
            records.push(line);
        }
    }
    for line in finalize_page(records, before_cursor, filters.since_ms) {
        batcher.push(line);
        flush_ready(batcher, out, request_id, stream_id);
    }

    // A non-zero exit here (rejected cursor, EACCES, out-of-range `-n`)
    // yields an empty page the server reports as `reached_start` --
    // indistinguishable from the real start of the journal. Surface a marker.
    let status = child.wait().await?;
    if !status.success() {
        batcher.push(LogLine {
            ts: now_ms(),
            level: Some(3),
            ident: None,
            msg: format!("journalctl exited with {status}"),
            marker: true,
            cursor: None,
        });
        flush_ready(batcher, out, request_id, stream_id);
    }
    Ok(())
}

// One more param than run_journal (needs the `DockerClient` handle too);
// same "no benefit to a context struct" reasoning as run_tail's doc.
#[allow(clippy::too_many_arguments)]
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

    // Mirrors run_journal's ticker: dockerd blocks between writes in follow
    // mode too, so without this timer a quiet container's backlog sits
    // buffered until it logs again -- do NOT "simplify" back to a plain
    // `stream.next()` loop.
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(item) => {
                    let raw = item?;
                    // A bollard item is a docker *frame*, not a line: a TTY
                    // container can stream a chunk holding several log
                    // lines, so split before batching each as its own LogLine.
                    for seg in raw.split_inclusive('\n') {
                        batcher.push(docker_line(seg, id));
                    }
                }
                None => break,
            },
            _ = ticker.tick() => {}
        }
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
    /// The filter flags legal on ANY read. `--since` is deliberately absent
    /// here -- it's rejected alongside `--cursor`, so only `journal_tail_argv`
    /// adds it.
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

/// The argv for a backward page read: `limit` entries *before*
/// `before_cursor`, newest-first (`finalize_page` reorders). `--cursor` is
/// inclusive of the anchor, so `-n limit+1` fetches it just to drop it.
///
/// Never `--since` -- journalctl rejects combining it with `--cursor`;
/// `finalize_page` applies the time window instead.
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

/// `--since @<epoch>` only has second resolution (`since_ms / 1000`
/// truncates); this must floor to the same second, or a page read becomes
/// up to 999ms stricter than the tail that produced its cursor, falsely
/// firing `reached_start` early.
fn floor_to_epoch_second(since_ms: u64) -> i64 {
    ((since_ms / 1000) * 1000) as i64
}

/// Turns a raw `--reverse` page (newest-first, from the anchor) into the
/// display page: drops the anchor, drops everything from the first
/// `since_ms` crossing onward, reorders oldest-first.
///
/// `take_while`, not `filter`: dropped entries must be a structural SUFFIX
/// (the server's `reached_start` rule assumes that), and timestamps aren't
/// guaranteed monotonic within a page -- a per-record `filter` could excise
/// one anomaly and silently resume past it, falsely latching `reached_start`.
pub fn finalize_page(records: Vec<LogLine>, before_cursor: &str, since_ms: u64) -> Vec<LogLine> {
    let mut kept: Vec<LogLine> = records
        .into_iter()
        .filter(|l| l.cursor.as_deref() != Some(before_cursor))
        .take_while(|l| since_ms == 0 || l.ts >= floor_to_epoch_second(since_ms))
        .collect();
    kept.reverse();
    kept
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
        assert_eq!(
            parse_source("nginx.service"),
            Err(SourceError::UnknownScheme)
        );
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
    fn spawn_failure_marker_surfaces_a_visible_marker_with_the_error_text() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        let line = spawn_failure_marker(&err);
        assert!(line.marker, "spawn failure must be a marker line");
        assert_eq!(line.cursor, None, "a marker is never a paging anchor");
        assert_eq!(line.level, Some(3));
        assert!(
            line.msg.contains("No such file or directory"),
            "message must surface the underlying error: {}",
            line.msg
        );
        assert_eq!(
            line.msg,
            "journalctl could not be started: No such file or directory"
        );
    }

    #[test]
    fn ndjson_round_trips_and_omits_marker_when_false() {
        let line = LogLine {
            ts: 42,
            level: Some(6),
            ident: Some("nginx".into()),
            msg: "up".into(),
            marker: false,
            cursor: None,
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
            cursor: None,
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

    fn line(msg: &str) -> LogLine {
        LogLine {
            ts: 1,
            level: Some(6),
            ident: None,
            msg: msg.to_string(),
            marker: false,
            cursor: None,
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
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"ts\":1,\"level\":6,\"ident\":null,\"msg\":\"a\"}\n"
        );
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
        assert!(
            !out2.contains("dropped"),
            "gap must be reported once: {out2}"
        );
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
        assert!(
            marker_at < after_at,
            "marker must precede the resumed lines"
        );
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

    /// A tail on a QUIET unit must still deliver its backlog promptly, not
    /// only when the next line arrives -- reverting the ticker makes this
    /// hang until timeout.
    #[tokio::test]
    #[ignore = "spawns journalctl; needs a live journal, run under sudo"]
    async fn live_idle_tail_flushes_its_backlog_without_waiting_for_a_new_line() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let docker = crate::docker::DockerClient::connect();
        let task = tokio::spawn(async move {
            run_tail(
                Source::Journal("systemd-journald.service".into()),
                20,
                true, // follow: the unit is quiet, so nothing new will arrive
                String::new(),
                JournalFilters::default(),
                docker,
                tx,
                "req-idle".into(),
                7,
            )
            .await;
        });
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a quiet tail must still flush its backlog");
        assert!(got.is_some(), "expected a LogChunk frame");
        task.abort();
    }

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
        assert!(
            !line_to_ndjson(&bare).contains("cursor"),
            "docker/marker lines stay cursor-free"
        );

        let with = LogLine {
            cursor: Some("s=abc".into()),
            ..bare
        };
        let s = line_to_ndjson(&with);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cursor"], "s=abc");
    }

    fn no_filters() -> JournalFilters {
        JournalFilters {
            max_priority: 0,
            since_ms: 0,
            current_boot: false,
        }
    }

    #[test]
    fn tail_argv_omits_unit_for_the_whole_journal() {
        let argv = journal_tail_argv(None, 200, true, &no_filters());
        assert!(!argv.iter().any(|a| a == "-u"), "whole journal has no -u");
        assert_eq!(argv, vec!["-n", "200", "-o", "json", "-f"]);
    }

    #[test]
    fn tail_argv_emits_every_filter() {
        let f = JournalFilters {
            max_priority: 4,
            since_ms: 1_600_000_000_000,
            current_boot: true,
        };
        let argv = journal_tail_argv(Some("nginx.service"), 200, false, &f);
        assert_eq!(
            argv,
            vec![
                "-u",
                "nginx.service",
                "-n",
                "200",
                "-o",
                "json",
                "-p",
                "4",
                "-b",
                "--since",
                "@1600000000",
            ]
        );
    }

    #[test]
    fn page_argv_keeps_priority_and_boot_but_never_since() {
        // journalctl rejects --since together with --cursor. -p and -b compose.
        let f = JournalFilters {
            max_priority: 3,
            since_ms: 1_600_000_000_000,
            current_boot: true,
        };
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
                "-u",
                "nginx.service",
                "--cursor",
                "s=abc;i=9",
                "--reverse",
                "-n",
                "501",
                "-o",
                "json",
                "-p",
                "3",
                "-b",
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

    #[test]
    fn journal_page_argv_reads_backward_from_the_cursor() {
        let argv = journal_page_argv(Some("nginx.service"), "s=abc;i=9", 500, &no_filters());
        // -u <unit> --cursor <c> --reverse -n <limit+1> -o json ; no -f
        assert_eq!(
            argv,
            vec![
                "-u",
                "nginx.service",
                "--cursor",
                "s=abc;i=9",
                "--reverse",
                "-n",
                "501",
                "-o",
                "json",
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
        let records = vec![
            line_with_cursor("s=anchor", 30),
            line_with_cursor("s=b", 20),
            line_with_cursor("s=a", 10),
        ];
        let page = finalize_page(records, "s=anchor", 0);
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
        assert!(finalize_page(records, "s=anchor", 0).is_empty());
    }

    #[test]
    fn finalize_page_drops_entries_older_than_the_window() {
        // --since cannot ride a cursor read, so the window is enforced here.
        // Values are whole seconds apart so the floor-to-the-second cutoff
        // doesn't collapse them into the same bucket.
        let records = vec![
            line_with_cursor("s=anchor", 1_600_000_300_000),
            line_with_cursor("s=c", 1_600_000_250_000),
            line_with_cursor("s=b", 1_600_000_150_000), // older than the cutoff
            line_with_cursor("s=a", 1_600_000_100_000), // older than the cutoff
        ];
        let page = finalize_page(records, "s=anchor", 1_600_000_200_000);
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(
            ts,
            vec![1_600_000_250_000],
            "only entries at/after the cutoff survive"
        );
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

    #[test]
    fn finalize_page_a_non_monotonic_dip_forms_a_clean_structural_cutoff() {
        // journalctl --reverse walks JOURNAL SEQUENCE order, not timestamp
        // order, so a clock step can put an anomalously-low timestamp
        // mid-page. `take_while` must cut there and stay cut, not resume
        // past it.
        let records = vec![
            line_with_cursor("s=anchor", 1_600_000_500_000),
            line_with_cursor("s=d", 1_600_000_400_000),
            line_with_cursor("s=c", 1_600_000_050_000), // backward clock step
            line_with_cursor("s=b", 1_600_000_300_000), // individually still >= cutoff
            line_with_cursor("s=a", 1_600_000_200_000), // individually still >= cutoff
        ];
        let page = finalize_page(records, "s=anchor", 1_600_000_100_000);
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(
            ts,
            vec![1_600_000_400_000],
            "the cutoff fires once, at the first crossing, and holds for the rest of the page"
        );
    }

    #[test]
    fn finalize_page_floors_the_cutoff_to_the_second_like_the_tail_does() {
        // Mirrors floor_to_epoch_second's rationale: the page cutoff must
        // floor to the same second as the tail's `--since @<epoch>`.
        let since_ms = 1_600_000_000_500; // 500ms into the second
        let records = vec![
            line_with_cursor("s=anchor", 1_600_000_001_000),
            // Before `since_ms` in exact-ms terms, but within the same
            // (floored) second the tail's `--since @1600000000` would include.
            line_with_cursor("s=b", 1_600_000_000_300),
            // A full second earlier: outside the window under either scheme.
            line_with_cursor("s=a", 1_599_999_999_500),
        ];
        let page = finalize_page(records, "s=anchor", since_ms);
        let ts: Vec<i64> = page.iter().map(|l| l.ts).collect();
        assert_eq!(
            ts,
            vec![1_600_000_000_300],
            "a record within the floored second must survive even though its raw ms value is before since_ms"
        );
    }

    #[tokio::test]
    #[ignore = "needs a live journal; run under sudo"]
    async fn live_journal_page_reads_older_than_a_cursor() {
        // Get a recent cursor from a normal tail first.
        let out = tokio::process::Command::new("journalctl")
            .args([
                "-u",
                "ssh.service",
                "-n",
                "5",
                "-o",
                "json",
                "--show-cursor",
            ])
            .output()
            .await
            .expect("journalctl");
        let text = String::from_utf8_lossy(&out.stdout);
        let newest_cursor = text
            .lines()
            .filter_map(journal_record_to_line)
            .next_back()
            .and_then(|l| l.cursor)
            .expect("a cursor from the tail");

        // Now read the page before it.
        let argv = journal_page_argv(Some("ssh.service"), &newest_cursor, 10, &no_filters());
        let page_out = tokio::process::Command::new("journalctl")
            .args(&argv)
            .output()
            .await
            .expect("journalctl page");
        let records: Vec<LogLine> = String::from_utf8_lossy(&page_out.stdout)
            .lines()
            .filter_map(journal_record_to_line)
            .collect();
        let page = finalize_page(records, &newest_cursor, 0);
        assert!(
            page.iter()
                .all(|l| l.cursor.as_deref() != Some(newest_cursor.as_str())),
            "the anchor is never in its own page"
        );
        // Page is oldest-first.
        if page.len() >= 2 {
            assert!(page[0].ts <= page[page.len() - 1].ts, "chronological order");
        }
    }

    #[tokio::test]
    #[ignore = "needs a live journal; run under sudo"]
    async fn live_filtered_whole_journal_page_reads_older_than_a_cursor() {
        let f = JournalFilters {
            max_priority: 6,
            since_ms: 0,
            current_boot: true,
        };
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
        assert!(
            page_out.status.success(),
            "-p and -b must compose with --cursor"
        );
        let records: Vec<LogLine> = String::from_utf8_lossy(&page_out.stdout)
            .lines()
            .filter_map(journal_record_to_line)
            .collect();
        let page = finalize_page(records, &newest_cursor, 0);
        assert!(
            page.iter()
                .all(|l| l.cursor.as_deref() != Some(newest_cursor.as_str())),
            "the anchor is never in its own page"
        );
        assert!(
            page.iter().all(|l| l.level.is_none_or(|p| p <= 6)),
            "priority ceiling honoured"
        );
    }
}
