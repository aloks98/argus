//! An interactive PTY for one terminal session.
//!
//! `portable-pty`'s reader is blocking (`std::io::Read`), so a dedicated OS
//! thread runs the read loop and forwards bytes as `PtyOutput` via
//! `mpsc::blocking_send`. On `PtyFlow{paused}` that thread parks BEFORE its next
//! `read()` (a shared condvar), so the kernel PTY buffer fills and the writing
//! process blocks — real backpressure to the true source.
//!
//! Writes are the mirror-image problem (review finding 3): the master's
//! writer is ALSO blocking (portable-pty 0.9 never sets `O_NONBLOCK`), and
//! the tty's canonical input queue is only ~4 KB. A paste larger than that
//! into a program not reading stdin (`sleep 300`, a hung editor) makes
//! `write_all` block for as long as the program doesn't read. Calling that
//! inline on the agent's async gRPC inbound-dispatch loop -- as this used to
//! -- stalls processing of every OTHER frame for the machine, `PtyClose`
//! included, while holding the `inbound_ptys` registry mutex the whole time.
//! So writes get their own dedicated OS thread too (`open`, below), fed by an
//! unbounded, in-order `std::sync::mpsc` channel: `write_input` (called
//! inline from the async dispatch loop) only ever pushes onto that channel,
//! which is O(1) and never blocks, and returns immediately. The blocking
//! `write_all` + `flush` happens exclusively on the writer thread, in the
//! order bytes were enqueued (a single-consumer FIFO channel preserves
//! order, and only one thread ever calls `write_all`, so two `PtyInput`
//! frames for the same session can never interleave on the wire the way two
//! independent `spawn_blocking` calls racing on the tokio blocking pool
//! could). `teardown` kills the child first (unblocking a stuck write with
//! EIO once the slave has no more readers), then drops the channel's sender
//! so the writer thread's `recv()` returns `Err` and it exits, then joins it
//! -- mirroring the reader thread's own park-then-join teardown.

use anyhow::{Context, Result};
use argus_proto::v1::{agent_frame, AgentFrame, PtyOutput};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

/// Pick the shell to spawn: the requested one if given, else the first standard
/// shell present. The agent typically runs as root, so `/bin/bash` then
/// `/bin/sh` is the sane order; `/bin/sh` exists on essentially every Linux.
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
    /// The ONLY way bytes reach the PTY's writer: pushing here is O(1) and
    /// never blocks, unlike the `write_all`+`flush` it feeds (see the
    /// module doc). `None` once `teardown` has run, which is what makes the
    /// writer thread's `recv()` return `Err` and exit.
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
    /// Enqueue keystrokes for the writer thread. Best-effort and
    /// non-blocking: a dead/torn-down PTY (no sender, or the writer thread
    /// already gone) just drops them, exactly like the old best-effort
    /// `write_all` did on a dead PTY.
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

    /// Park or wake the reader thread (server flow control).
    ///
    /// A poisoned pause mutex must never strand a session without its
    /// closing EOF: recover the guard rather than silently no-op'ing (as this
    /// used to) or panicking (as the reader thread's own lock/wait used to) --
    /// both would leave the reader either stuck or dead without cleanup.
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

        // Recover a poisoned lock rather than skipping the kill (as a bare
        // `if let Ok(..)` used to): `set_paused` and the reader thread's own
        // lock/wait already recover from poison, so a poisoned `child` lock
        // must too, or teardown silently skips the kill while still joining
        // the reader below -- reintroducing the unbounded hang a poisoned
        // lock is supposed to be recovered FROM.
        {
            let mut c = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            kill_and_reap(c.as_mut());
        }

        // Drop the writer's channel sender so its `recv()` returns `Err`
        // once any already-queued input drains, and join it. This happens
        // AFTER the kill above on purpose: a writer thread parked inside a
        // blocking `write_all` (the paste-into-a-wedged-program case this
        // fix exists for) only unblocks once the child is dead and the pty
        // slave has no more readers (the write then fails with EIO), so
        // killing first is what bounds this join.
        self.input_tx = None;
        if let Some(h) = self.writer_thread.take() {
            let _ = h.join();
        }

        // Wake a parked reader so it reaches its next `read()`, which will
        // now see EOF (the child above is dead, so nothing holds the pty
        // slave open) and return -- bounding the join below.
        self.set_paused(false);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }

    /// Tear the PTY down and wait for full cleanup (see `teardown`).
    pub fn close(mut self) {
        self.teardown();
    }

    /// Test-only accessor for the child's pid, so tests can verify it was
    /// actually reaped. Not part of the production API surface: session
    /// dispatch has no legitimate use for a raw pid, and this is compiled
    /// out entirely in non-test builds.
    #[cfg(test)]
    fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
    }
}

impl Drop for PtyHandle {
    /// Safety net for a handle dropped without an explicit `close()` (a
    /// panic unwind, a caller bug): run the same bounded teardown so the
    /// reader thread and child are never orphaned. No-ops if `close()`
    /// already ran (`teardown` is idempotent).
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Terminate `child` and guarantee it exits and is reaped -- escalating to a
/// hard, untrappable kill if the initial signal doesn't do it, so this
/// returns in bounded time regardless of what the shell does.
///
/// `portable-pty`'s own `kill()` sends SIGHUP on Unix (a shell can trap or
/// ignore it), so a single call is not sufficient on its own for a
/// guarantee. We give it a short window to take effect, and if the child is
/// still alive, call `kill()` again: portable-pty's Unix implementation
/// itself escalates a still-alive child to a real SIGKILL on a repeat/failed
/// grace period, which cannot be trapped or ignored -- so by the time we
/// reach the final `wait()`, termination is guaranteed short of the child
/// being stuck in an uninterruptible kernel sleep (D state), which no signal
/// fixes and is out of scope for a terminal session.
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

    // The writer thread: the sole caller of the (possibly blocking, see the
    // module doc) `write_all`+`flush`, one chunk at a time, strictly in the
    // order `write_input` enqueued them. `write_input` itself never touches
    // `writer` directly -- only this thread does -- so the async gRPC
    // dispatch loop that calls `write_input` can never block on a wedged
    // program's full tty input queue.
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
            // Park BEFORE reading so a flooding producer is throttled at source.
            // Recover from a poisoned mutex rather than panicking: a dead
            // reader thread never reaches the `eof` frame below, stranding
            // the session without a close signal.
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
        // Empty request -> one of the standard shells present on the host.
        let s = resolve_shell("");
        assert!(s == "/bin/bash" || s == "/bin/sh", "got {s}");
    }

    /// Spawn a real `/bin/sh`, send a command, and read its output back.
    #[tokio::test]
    #[ignore = "spawns a real shell; run on a host with /bin/sh"]
    async fn live_open_runs_a_command_and_reports_output() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentFrame>(64);
        let handle = open("s1".into(), 1, 80, 24, "/bin/sh", tx).expect("open pty");
        handle.write_input(b"echo hello-pty\n");

        // Collect output for up to 2s, looking for the echoed marker.
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

    /// `close()` must reap the child -- no zombie left behind once it
    /// returns. A zombie shows up in `/proc/<pid>` with state `Z` until
    /// something calls `wait()`/`waitpid()` on it; a properly reaped child
    /// leaves no `/proc` entry at all.
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

        // Independent corroboration via `ps`, matching the exact check the
        // review asked for: no process at this pid at all (not even a `Z`).
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

    /// A shell that ignores SIGHUP must still be forced out: `close()` must
    /// return in bounded time rather than hang behind a `read()` that would
    /// never see EOF from a plain, trappable SIGHUP alone. No child process
    /// is spawned inside the shell here on purpose -- a lingering foreground
    /// child (e.g. a backgrounded `sleep`) would keep its own copy of the pty
    /// slave open regardless of what happens to the shell, which would be
    /// testing process-tree fd inheritance rather than the signal escalation
    /// this fix is actually responsible for.
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

    /// Review finding 3: `write_input` must return immediately even when the
    /// PTY's own write would block -- e.g. a paste larger than the ~4 KB
    /// canonical tty input queue into a program that never reads stdin.
    /// `exec sleep 300` replaces the shell with a real child that never
    /// reads its controlling tty, reproducing the exact "hung program"
    /// scenario the finding describes. Before the fix this would call
    /// `write_all` inline and block for as long as the program doesn't
    /// read -- here, on the agent's async gRPC inbound loop, that would have
    /// stalled every other frame for the machine.
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
