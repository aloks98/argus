//! In-memory session hub (Docker slice): the live agent-connection registry, the
//! latest Docker and systemd snapshots per machine, and the command_id -> waiter
//! correlation map for synchronous verb results. Shared as an `Arc<Hub>` between
//! the gRPC surface (which fills it from the Session stream) and the HTTP surface
//! (which reads snapshots and dispatches verbs). All state is in-memory and
//! re-derived on reconnect — consistent with the stateless single-replica control
//! plane.

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
/// (which fills the buffer) and the WS output task (which drains it).
///
/// `buffered` and `paused` are guarded as ONE unit (a single `Mutex`, not two
/// independent atomics) so a fill (`deliver_pty_output`) racing a drain
/// (`on_pty_drained`) can never compute "should we flip the edge" from a torn
/// combination of the two — e.g. reading a byte count one task just updated
/// but a `paused` flag the other hasn't caught up to yet. Review fix: the
/// original independent-atomics version let a dropped `PtyFlow` send (the
/// agent's shared outbound channel is only 16 deep and carries everything)
/// leave this pair permanently out of sync with what the agent actually
/// knows — either latching `paused` true forever with the frame never sent
/// (suppressing every future pause attempt), or, worse, recording locally
/// "resumed" while the agent is still genuinely parked, wedging the terminal
/// with no error, no eof, and no close until the 30-minute idle timer.
pub struct PtyFlowState {
    inner: Mutex<PtyFlowInner>,
}

struct PtyFlowInner {
    buffered: usize,
    paused: bool,
}

/// One live agent connection: its outbound frame channel plus a per-connection
/// counter for the fresh non-zero `stream_id` each dispatched command gets. The
/// `epoch` distinguishes successive connections of the same machine so a
/// lingering old session's teardown can't evict a freshly-reconnected one.
///
/// `shutdown` is the other half of that story: the epoch guard stops a STALE
/// disconnect from evicting a NEWER registration, but by itself does nothing
/// to stop the STALE session's server-side loop from continuing to run --
/// left alone, it would keep pumping heartbeats (marking the machine online)
/// while no longer reachable via `conns` for verb/log/terminal dispatch. Each
/// `register` hands the caller a clone of ITS OWN handle's `shutdown`, for the
/// session loop to select on; when a later `register` for the same machine
/// replaces this handle, it fires `shutdown` so that loop notices promptly
/// and exits through the normal teardown path instead of lingering
/// alive-but-unroutable.
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
    /// request_id -> the SSE sink for that tail, with the machine it belongs to.
    /// A stream sink, unlike `pending`'s one-shot.
    ///
    /// Filled by `deliver_chunk` (the gRPC frame handler) and read/removed via
    /// `open_tail`/`close_tail` (the HTTP SSE handler).
    tails: Mutex<HashMap<String, (Uuid, mpsc::Sender<LogChunk>)>>,
    /// session_id -> (machine, byte sink for the WS handler, flow state).
    #[allow(clippy::type_complexity)]
    ptys: Mutex<HashMap<String, (Uuid, mpsc::Sender<Vec<u8>>, Arc<PtyFlowState>)>>,
    epoch_counter: AtomicU64,
}

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

    /// Register a live connection, returning its epoch and a shutdown signal
    /// for the CALLER's own session loop to select on. A re-register for the
    /// same machine replaces the old handle (last writer wins) -- and, unlike
    /// before, fires the OLD handle's shutdown signal on the way out, so that
    /// superseded session's loop wakes promptly and exits through the normal
    /// teardown path (rather than lingering alive-but-unroutable: still
    /// pumping heartbeats server-side with no way for the hub to route verbs/
    /// logs/terminal to it anymore).
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

    /// Whether a machine currently has a live registered Session. Used by
    /// the terminal WS handler to tell apart two failure modes that look
    /// identical from the receiving end (a PTY's output channel just closes
    /// with no `eof` marker): the whole agent connection ending
    /// (`close_ptys_for`, called from `grpc.rs` strictly AFTER `unregister`)
    /// versus `deliver_pty_output`'s single-session overrun backstop
    /// (`close_pty`, which never touches `conns`). By the time a caller
    /// observes the channel closed, `unregister` for a real disconnect has
    /// already run, so this is a reliable way to distinguish the two.
    pub fn is_connected(&self, machine_id: Uuid) -> bool {
        self.conns.lock().unwrap().contains_key(&machine_id)
    }

    /// Replace the cached container snapshot for a machine.
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

    /// Replace the cached unit snapshot for a machine.
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

    /// How many of a machine's cached units are in the `failed` state. Counted
    /// here rather than in the handler so the fleet query stays one cheap read
    /// under a single lock, and never clones the snapshot.
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
        // Extract the channel + stream_id under the lock, then release it before
        // the async send (never hold a std Mutex guard across an await).
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

    /// Deliver a result to the waiter for this command_id, but only if the
    /// resolving session is the machine the command was dispatched to — a
    /// result from any other authenticated session is ignored and the waiter
    /// left intact. No-op if there is no waiter.
    pub fn complete(&self, command_id: &str, machine_id: Uuid, result: CommandResult) {
        let mut pending = self.pending.lock().unwrap();
        if pending.get(command_id).map(|(m, _)| *m) == Some(machine_id) {
            if let Some((_, tx)) = pending.remove(command_id) {
                let _ = tx.send(result);
            }
        }
    }

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

    /// Close every tail opened against this machine. Called when its agent
    /// session ends: the agent-side tails are already aborted on teardown, so
    /// these server-side sinks would otherwise hang their SSE streams open with
    /// no eof, showing a frozen "live" tail in the browser forever.
    pub fn close_tails_for(&self, machine_id: Uuid) {
        self.tails
            .lock()
            .unwrap()
            .retain(|_, (owner, _)| *owner != machine_id);
    }

    /// Deliver a chunk, but only from the machine the tail was opened against —
    /// the same trust boundary `complete()` enforces for command results. An
    /// `eof` chunk is delivered and then closes the sink.
    ///
    /// Called by the gRPC frame handler as `LogChunk` frames arrive on the
    /// agent's Session stream.
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

    /// Sent by the HTTP SSE handler when a tail is opened.
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

    /// Sent by the HTTP SSE handler's `TailGuard` when the browser disconnects.
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

    /// Mint a session, register its byte sink + flow state, return them to the
    /// WS handler. Mirrors `open_tail`. Called by `terminal::handle` on every
    /// WebSocket upgrade.
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

    /// Route an agent `PtyOutput` into its session's buffer. NON-BLOCKING: the
    /// caller is the gRPC inbound drain, which must never stall the shared
    /// Session. Accounts bytes and, on crossing the high-water mark, emits
    /// `PtyFlow{paused:true}` so the agent throttles. A full channel is the
    /// unreachable-by-sizing defensive teardown (never a dropped byte).
    ///
    /// Channel contract (this is what the WS handler must consume): the
    /// channel carries `Vec<u8>` items, and an EMPTY item is *exclusively* the
    /// eof marker -- a real chunk is never forwarded empty. That is enforced
    /// structurally right here, not by convention on the agent side: a
    /// `PtyOutput{data: [], eof: false}` (genuinely nothing to deliver, not
    /// eof) is dropped before it would ever reach the channel, so it can never
    /// be confused with the marker below. The only way an empty `Vec` reaches
    /// the receiver is the explicit eof push.
    pub fn deliver_pty_output(&self, session_id: &str, machine_id: Uuid, data: Vec<u8>, eof: bool) {
        let (sender, state) = {
            let ptys = self.ptys.lock().unwrap();
            match ptys.get(session_id) {
                Some((owner, tx, st)) if *owner == machine_id => (tx.clone(), st.clone()),
                _ => return,
            }
        };
        // Nothing to deliver and not eof: dropping this is free (zero bytes
        // change nothing on screen) and is what keeps the invariant above
        // true -- forwarding it would be indistinguishable from eof.
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
            // The WS handler observes this empty final frame and closes. Same
            // never-drop-never-block rule applies to the marker itself: if it
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
        // Account the bytes and decide the pause edge as ONE guarded step
        // (see `PtyFlowState`'s doc comment), so a concurrent drain can never
        // observe a torn buffered/paused pair.
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
            // The frame never reached the agent (its shared 16-slot outbound
            // channel is fullest exactly under load, i.e. exactly when this
            // fires). Roll the local flag back to false rather than leaving
            // it latched true with nobody told: the agent keeps flooding
            // regardless (it was never actually paused), so the very next
            // byte that crosses high-water will retry this send -- `deliver_
            // pty_output` MUST stay non-blocking/non-awaiting here (it runs
            // on the never-stall gRPC inbound drain), so this is a best-
            // effort retry-on-next-edge rather than a guaranteed delivery
            // (contrast `on_pty_drained`'s resume path below, which CAN
            // afford to await and so gets a stronger guarantee).
            let mut inner = state.inner.lock().unwrap();
            inner.paused = false;
        }
    }

    /// Called by the WS handler after it writes `len` bytes to the socket, so
    /// the byte counter reflects what is still buffered. On dropping below the
    /// low-water mark, emits `PtyFlow{paused:false}`.
    ///
    /// Async (unlike `deliver_pty_output`): the caller is the WS outbound
    /// task, which already awaits the socket send, so this can afford to
    /// await room on the agent's outbound channel too -- a GUARANTEED
    /// delivery rather than a best-effort `try_send`. That matters
    /// specifically for the resume edge: a dropped resume is what wedges a
    /// terminal forever (the agent stays parked, so no more output ever
    /// arrives to give a pause edge -- and thus no more `deliver_pty_output`
    /// calls -- a chance to retry), whereas a dropped pause naturally gets
    /// retried by the next byte the still-flooding agent produces. Making
    /// resume undroppable (as long as the agent is still connected at all)
    /// closes that gap at the source instead of hoping for a retry that may
    /// never come.
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

    /// Non-blocking `PtyFlow` send (from `deliver_pty_output`'s sync,
    /// never-await accounting path).
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

    /// Guaranteed-delivery `PtyFlow` send (awaits room on the outbound
    /// channel instead of failing immediately). Used only by `on_pty_drained`
    /// -- see its doc comment for why the resume edge specifically needs
    /// this instead of `try_send_pty_flow`.
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

    /// Forwards a keystroke frame from `terminal::handle`'s inbound loop.
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

    /// Forwards a resize control frame from `terminal::handle`'s inbound loop.
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
        // The agent re-sends a full snapshot every tick and on reconnect; a
        // resolved failure must not linger in the cache.
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
        // Machine reconnects: a second register replaces the first.
        let (tx2, _rx2) = mpsc::channel(1);
        let (_epoch2, _shutdown2) = hub.register(m, tx2);
        // The OLD session's teardown must not remove the new connection.
        hub.unregister(m, epoch1);
        assert!(
            hub.conns.lock().unwrap().contains_key(&m),
            "newer connection must survive a stale unregister"
        );
    }

    #[test]
    fn a_normal_single_session_disconnect_still_unregisters() {
        // No supersession involved at all: a lone session's own teardown
        // must still remove its handle -- the epoch guard added for the
        // stale-disconnect case must not accidentally make unregister a
        // no-op in the common case.
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
        // The exact inversion of the bug: A connects, then B connects for the
        // same machine (replacing A). A's shutdown signal must fire so its
        // session loop can exit, and the hub must still route to B -- its tx
        // works and the registry is non-empty.
        let hub = Hub::new();
        let m = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel(1);
        let (epoch_a, shutdown_a) = hub.register(m, tx_a);
        let (tx_b, mut rx_b) = mpsc::channel(4);
        let (epoch_b, _shutdown_b) = hub.register(m, tx_b);
        assert_ne!(epoch_a, epoch_b, "each registration gets a fresh epoch");

        // A's shutdown signal fired: a permit is already stored, so this
        // resolves immediately rather than hanging.
        tokio::time::timeout(std::time::Duration::from_millis(100), shutdown_a.notified())
            .await
            .expect("the replaced session's shutdown signal must fire promptly");

        // The hub still routes to B: dispatch succeeds and B's channel
        // receives the frame.
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
        // Wrong machine: ignored, waiter still pending.
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
        // Owning machine: resolves it.
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

        // B's tail is untouched: still delivers.
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

        // Push just over the high-water mark -> a pause frame is emitted.
        let chunk = vec![0u8; argus_common::PTY_HIGH_WATER + 1];
        hub.deliver_pty_output(&session_id, machine_id, chunk, false);
        let paused = recv_pty_flow(&mut agent_rx).await;
        assert!(
            paused,
            "crossing high-water must emit PtyFlow{{paused:true}}"
        );

        // Drain below the low-water mark -> a resume frame is emitted.
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

    /// The measured shape of the review's live-firehose reproduction (`seq 1
    /// 200000` in a real terminal: 1,488,985 bytes across 10,818 `PtyOutput`
    /// frames, ~138 bytes/chunk) -- NOT the 64 KiB the original sizing
    /// assumed. Feeds many ~138-byte chunks, without draining the receiver
    /// (mirrors a browser renderer that can't keep up), and asserts the BYTE
    /// watermark binds before the message-count channel cap does: a
    /// `PtyFlow{paused:true}` must come out, and the session must still be
    /// alive afterward -- not torn down by the full-channel backstop.
    ///
    /// This must fail against the old `PTY_CHANNEL_CAP = 2048`: at 138
    /// bytes/chunk the channel fills (2048 messages) at ~283 KiB, well below
    /// `PTY_HIGH_WATER` (1 MiB — reached only after ~7,600 chunks), so the
    /// full-channel teardown fires FIRST and this test's `PtyClose` branch
    /// panics instead of ever seeing a `PtyFlow`.
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

        // Still alive: a session the backstop tore down would silently drop
        // this (the entry is gone from the registry), so a further chunk
        // must still reach the receiver.
        hub.deliver_pty_output(&session_id, machine_id, chunk.clone(), false);
        assert_eq!(
            rx.recv()
                .await
                .expect("session still delivering after pausing")
                .len(),
            138
        );
    }

    /// Mirrors `close_tails_for_closes_only_the_owning_machines_tails`: closing
    /// one machine's PTY sessions must not touch another machine's.
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

        // B's pty is untouched: still delivers.
        hub.deliver_pty_output(&sid_b, b, b"still alive".to_vec(), false);
        assert_eq!(
            rx_b.recv().await.expect("B's pty still delivers"),
            b"still alive"
        );
    }

    /// Mirrors `a_foreign_machine_cannot_deliver_into_another_machines_tail`:
    /// the same trust boundary applies to PTY output -- a session belongs to
    /// the machine it was opened against, and any other authenticated agent
    /// must not be able to inject bytes into it.
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

    /// The highest-risk branch: when the per-session channel is full despite
    /// flow control, `deliver_pty_output` must neither block (this is called
    /// from the never-stall gRPC inbound drain) nor silently drop the byte --
    /// it must tear the session down (dispatch `PtyClose`, drop the sender so
    /// the consumer observes disconnection) while everything already buffered
    /// stays intact for the consumer to drain.
    #[tokio::test]
    async fn deliver_pty_output_full_channel_tears_down_the_session_instead_of_dropping_or_blocking(
    ) {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (tx, mut agent_rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        hub.register(machine_id, tx);

        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        // Fill the channel to capacity WITHOUT draining `rx`. One byte per
        // chunk keeps the total (PTY_CHANNEL_CAP bytes) far under
        // PTY_HIGH_WATER, so no PtyFlow pause frame is emitted along the way
        // -- the only frame this test expects on `agent_rx` is the teardown's
        // PtyClose.
        for _ in 0..argus_common::PTY_CHANNEL_CAP {
            hub.deliver_pty_output(&session_id, machine_id, vec![0u8], false);
        }

        // The channel is now exactly full. This delivery must return
        // immediately (a hang here would mean the call blocked, which alone
        // would fail the test via timeout) rather than either dropping the
        // byte silently or awaiting room.
        hub.deliver_pty_output(&session_id, machine_id, vec![0u8], false);

        // Teardown must have dispatched PtyClose to the agent.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), agent_rx.recv())
            .await
            .expect("a frame within 1s")
            .expect("channel open")
            .expect("ok frame");
        match frame.payload {
            Some(server_frame::Payload::PtyClose(c)) => assert_eq!(c.session_id, session_id),
            other => panic!("expected PtyClose from the full-channel teardown, got {other:?}"),
        }

        // Every chunk that DID make it in before the channel filled is still
        // there -- nothing was dropped -- and once fully drained the sender
        // (removed from the registry by the teardown) is gone, so the
        // channel closes instead of hanging open forever.
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

    /// Finding 2: a dropped `PtyFlow{paused:true}` send (the agent's shared
    /// 16-slot outbound channel is fullest exactly under load, i.e. exactly
    /// when a pause would fire) must NOT leave the local `paused` flag
    /// latched true with the frame never having gone out -- that would
    /// suppress every future pause attempt (the CAS-style edge check only
    /// fires when the old value was false) while the agent, never actually
    /// told to pause, keeps flooding regardless. The fix rolls `paused` back
    /// to false on send failure so the very next byte crossing high-water
    /// retries.
    ///
    /// This must fail against the old fire-and-forget code, which did
    /// `state.paused.swap(true, ..)` unconditionally BEFORE attempting the
    /// send and never rolled back on failure.
    #[tokio::test]
    async fn a_dropped_pause_frame_does_not_latch_paused_with_no_recovery() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        // A capacity-1 outbound channel, pre-filled: the pause attempt's
        // try_send below has no room and must fail.
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

        // Drain the prefilled slot so the retry has room, then feed one more
        // byte over high-water: the retry this time must get through.
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

    /// The eof path, after hardening the empty-`Vec` marker against ambiguity
    /// with a genuinely empty, non-eof chunk (which must never be forwarded at
    /// all -- see `deliver_pty_output`'s doc comment for the full contract).
    #[tokio::test]
    async fn deliver_pty_output_eof_is_observed_unambiguously_by_the_consumer() {
        let hub = Hub::new();
        let machine_id = Uuid::new_v4();
        let (session_id, mut rx, _state) = hub.open_pty(machine_id);

        // A genuinely empty, non-eof chunk must be dropped, not forwarded --
        // forwarding it would be indistinguishable from the eof marker below.
        hub.deliver_pty_output(&session_id, machine_id, Vec::new(), false);
        assert!(
            rx.try_recv().is_err(),
            "an empty non-eof chunk must never reach the channel"
        );

        // Real output arriving together with eof (the common case: the
        // shell's final read returns its last bytes and EOF together).
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

    /// Eof with no trailing data (the shell exits with nothing left to flush)
    /// must still produce exactly the marker -- no phantom empty chunk before
    /// it, since that would already have been the (indistinguishable) case
    /// this task's fix rules out.
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
