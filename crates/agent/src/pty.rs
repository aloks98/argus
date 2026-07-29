//! An interactive PTY for one terminal session.
//!
//! Both directions run on dedicated OS threads: `portable-pty`'s reader and
//! writer are blocking. Reads: the thread parks BEFORE each `read()` on
//! `PtyFlow{paused}`, so the kernel buffer fills and the writer blocks --
//! real backpressure to the source.
//!
//! Writes: the tty's canonical queue is only ~4 KB, so `write_all` can
//! block behind a wedged program. Doing that inline on the async dispatch
//! loop would stall every other frame while holding a mutex, so
//! `write_input` only pushes onto an unbounded, order-preserving
//! `std::sync::mpsc` channel (O(1), never blocks); a dedicated writer
//! thread performs the actual blocking `write_all`+`flush`.
//!
//! `teardown` kills the child first (unsticking a blocked write with EIO
//! once the slave has no readers), drops the sender so the writer thread's
//! `recv()` exits, then joins both threads.

use anyhow::{Context, Result};
use argus_proto::v1::{agent_frame, AgentFrame, PtyOutput};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

/// Picks the shell to spawn: the requested one if given, else the first
/// standard shell present. Bash first since the agent typically runs as
/// root; `/bin/sh` exists on virtually every Linux as the fallback.
pub fn resolve_shell(requested: &str) -> String {
    if !requested.is_empty() {
        return requested.to_string();
    }
    if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

/// Everything the session dispatch needs to drive one live PTY.
pub struct PtyHandle {
    /// The ONLY way bytes reach the writer: O(1), never blocks (see module
    /// doc). `None` once `teardown` runs -- that's what makes the writer
    /// thread's `recv()` return `Err` and exit.
    input_tx: Option<std_mpsc::Sender<Vec<u8>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// `true` = the reader thread should park before its next read.
    pause: Arc<(Mutex<bool>, Condvar)>,
    reader: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
    /// Guards `teardown` so it runs exactly once even if both an explicit
    /// `close()` and the subsequent `Drop` reach it, or a bare `Drop` does.
    torn_down: AtomicBool,
}

impl PtyHandle {
    /// Enqueues keystrokes for the writer thread. Best-effort and
    /// non-blocking: a dead/torn-down PTY (no sender) just drops them.
    pub fn write_input(&self, data: &[u8]) {
        if let Some(tx) = &self.input_tx {
            let _ = tx.send(data.to_vec());
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(m) = self.master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    /// Parks or wakes the reader thread (server flow control). Recovers a
    /// poisoned mutex rather than no-op'ing or panicking -- either would
    /// strand the session without its closing EOF.
    pub fn set_paused(&self, paused: bool) {
        let (lock, cvar) = &*self.pause;
        {
            let mut p = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *p = paused;
        }
        cvar.notify_all();
    }

    /// Tear the PTY down: kill and reap the shell, then stop and join BOTH
    /// background threads. Idempotent -- safe to call directly and then let
    /// the value drop, or to never call at all and rely solely on `Drop`.
    fn teardown(&mut self) {
        if self.torn_down.swap(true, Ordering::SeqCst) {
            return; // already torn down (explicit close(), then Drop)
        }

        // Recover a poisoned lock rather than skip the kill: `set_paused`
        // and the reader already recover from poison, so this must too, or
        // teardown silently skips the kill while still joining the reader --
        // reintroducing the hang poison-recovery exists to prevent.
        {
            let mut c = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            kill_and_reap(c.as_mut());
        }

        // Drops the sender so `recv()` returns `Err` once queued input
        // drains, then joins. Happens AFTER the kill on purpose: a writer
        // stuck in `write_all` only unblocks once the child is dead and the
        // slave has no readers (EIO) -- killing first is what bounds this join.
        self.input_tx = None;
        if let Some(h) = self.writer_thread.take() {
            let _ = h.join();
        }

        // Wakes a parked reader so it reaches `read()`, which now sees EOF
        // (child is dead, nothing holds the slave open) -- bounding the join.
        self.set_paused(false);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }

    /// Tear the PTY down and wait for full cleanup (see `teardown`).
    pub fn close(mut self) {
        self.teardown();
    }

    /// Test-only pid accessor so tests can verify reaping; not part of the
    /// production API (session dispatch has no use for a raw pid).
    #[cfg(test)]
    fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
    }
}

impl Drop for PtyHandle {
    /// Safety net for a handle dropped without `close()` (panic unwind, a
    /// caller bug): runs the same bounded teardown. No-ops if `close()`
    /// already ran (idempotent).
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Terminates `child` and guarantees it's reaped, escalating to an
/// untrappable kill if needed, in bounded time regardless of the shell.
///
/// `kill()` sends SIGHUP (trappable) on Unix, so one call isn't enough. A
/// second call after a short grace window escalates: `portable-pty`'s Unix
/// impl itself sends a real SIGKILL on a repeat/failed grace period, which
/// cannot be trapped -- except a child stuck in uninterruptible sleep (D
/// state), out of scope here.
fn kill_and_reap(child: &mut dyn Child) {
    let _ = child.kill();

    // Bounded poll: give the signal a short window to land before assuming
    // it didn't. 25 * 20ms = 0.5s.
    for _ in 0..25 {
        match child.try_wait() {
            Ok(Some(_)) => return, // exited and reaped
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }

    // Still around (or unqueryable): escalate, then block until it's gone.
    let _ = child.kill();
    let _ = child.wait();
}

/// Spawn a PTY running `shell` at `cols`x`rows` and start forwarding its output
/// as `PtyOutput{session_id}` frames on `out` (tagged with `stream_id`).
pub fn open(
    session_id: String,
    stream_id: u64,
    cols: u16,
    rows: u16,
    shell: &str,
    out: Sender<AgentFrame>,
) -> Result<PtyHandle> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(resolve_shell(shell));
    cmd.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
    // Drop the slave in the parent so EOF propagates once the child exits.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let mut writer = pair.master.take_writer().context("take writer")?;

    // The writer thread is the sole caller of `write_all`+`flush` (possibly
    // blocking, see module doc), strictly in enqueue order -- so the async
    // dispatch loop that calls `write_input` never touches it directly.
    let (input_tx, input_rx) = std_mpsc::channel::<Vec<u8>>();
    let writer_handle = std::thread::spawn(move || {
        while let Ok(data) = input_rx.recv() {
            let _ = writer.write_all(&data);
            let _ = writer.flush();
        }
    });

    let pause = Arc::new((Mutex::new(false), Condvar::new()));
    let pause_thread = pause.clone();
    let sid = session_id.clone();

    let reader_handle = std::thread::spawn(move || {
        let mut buf = vec![0u8; argus_common::PTY_READ_BUF];
        loop {
            // Parks BEFORE reading so a flooding producer is throttled at
            // the source. Recovers from a poisoned mutex rather than
            // panicking -- a dead reader never reaches the EOF frame below.
            {
                let (lock, cvar) = &*pause_thread;
                let mut paused = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while *paused {
                    paused = cvar
                        .wait(paused)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // Shell exited (or the fd closed): signal EOF and stop.
                    let _ = out.blocking_send(AgentFrame {
                        stream_id,
                        payload: Some(agent_frame::Payload::PtyOutput(PtyOutput {
                            session_id: sid.clone(),
                            data: Vec::new(),
                            eof: true,
                        })),
                    });
                    break;
                }
                Ok(n) => {
                    if out
                        .blocking_send(AgentFrame {
                            stream_id,
                            payload: Some(agent_frame::Payload::PtyOutput(PtyOutput {
                                session_id: sid.clone(),
                                data: buf[..n].to_vec(),
                                eof: false,
                            })),
                        })
                        .is_err()
                    {
                        break; // session gone
                    }
                }
            }
        }
    });

    Ok(PtyHandle {
        input_tx: Some(input_tx),
        master: Mutex::new(pair.master),
        child: Mutex::new(child),
        pause,
        reader: Some(reader_handle),
        writer_thread: Some(writer_handle),
        torn_down: AtomicBool::new(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_shell_prefers_the_requested_shell() {
        assert_eq!(resolve_shell("/bin/zsh"), "/bin/zsh");
    }

    #[test]
    fn resolve_shell_falls_back_when_empty() {
        let s = resolve_shell("");
        assert!(s == "/bin/bash" || s == "/bin/sh", "got {s}");
    }

    #[tokio::test]
    #[ignore = "spawns a real shell; run on a host with /bin/sh"]
    async fn live_open_runs_a_command_and_reports_output() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentFrame>(64);
        let handle = open("s1".into(), 1, 80, 24, "/bin/sh", tx).expect("open pty");
        handle.write_input(b"echo hello-pty\n");

        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(frame)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            {
                if let Some(agent_frame::Payload::PtyOutput(o)) = frame.payload {
                    seen.extend_from_slice(&o.data);
                }
            }
            if String::from_utf8_lossy(&seen).contains("hello-pty") {
                break;
            }
        }
        // Tear down before asserting so a failure doesn't orphan the shell
        // and reader thread.
        let ok = String::from_utf8_lossy(&seen).contains("hello-pty");
        handle.close();
        assert!(
            ok,
            "PTY did not echo the command; got {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// `close()` must reap the child: a zombie shows up in `/proc/<pid>`
    /// with state `Z` until `wait()`/`waitpid()` runs; a properly reaped
    /// child leaves no `/proc` entry at all.
    #[tokio::test]
    #[ignore = "spawns a real shell; run on a host with /bin/sh"]
    async fn live_close_reaps_the_child_leaving_no_zombie() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentFrame>(64);
        let handle = open("s2".into(), 2, 80, 24, "/bin/sh", tx).expect("open pty");
        let pid = handle.pid().expect("child pid");

        handle.close();

        let proc_path = format!("/proc/{pid}/stat");
        assert!(
            !std::path::Path::new(&proc_path).exists(),
            "child pid {pid} was not reaped after close() \
             (still present in /proc -- zombie or otherwise not cleaned up)"
        );

        // Independent corroboration via `ps`: no process at this pid at all
        // (not even a `Z`).
        let ps = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        let stat = String::from_utf8_lossy(&ps.stdout);
        eprintln!(
            "pid={pid} ps -o stat= -p {pid} -> {stat:?} (exit {:?})",
            ps.status.code()
        );
        assert!(
            stat.trim().is_empty(),
            "expected no ps entry for reaped pid {pid}, got stat {stat:?}"
        );
    }

    /// A SIGHUP-ignoring shell must still be forced out in bounded time
    /// (verifies `kill_and_reap`'s escalation). No child process is spawned
    /// here on purpose -- a lingering foreground child would keep its own
    /// copy of the pty slave open, testing fd inheritance instead of the
    /// signal escalation this test targets.
    #[tokio::test]
    #[ignore = "spawns a real shell; run on a host with /bin/sh"]
    async fn live_close_returns_promptly_when_the_shell_ignores_sighup() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentFrame>(64);
        let handle = open("s3".into(), 3, 80, 24, "/bin/sh", tx).expect("open pty");
        handle.write_input(b"trap '' HUP\n");
        // Give the shell time to install the trap and settle back at its
        // (idle, no-child) read loop before we try to kill it.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let start = tokio::time::Instant::now();
        handle.close();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "close() took too long against a SIGHUP-ignoring shell: {elapsed:?}"
        );
    }

    /// `write_input` must return immediately even when the PTY write itself
    /// would block (module doc). `exec sleep 300` reproduces that "hung
    /// program" scenario: a real child that never reads its controlling tty.
    #[tokio::test]
    #[ignore = "spawns a real shell; run on a host with /bin/sh"]
    async fn live_write_input_does_not_block_the_caller_against_a_wedged_program() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentFrame>(64);
        let handle = open("s4".into(), 4, 80, 24, "/bin/sh", tx).expect("open pty");
        handle.write_input(b"exec sleep 300\n");
        // Give the shell time to exec into the child before we try to wedge it.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let start = tokio::time::Instant::now();
        // Comfortably larger than the ~4 KB canonical tty input queue.
        handle.write_input(&vec![b'x'; 64 * 1024]);
        let elapsed = start.elapsed();

        handle.close();
        assert!(
            elapsed < Duration::from_millis(500),
            "write_input blocked the caller for {elapsed:?} -- it must only \
             enqueue onto the writer channel, never perform the blocking \
             write itself"
        );
    }
}
