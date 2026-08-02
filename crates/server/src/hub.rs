//! In-memory session hub, shared as `Arc<Hub>` between gRPC (fills it from
//! the Session stream) and HTTP (reads snapshots, dispatches verbs). All
//! state is re-derived on reconnect -- consistent with the stateless,
//! single-replica control plane.

use crate::repo;
use argus_proto::v1::{
    server_frame, Command, CommandResult, Container, LogChunk, LogTailRequest, LogTailStop,
    PtyClose, PtyFlow, PtyInput, PtyOpen, PtyResize, ServerFrame, Unit, Verb,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Notify};
use tonic::Status;
use uuid::Uuid;

/// Per-session flow-control accounting, shared between the gRPC inbound task
/// (fills the buffer) and the WS output task (drains it).
///
/// `buffered`/`paused` are guarded as ONE unit (a single `Mutex`, not two
/// atomics), so a fill racing a drain can never compute the pause/resume
/// edge from a torn combination. A dropped `PtyFlow` send (agent's outbound
/// channel is only 16 deep) could otherwise leave this pair out of sync:
/// `paused` latches true forever, or we record "resumed" while the agent is
/// still parked, wedging the terminal silently until the 30-min idle timer.
pub struct PtyFlowState {
    inner: Mutex<PtyFlowInner>,
}

struct PtyFlowInner {
    buffered: usize,
    paused: bool,
}

/// One live agent connection: outbound frame channel, a per-connection
/// stream_id counter, and an `epoch` distinguishing successive connections
/// of the same machine so a lingering old session's teardown can't evict a
/// freshly-reconnected one.
///
/// `shutdown` covers the other failure mode: the epoch guard alone doesn't
/// stop the STALE session's server-side loop -- unrouted via `conns` but
/// still pumping heartbeats. Each `register` hands the caller its own
/// handle's `shutdown` to select on; a later `register` for the same machine
/// fires it, so the superseded loop exits through normal teardown instead of
/// lingering alive-but-unroutable.
struct ConnHandle {
    tx: mpsc::Sender<Result<ServerFrame, Status>>,
    next_stream_id: AtomicU64,
    epoch: u64,
    shutdown: Arc<Notify>,
}

/// Why a command couldn't be dispatched.
#[derive(Debug)]
pub enum DispatchError {
    /// No live Session for that machine (offline / just disconnected).
    NotConnected,
}

#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<Uuid, ConnHandle>>,
    docker: Mutex<HashMap<Uuid, Vec<Container>>>,
    systemd: Mutex<HashMap<Uuid, Vec<Unit>>>,
    pending: Mutex<HashMap<String, (Uuid, oneshot::Sender<CommandResult>)>>,
    /// request_id -> the SSE sink for that tail + its owning machine (a
    /// stream sink, unlike `pending`'s one-shot). Filled by `deliver_chunk`;
    /// read/removed via `open_tail`/`close_tail`.
    tails: Mutex<HashMap<String, (Uuid, mpsc::Sender<LogChunk>)>>,
    /// session_id -> (machine, byte sink for the WS handler, flow state).
    #[allow(clippy::type_complexity)]
    ptys: Mutex<HashMap<String, (Uuid, mpsc::Sender<Vec<u8>>, Arc<PtyFlowState>)>>,
    /// machine -> when `repo::mark_online` last actually wrote, for
    /// `should_mark_online`'s debounce. Entries are dropped on session end
    /// (`clear_online_stamp`) so the map stays bounded by fleet size.
    online_stamps: Mutex<HashMap<Uuid, std::time::Instant>>,
    epoch_counter: AtomicU64,
}

/// How long after a `mark_online` write further writes are skipped. The agent
/// sends Heartbeat + Metrics + DockerState + SystemdState back-to-back every
/// tick, which without this is 4 `UPDATE machines` round trips (and 4 dead
/// row versions) per 15s per machine where one carries all the information.
///
/// MUST stay well under `DEFAULT_HEARTBEAT_SECS` (15s): the first frame of
/// each tick then always writes, so `last_seen_at` is at most one debounce
/// window stale and a sweeper-flipped (45s) machine still recovers on its
/// next heartbeat exactly as before.
const MARK_ONLINE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

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

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    /// Returns the epoch and a shutdown signal for the CALLER's own session
    /// loop to select on. A re-register for the same machine replaces the
    /// old handle and fires its shutdown (see `ConnHandle`'s doc for why).
    pub fn register(
        &self,
        machine_id: Uuid,
        tx: mpsc::Sender<Result<ServerFrame, Status>>,
    ) -> (u64, Arc<Notify>) {
        let epoch = self.epoch_counter.fetch_add(1, Ordering::Relaxed) + 1;
        let shutdown = Arc::new(Notify::new());
        let old = self.conns.lock().unwrap().insert(
            machine_id,
            ConnHandle {
                tx,
                next_stream_id: AtomicU64::new(1),
                epoch,
                shutdown: shutdown.clone(),
            },
        );
        if let Some(old) = old {
            old.shutdown.notify_one();
        }
        (epoch, shutdown)
    }

    /// Remove a connection only if it is still the one with this `epoch` — a
    /// stale disconnect must not evict a newer reconnection.
    pub fn unregister(&self, machine_id: Uuid, epoch: u64) {
        let mut conns = self.conns.lock().unwrap();
        if conns.get(&machine_id).map(|h| h.epoch) == Some(epoch) {
            conns.remove(&machine_id);
        }
    }

    /// Used by the terminal WS handler to tell apart two failure modes that
    /// look identical from the receiving end (a PTY channel just closes, no
    /// `eof`): the whole connection ending (`close_ptys_for`, strictly AFTER
    /// `unregister`) vs `deliver_pty_output`'s overrun backstop (`close_pty`,
    /// which never touches `conns`). By the time a caller sees the channel
    /// closed, a real disconnect's `unregister` has already run.
    pub fn is_connected(&self, machine_id: Uuid) -> bool {
        self.conns.lock().unwrap().contains_key(&machine_id)
    }

    pub fn set_docker(&self, machine_id: Uuid, containers: Vec<Container>) {
        self.docker.lock().unwrap().insert(machine_id, containers);
    }

    /// The latest cached snapshot for a machine (empty if none reported yet).
    pub fn get_docker(&self, machine_id: Uuid) -> Vec<Container> {
        self.docker
            .lock()
            .unwrap()
            .get(&machine_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_systemd(&self, machine_id: Uuid, units: Vec<Unit>) {
        self.systemd.lock().unwrap().insert(machine_id, units);
    }

    /// The latest cached unit snapshot for a machine (empty if none reported).
    pub fn get_systemd(&self, machine_id: Uuid) -> Vec<Unit> {
        self.systemd
            .lock()
            .unwrap()
            .get(&machine_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether a liveness-bearing frame should actually write
    /// `repo::mark_online`, reserving the slot (stamping `now`) when it says
    /// yes -- check and stamp are one step under the lock, so two frames
    /// racing can't both be told to write. `true` at most once per
    /// `MARK_ONLINE_DEBOUNCE` per machine.
    ///
    /// Best-effort by design: if the caller's write then fails, the stamp is
    /// still spent and `last_seen_at` waits for the next window -- fine,
    /// because the sweep threshold (45s) dwarfs the window (5s).
    pub fn should_mark_online(&self, machine_id: Uuid) -> bool {
        let mut stamps = self.online_stamps.lock().unwrap();
        let now = std::time::Instant::now();
        match stamps.get(&machine_id) {
            Some(last) if now.duration_since(*last) < MARK_ONLINE_DEBOUNCE => false,
            _ => {
                stamps.insert(machine_id, now);
                true
            }
        }
    }

    /// Unconditional stamp for the `Hello` path, which always writes (a fresh
    /// session must flip a pending/offline machine immediately) but must
    /// still start a debounce window so the snapshot frames right behind it
    /// don't each write again.
    pub fn stamp_online(&self, machine_id: Uuid) {
        self.online_stamps
            .lock()
            .unwrap()
            .insert(machine_id, std::time::Instant::now());
    }

    /// Session ended: forget the stamp so the machine's next session (or a
    /// concurrent one) writes on its first liveness frame, and the map
    /// doesn't accumulate entries for departed machines.
    pub fn clear_online_stamp(&self, machine_id: Uuid) {
        self.online_stamps.lock().unwrap().remove(&machine_id);
    }

    /// Counted here (not in the handler) so the fleet query stays one cheap
    /// read under a single lock, never cloning the snapshot.
    pub fn failed_unit_count(&self, machine_id: Uuid) -> usize {
        self.systemd
            .lock()
            .unwrap()
            .get(&machine_id)
            .map(|units| units.iter().filter(|u| u.active_state == "failed").count())
            .unwrap_or(0)
    }

    /// Send a verb down the machine's Session on a fresh non-zero stream_id.
    pub async fn send_command(
        &self,
        machine_id: Uuid,
        command_id: String,
        verb: Verb,
        target: String,
        issued_by: repo::Actor<'_>,
    ) -> Result<(), DispatchError> {
        // Extract under the lock, release before the async send (never hold
        // a std Mutex guard across an await).
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::Command(Command {
                command_id,
                verb: verb as i32,
                target,
                issued_by: issued_by.as_str().into_owned(),
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Register interest in a command's result; the returned receiver resolves
    /// when a matching `CommandResult` frame arrives (or errors if abandoned).
    pub fn register_pending(
        &self,
        command_id: String,
        machine_id: Uuid,
    ) -> oneshot::Receiver<CommandResult> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(command_id, (machine_id, tx));
        rx
    }

    /// Drop a pending waiter (e.g. on dispatch failure or client timeout).
    pub fn abandon_pending(&self, command_id: &str) {
        self.pending.lock().unwrap().remove(command_id);
    }

    /// Delivers only if the resolving session is the machine the command was
    /// dispatched to -- a result from any other authenticated session is
    /// ignored, waiter left intact. No-op if there is no waiter.
    pub fn complete(&self, command_id: &str, machine_id: Uuid, result: CommandResult) {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(command_id).map(|(m, _)| *m) == Some(machine_id) {
            if let Some((_, tx)) = pending.remove(command_id) {
                let _ = tx.send(result);
            }
        }
    }

    /// Buffer is generous (agent already batches); if it fills, the browser
    /// is slower than the log, and drop-counting happens agent-side.
    pub fn open_tail(&self, machine_id: Uuid) -> (String, mpsc::Receiver<LogChunk>) {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(64);
        self.tails
            .lock()
            .unwrap()
            .insert(request_id.clone(), (machine_id, tx));
        (request_id, rx)
    }

    pub fn close_tail(&self, request_id: &str) {
        self.tails.lock().unwrap().remove(request_id);
    }

    /// Called when the agent's session ends: agent-side tails are already
    /// aborted, so these server-side sinks would otherwise hang the SSE
    /// stream open with no eof -- a frozen "live" tail in the browser forever.
    pub fn close_tails_for(&self, machine_id: Uuid) {
        self.tails
            .lock()
            .unwrap()
            .retain(|_, (owner, _)| *owner != machine_id);
    }

    /// Only from the machine the tail was opened against -- same trust
    /// boundary `complete()` enforces. An eof chunk is delivered, then
    /// closes the sink.
    pub fn deliver_chunk(&self, request_id: &str, machine_id: Uuid, chunk: LogChunk) {
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

    #[allow(clippy::too_many_arguments)]
    pub async fn send_log_start(
        &self,
        machine_id: Uuid,
        request_id: String,
        source: String,
        tail_lines: u32,
        follow: bool,
        before_cursor: String,
        filters: LogFilters,
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
                max_priority: filters.max_priority,
                since_ms: filters.since_ms,
                current_boot: filters.current_boot,
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
            payload: Some(server_frame::Payload::LogTailStop(LogTailStop {
                request_id,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Mirrors `open_tail`.
    pub fn open_pty(
        &self,
        machine_id: Uuid,
    ) -> (String, mpsc::Receiver<Vec<u8>>, Arc<PtyFlowState>) {
        let session_id = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(argus_common::PTY_CHANNEL_CAP);
        let state = Arc::new(PtyFlowState {
            inner: Mutex::new(PtyFlowInner {
                buffered: 0,
                paused: false,
            }),
        });
        self.ptys
            .lock()
            .unwrap()
            .insert(session_id.clone(), (machine_id, tx, state.clone()));
        (session_id, rx, state)
    }

    pub fn close_pty(&self, session_id: &str) {
        self.ptys.lock().unwrap().remove(session_id);
    }

    pub fn close_ptys_for(&self, machine_id: Uuid) {
        self.ptys
            .lock()
            .unwrap()
            .retain(|_, (owner, _, _)| *owner != machine_id);
    }

    /// Routes an agent `PtyOutput` into its session's buffer. NON-BLOCKING: the
    /// caller is the gRPC inbound drain, which must never stall the shared
    /// Session. On crossing the high-water mark, emits `PtyFlow{paused:true}`. A
    /// full channel is the unreachable-by-sizing defensive teardown, never a
    /// dropped byte.
    ///
    /// Channel contract: an EMPTY `Vec<u8>` item is *exclusively* the eof marker
    /// -- enforced structurally (a genuinely-empty, non-eof `PtyOutput` is
    /// dropped before reaching the channel), not by agent-side convention.
    pub fn deliver_pty_output(&self, session_id: &str, machine_id: Uuid, data: Vec<u8>, eof: bool) {
        let (sender, state) = {
            let ptys = self.ptys.lock().unwrap();
            match ptys.get(session_id) {
                Some((owner, tx, st)) if *owner == machine_id => (tx.clone(), st.clone()),
                _ => return,
            }
        };
        // Dropping this is free and matches the eof-marker invariant above.
        if data.is_empty() && !eof {
            return;
        }
        let len = data.len();
        if len > 0 && sender.try_send(data).is_err() {
            // Channel full despite flow control (a bug, not a fast
            // program): tear the session down rather than drop or block.
            self.close_pty(session_id);
            let _ = self.try_send_pty_close(machine_id, session_id);
            return;
        }
        if eof {
            // Same never-drop-never-block rule applies to the marker: if it
            // can't be enqueued, tear the session down rather than leave the
            // WS handler waiting on an eof that will never arrive.
            if sender.try_send(Vec::new()).is_err() {
                self.close_pty(session_id);
                let _ = self.try_send_pty_close(machine_id, session_id);
                return;
            }
        }
        if len == 0 {
            return;
        }
        // Account bytes and decide the pause edge as ONE guarded step (see
        // `PtyFlowState`'s doc), so a drain can never observe a torn pair.
        let should_pause = {
            let mut inner = state.inner.lock().unwrap();
            inner.buffered += len;
            if inner.buffered >= argus_common::PTY_HIGH_WATER && !inner.paused {
                inner.paused = true;
                true
            } else {
                false
            }
        };
        if should_pause
            && self
                .try_send_pty_flow(machine_id, session_id, true)
                .is_err()
        {
            // The frame never reached the agent (its channel is fullest
            // exactly under load, exactly when this fires). Roll `paused`
            // back rather than leaving it latched true with the agent never
            // told -- best-effort (no await, per the never-stall rule),
            // unlike `on_pty_drained`'s guaranteed resume below.
            let mut inner = state.inner.lock().unwrap();
            inner.paused = false;
        }
    }

    /// Called by the WS handler after writing `len` bytes to the socket, so
    /// the byte counter reflects what's still buffered. Drops below
    /// low-water -> emits `PtyFlow{paused:false}`.
    ///
    /// Async (unlike `deliver_pty_output`): can afford to await room on the
    /// agent's channel -- a GUARANTEED delivery, unlike the pause edge's
    /// best-effort `try_send`. A dropped resume wedges the terminal forever
    /// (parked agent produces no more output to retry); a dropped pause
    /// retries on the next byte.
    pub async fn on_pty_drained(&self, session_id: &str, len: usize) {
        let (machine_id, state) = {
            let ptys = self.ptys.lock().unwrap();
            match ptys.get(session_id) {
                Some((owner, _, st)) => (*owner, st.clone()),
                None => return,
            }
        };
        let should_resume = {
            let mut inner = state.inner.lock().unwrap();
            // Saturating: an over-report here must not wrap `buffered` to
            // ~usize::MAX and pin the session above high-water forever.
            inner.buffered = inner.buffered.saturating_sub(len);
            if inner.buffered <= argus_common::PTY_LOW_WATER && inner.paused {
                inner.paused = false;
                true
            } else {
                false
            }
        };
        if should_resume {
            let _ = self.send_pty_flow(machine_id, session_id, false).await;
        }
    }

    /// From `deliver_pty_output`'s sync, never-await accounting path.
    fn try_send_pty_flow(
        &self,
        machine_id: Uuid,
        session_id: &str,
        paused: bool,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyFlow(PtyFlow {
                session_id: session_id.to_string(),
                paused,
            })),
        };
        tx.try_send(Ok(frame))
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Guaranteed-delivery (awaits room) -- used only by `on_pty_drained`,
    /// see its doc for why the resume edge needs this over `try_send_pty_flow`.
    async fn send_pty_flow(
        &self,
        machine_id: Uuid,
        session_id: &str,
        paused: bool,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyFlow(PtyFlow {
                session_id: session_id.to_string(),
                paused,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    fn try_send_pty_close(&self, machine_id: Uuid, session_id: &str) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyClose(PtyClose {
                session_id: session_id.to_string(),
            })),
        };
        tx.try_send(Ok(frame))
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Dispatched by `terminal::handle` before it audits, so an offline agent
    /// leaves no `terminal.open` row.
    pub async fn send_pty_open(
        &self,
        machine_id: Uuid,
        session_id: String,
        cols: u32,
        rows: u32,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyOpen(PtyOpen {
                session_id,
                cols,
                rows,
                shell: String::new(),
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    pub async fn send_pty_input(
        &self,
        machine_id: Uuid,
        session_id: String,
        data: Vec<u8>,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyInput(PtyInput {
                session_id,
                data,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    pub async fn send_pty_resize(
        &self,
        machine_id: Uuid,
        session_id: String,
        cols: u32,
        rows: u32,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyResize(PtyResize {
                session_id,
                cols,
                rows,
            })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Dispatched by `terminal::handle`'s `PtyCloseGuard` on every exit past a
    /// successful `PtyOpen` dispatch, including a panic unwind.
    pub async fn send_pty_close(
        &self,
        machine_id: Uuid,
        session_id: String,
    ) -> Result<(), DispatchError> {
        let (tx, stream_id) = self.conn_slot(machine_id)?;
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::PtyClose(PtyClose { session_id })),
        };
        tx.send(Ok(frame))
            .await
            .map_err(|_| DispatchError::NotConnected)
    }

    /// Outbound channel plus a fresh non-zero sub-stream id -- factored out
    /// since every sender needs the same "extract under the lock, release
    /// before the await" dance.
    fn conn_slot(
        &self,
        machine_id: Uuid,
    ) -> Result<(mpsc::Sender<Result<ServerFrame, Status>>, u64), DispatchError> {
        let conns = self.conns.lock().unwrap();
        let handle = conns.get(&machine_id).ok_or(DispatchError::NotConnected)?;
        let stream_id = handle.next_stream_id.fetch_add(1, Ordering::Relaxed);
        Ok((handle.tx.clone(), stream_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(id: &str, state: &str) -> Container {
        Container {
            id: id.into(),
            name: format!("name-{id}"),
            image: "img".into(),
            state: state.into(),
            status: "Up".into(),
            health: String::new(),
        }
    }

    fn unit(name: &str, active_state: &str) -> Unit {
        Unit {
            name: name.into(),
            load_state: "loaded".into(),
            active_state: active_state.into(),
            sub_state: "dead".into(),
            description: format!("desc {name}"),
        }
    }

    /// One write per debounce window per machine: the first caller is told
    /// to write and reserves the slot; back-to-back frames (the agent sends
    /// four per tick) are told not to. `clear_online_stamp` (session end /
    /// window expiry) and `stamp_online` (Hello) both reset the edge.
    #[test]
    fn should_mark_online_debounces_until_cleared() {
        let hub = Hub::new();
        let m = Uuid::new_v4();

        assert!(hub.should_mark_online(m), "first frame must write");
        assert!(
            !hub.should_mark_online(m),
            "a frame inside the window must not"
        );
        assert!(
            hub.should_mark_online(Uuid::new_v4()),
            "the debounce is per machine, not global"
        );

        hub.clear_online_stamp(m);
        assert!(
            hub.should_mark_online(m),
            "a cleared stamp (session end / expired window) must write again"
        );

        hub.stamp_online(m);
        assert!(
            !hub.should_mark_online(m),
            "stamp_online (the Hello path) must open a window too"
        );
    }

    #[test]
    fn set_then_get_docker_round_trips_and_defaults_empty() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert!(hub.get_docker(m).is_empty());
        hub.set_docker(m, vec![container("a", "running")]);
        let got = hub.get_docker(m);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");
    }

    #[test]
    fn set_then_get_systemd_round_trips_and_defaults_empty() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert!(hub.get_systemd(m).is_empty());
        hub.set_systemd(m, vec![unit("nginx.service", "active")]);
        let got = hub.get_systemd(m);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "nginx.service");
    }

    #[test]
    fn failed_unit_count_counts_only_failed_units() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        assert_eq!(hub.failed_unit_count(m), 0, "unknown machine reports zero");

        hub.set_systemd(
            m,
            vec![
                unit("a.service", "active"),
                unit("b.service", "failed"),
                unit("c.mount", "failed"),
                unit("d.service", "inactive"),
            ],
        );
        assert_eq!(hub.failed_unit_count(m), 2);
    }

    #[test]
    fn a_later_snapshot_replaces_the_earlier_one() {
        // Full snapshot resent every tick/reconnect; a resolved failure must not linger.
        let hub = Hub::new();
        let m = Uuid::new_v4();
        hub.set_systemd(m, vec![unit("b.service", "failed")]);
        assert_eq!(hub.failed_unit_count(m), 1);
        hub.set_systemd(m, vec![unit("b.service", "active")]);
        assert_eq!(hub.failed_unit_count(m), 0);
    }

    #[test]
    fn stale_unregister_does_not_evict_a_newer_connection() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx1, _rx1) = mpsc::channel(1);
        let (epoch1, _shutdown1) = hub.register(m, tx1);
        let (tx2, _rx2) = mpsc::channel(1);
        let (_epoch2, _shutdown2) = hub.register(m, tx2);
        hub.unregister(m, epoch1);
        assert!(
            hub.conns.lock().unwrap().contains_key(&m),
            "newer connection must survive a stale unregister"
        );
    }

    #[test]
    fn a_normal_single_session_disconnect_still_unregisters() {
        // No supersession: a lone session's own teardown must still remove
        // its handle -- the epoch guard must not make this a no-op.
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(1);
        let (epoch, _shutdown) = hub.register(m, tx);
        hub.unregister(m, epoch);
        assert!(
            !hub.conns.lock().unwrap().contains_key(&m),
            "a session's own disconnect must unregister it"
        );
    }

    #[tokio::test]
    async fn register_replacing_a_live_handle_fires_the_old_handles_shutdown_signal() {
        // A connects, then B connects for the same machine (replacing A). A's
        // shutdown must fire, and the hub must still route to B.
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (epoch_a, shutdown_a) = hub.register(m, tx_a);
        let (tx_b, mut rx_b) = mpsc::channel(4);
        let (epoch_b, _shutdown_b) = hub.register(m, tx_b);
        assert_ne!(epoch_a, epoch_b, "each registration gets a fresh epoch");

        // A permit is already stored, so this resolves immediately.
        tokio::time::timeout(std::time::Duration::from_millis(100), shutdown_a.notified())
            .await
            .expect("the replaced session's shutdown signal must fire promptly");

        hub.send_command(
            m,
            "cmd-super".into(),
            Verb::ContainerStart,
            "c1".into(),
            repo::Actor::System,
        )
        .await
        .expect("the hub must still route to the surviving session B");
        let frame = rx_b.recv().await.expect("B receives the dispatched frame");
        assert!(frame.is_ok());

        assert!(
            hub.conns.lock().unwrap().contains_key(&m),
            "the registry must still hold B's live connection"
        );
    }

    #[tokio::test]
    async fn send_command_to_absent_machine_errors() {
        let hub = Hub::new();
        let res = hub
            .send_command(
                Uuid::new_v4(),
                "cmd-1".into(),
                Verb::ContainerStart,
                "c1".into(),
                repo::Actor::System,
            )
            .await;
        assert!(matches!(res, Err(DispatchError::NotConnected)));
    }

    #[tokio::test]
    async fn send_command_delivers_a_command_frame_on_a_nonzero_stream() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(4);
        hub.register(m, tx);
        hub.send_command(
            m,
            "cmd-9".into(),
            Verb::ContainerRestart,
            "web".into(),
            repo::Actor::System,
        )
        .await
        .expect("dispatch");
        let frame = rx.recv().await.unwrap().unwrap();
        assert_ne!(frame.stream_id, 0, "commands ride a non-zero sub-stream");
        match frame.payload {
            Some(server_frame::Payload::Command(c)) => {
                assert_eq!(c.command_id, "cmd-9");
                assert_eq!(c.verb, Verb::ContainerRestart as i32);
                assert_eq!(c.target, "web");
            }
            other => panic!("expected a Command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_pending_then_complete_delivers_result() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let rx = hub.register_pending("cmd-2".into(), m);
        hub.complete(
            "cmd-2",
            m,
            CommandResult {
                command_id: "cmd-2".into(),
                ok: true,
                exit_code: 0,
                message: "done".into(),
            },
        );
        let got = rx.await.expect("result delivered");
        assert!(got.ok);
    }

    #[tokio::test]
    async fn abandoned_pending_never_delivers() {
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let rx = hub.register_pending("cmd-3".into(), m);
        hub.abandon_pending("cmd-3");
        hub.complete(
            "cmd-3",
            m,
            CommandResult {
                command_id: "cmd-3".into(),
                ok: true,
                exit_code: 0,
                message: String::new(),
            },
        );
        assert!(rx.await.is_err(), "sender was dropped; receiver must error");
    }

    #[tokio::test]
    async fn complete_from_a_different_machine_does_not_wake_the_waiter() {
        let hub = Hub::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut rx = hub.register_pending("cmd-x".into(), owner);
        hub.complete(
            "cmd-x",
            other,
            CommandResult {
                command_id: "cmd-x".into(),
                ok: true,
                exit_code: 0,
                message: String::new(),
            },
        );
        assert!(
            rx.try_recv().is_err(),
            "a foreign machine must not resolve the waiter"
        );
        hub.complete(
            "cmd-x",
            owner,
            CommandResult {
                command_id: "cmd-x".into(),
                ok: true,
                exit_code: 0,
                message: "ok".into(),
            },
        );
        assert!(rx.await.expect("owner resolves").ok);
    }

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
        // Same trust boundary as command results: an authenticated agent
        // must not be able to inject into another machine's tail.
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
    async fn close_tails_for_closes_only_the_owning_machines_tails() {
        let hub = Hub::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (_rid_a1, mut rx_a1) = hub.open_tail(a);
        let (_rid_a2, mut rx_a2) = hub.open_tail(a);
        let (rid_b, mut rx_b) = hub.open_tail(b);

        hub.close_tails_for(a);

        assert!(rx_a1.recv().await.is_none(), "A's first tail must close");
        assert!(rx_a2.recv().await.is_none(), "A's second tail must close");

        hub.deliver_chunk(&rid_b, b, chunk(&rid_b, "still alive", false));
        assert_eq!(
            rx_b.recv().await.expect("B's tail still delivers").data,
            b"still alive"
        );
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
        hub.send_log_start(
            m,
            "r1".into(),
            "journal:nginx.service".into(),
            200,
            true,
            String::new(),
            LogFilters::default(),
        )
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
                assert_eq!(r.before_cursor, "");
                assert_eq!(r.max_priority, 0);
                assert_eq!(r.since_ms, 0);
                assert!(!r.current_boot);
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

    #[tokio::test]
    async fn pty_flow_pauses_at_high_water_and_resumes_at_low_water() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        // A fake agent connection so send_pty_flow has somewhere to go.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        hub.register(machine_id, tx);

        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        let chunk = vec![0u8; argus_common::PTY_HIGH_WATER + 1];
        hub.deliver_pty_output(&session_id, machine_id, chunk, false);
        let paused = recv_pty_flow(&mut agent_rx).await;
        assert!(
            paused,
            "crossing high-water must emit PtyFlow{{paused:true}}"
        );

        while rx.try_recv().is_ok() {
            hub.on_pty_drained(&session_id, argus_common::PTY_HIGH_WATER + 1)
                .await;
        }
        let resumed_paused = recv_pty_flow(&mut agent_rx).await;
        assert!(!resumed_paused, "draining below low-water must resume");
    }

    // Helper: pull frames until a PtyFlow arrives, return its `paused`.
    async fn recv_pty_flow(rx: &mut mpsc::Receiver<Result<ServerFrame, Status>>) -> bool {
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("a frame within 1s")
                .expect("channel open")
                .expect("ok frame");
            if let Some(server_frame::Payload::PtyFlow(f)) = frame.payload {
                return f.paused;
            }
        }
    }

    /// Realistic PTY output arrives as many small chunks (~138 bytes), not
    /// one 64 KiB block. Asserts the BYTE watermark binds before the
    /// message-count channel cap does: `PtyFlow{paused:true}` must come out,
    /// and the session must still be alive afterward.
    #[tokio::test]
    async fn many_small_chunks_trip_the_byte_watermark_before_the_channel_fills() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        hub.register(machine_id, tx);
        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        let chunk = vec![b'x'; 138];
        // Comfortably past PTY_HIGH_WATER at 138 bytes/chunk.
        let chunks_needed = argus_common::PTY_HIGH_WATER / 138 + 8;
        for _ in 0..chunks_needed {
            hub.deliver_pty_output(&session_id, machine_id, chunk.clone(), false);
        }

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), agent_rx.recv())
            .await
            .expect("a frame within 1s -- flow control never engaged in time")
            .expect("channel open")
            .expect("ok frame");
        match frame.payload {
            Some(server_frame::Payload::PtyFlow(f)) => {
                assert!(f.paused, "expected PtyFlow{{paused:true}}")
            }
            Some(server_frame::Payload::PtyClose(_)) => panic!(
                "session was torn down by the full-channel backstop instead of being \
                 throttled -- the message-count cap bound before the byte watermark did"
            ),
            other => panic!("expected PtyFlow, got {other:?}"),
        }

        // Still alive: a torn-down session would silently drop this (gone
        // from the registry), so a further chunk must still arrive.
        hub.deliver_pty_output(&session_id, machine_id, chunk.clone(), false);
        assert_eq!(
            rx.recv()
                .await
                .expect("session still delivering after pausing")
                .len(),
            138
        );
    }

    /// Mirrors `close_tails_for_closes_only_the_owning_machines_tails`.
    #[tokio::test]
    async fn close_ptys_for_closes_only_the_owning_machines_ptys() {
        let hub = Hub::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (_sid_a1, mut rx_a1, _st_a1) = hub.open_pty(a);
        let (_sid_a2, mut rx_a2, _st_a2) = hub.open_pty(a);
        let (sid_b, mut rx_b, _st_b) = hub.open_pty(b);

        hub.close_ptys_for(a);

        assert!(rx_a1.recv().await.is_none(), "A's first pty must close");
        assert!(rx_a2.recv().await.is_none(), "A's second pty must close");

        hub.deliver_pty_output(&sid_b, b, b"still alive".to_vec(), false);
        assert_eq!(
            rx_b.recv().await.expect("B's pty still delivers"),
            b"still alive"
        );
    }

    /// Mirrors `a_foreign_machine_cannot_deliver_into_another_machines_tail`
    /// for PTY output.
    #[tokio::test]
    async fn a_foreign_machine_cannot_deliver_into_another_machines_pty() {
        let hub = Hub::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let (session_id, mut rx, _state) = hub.open_pty(owner);

        hub.deliver_pty_output(&session_id, other, b"spoof".to_vec(), false);
        assert!(
            rx.try_recv().is_err(),
            "a foreign machine must not deliver into another machine's pty"
        );

        hub.deliver_pty_output(&session_id, owner, b"real".to_vec(), false);
        assert_eq!(rx.recv().await.expect("owner delivers"), b"real");
    }

    /// Full channel despite flow control: `deliver_pty_output` must neither
    /// block (called from the never-stall gRPC inbound drain) nor silently
    /// drop the byte -- it must tear the session down while everything
    /// already buffered stays intact to drain.
    #[tokio::test]
    async fn deliver_pty_output_full_channel_tears_down_the_session_instead_of_dropping_or_blocking(
    ) {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        hub.register(machine_id, tx);

        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        // One byte per chunk keeps the total (PTY_CHANNEL_CAP bytes) far
        // under PTY_HIGH_WATER, so no pause frame confounds this test --
        // only the teardown's PtyClose is expected on `agent_rx`.
        for _ in 0..argus_common::PTY_CHANNEL_CAP {
            hub.deliver_pty_output(&session_id, machine_id, vec![0u8], false);
        }

        // Channel now exactly full. This call must return immediately (a
        // hang would fail the test via timeout), not drop or await room.
        hub.deliver_pty_output(&session_id, machine_id, vec![0u8], false);

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), agent_rx.recv())
            .await
            .expect("a frame within 1s")
            .expect("channel open")
            .expect("ok frame");
        match frame.payload {
            Some(server_frame::Payload::PtyClose(c)) => assert_eq!(c.session_id, session_id),
            other => panic!("expected PtyClose from the full-channel teardown, got {other:?}"),
        }

        // Every chunk that made it in before the fill is still there --
        // nothing dropped -- and once drained, the sender (removed by
        // teardown) is gone, so the channel closes instead of hanging.
        let mut drained = 0;
        while let Some(chunk) = rx.recv().await {
            assert_eq!(chunk, vec![0u8]);
            drained += 1;
        }
        assert_eq!(
            drained,
            argus_common::PTY_CHANNEL_CAP,
            "no buffered byte was dropped by the teardown"
        );
    }

    /// A dropped `PtyFlow{paused:true}` send (the agent's 16-slot outbound
    /// channel is fullest exactly under load) must NOT leave `paused`
    /// latched true with the frame never sent -- that would suppress every
    /// future pause while the agent keeps flooding. Rolls back to false so
    /// the next byte crossing high-water retries.
    #[tokio::test]
    async fn a_dropped_pause_frame_does_not_latch_paused_with_no_recovery() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        // Capacity-1 channel, pre-filled: the pause attempt's try_send below
        // has no room and must fail.
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(1);
        tx.try_send(Ok(ServerFrame {
            stream_id: 0,
            payload: None,
        }))
        .expect("prefill the one slot");
        hub.register(machine_id, tx);

        let (session_id, _rx, state) = hub.open_pty(machine_id);

        // Cross high-water: the pause send fails (channel full).
        let chunk = vec![0u8; argus_common::PTY_HIGH_WATER + 1];
        hub.deliver_pty_output(&session_id, machine_id, chunk, false);

        assert!(
            !state.inner.lock().unwrap().paused,
            "a dropped pause send must roll back rather than latch paused=true \
             forever with the agent never actually told"
        );

        // Drain the prefilled slot so the retry has room; the retry must get through.
        let _ = agent_rx.recv().await.expect("drain the prefill frame");
        hub.deliver_pty_output(&session_id, machine_id, vec![0u8], false);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), agent_rx.recv())
            .await
            .expect("a retried PtyFlow within 1s -- no recovery from the dropped send")
            .expect("channel open")
            .expect("ok frame");
        match frame.payload {
            Some(server_frame::Payload::PtyFlow(f)) => {
                assert!(f.paused, "the retried pause must now get through")
            }
            other => panic!("expected PtyFlow, got {other:?}"),
        }
    }

    /// The eof path, hardened against ambiguity with a genuinely empty,
    /// non-eof chunk (see `deliver_pty_output`'s doc for the full contract).
    #[tokio::test]
    async fn deliver_pty_output_eof_is_observed_unambiguously_by_the_consumer() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        hub.deliver_pty_output(&session_id, machine_id, Vec::new(), false);
        assert!(
            rx.try_recv().is_err(),
            "an empty non-eof chunk must never reach the channel"
        );

        // The common case: the shell's final read returns its last bytes and EOF together.
        hub.deliver_pty_output(&session_id, machine_id, b"bye\n".to_vec(), true);
        let data = rx.recv().await.expect("the real bytes arrive first");
        assert_eq!(data, b"bye\n");
        let marker = rx.recv().await.expect("the eof marker follows");
        assert!(
            marker.is_empty(),
            "eof is signalled by an empty chunk, and only by one"
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing follows the eof marker for this session"
        );
    }

    /// Eof with no trailing data must still produce exactly the marker -- no
    /// phantom empty chunk first, which would be indistinguishable from a
    /// real empty non-eof chunk.
    #[tokio::test]
    async fn deliver_pty_output_eof_with_no_trailing_data_sends_only_the_marker() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        hub.deliver_pty_output(&session_id, machine_id, Vec::new(), true);
        let marker = rx.recv().await.expect("eof marker arrives");
        assert!(marker.is_empty());
        assert!(rx.try_recv().is_err(), "nothing else follows");
    }
}
