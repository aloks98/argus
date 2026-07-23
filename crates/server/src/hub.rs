//! In-memory session hub (Docker slice): the live agent-connection registry, the
//! latest Docker and systemd snapshots per machine, and the command_id -> waiter
//! correlation map for synchronous verb results. Shared as an `Arc<Hub>` between
//! the gRPC surface (which fills it from the Session stream) and the HTTP surface
//! (which reads snapshots and dispatches verbs). All state is in-memory and
//! re-derived on reconnect — consistent with the stateless single-replica control
//! plane.

use argus_proto::v1::{server_frame, Command, CommandResult, Container, ServerFrame, Unit, Verb};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

/// One live agent connection: its outbound frame channel plus a per-connection
/// counter for the fresh non-zero `stream_id` each dispatched command gets. The
/// `epoch` distinguishes successive connections of the same machine so a
/// lingering old session's teardown can't evict a freshly-reconnected one.
struct ConnHandle {
    tx: mpsc::Sender<Result<ServerFrame, Status>>,
    next_stream_id: AtomicU64,
    epoch: u64,
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
    epoch_counter: AtomicU64,
}

impl Hub {
    pub fn new() -> Hub {
        Hub::default()
    }

    /// Register a live connection, returning its epoch. A re-register for the
    /// same machine replaces the old handle (last writer wins).
    pub fn register(&self, machine_id: Uuid, tx: mpsc::Sender<Result<ServerFrame, Status>>) -> u64 {
        let epoch = self.epoch_counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.conns.lock().unwrap().insert(
            machine_id,
            ConnHandle {
                tx,
                next_stream_id: AtomicU64::new(1),
                epoch,
            },
        );
        epoch
    }

    /// Remove a connection only if it is still the one with this `epoch` — a
    /// stale disconnect must not evict a newer reconnection.
    pub fn unregister(&self, machine_id: Uuid, epoch: u64) {
        let mut conns = self.conns.lock().unwrap();
        if conns.get(&machine_id).map(|h| h.epoch) == Some(epoch) {
            conns.remove(&machine_id);
        }
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
        issued_by: String,
    ) -> Result<(), DispatchError> {
        // Extract the channel + stream_id under the lock, then release it before
        // the async send (never hold a std Mutex guard across an await).
        let (tx, stream_id) = {
            let conns = self.conns.lock().unwrap();
            let handle = conns.get(&machine_id).ok_or(DispatchError::NotConnected)?;
            let stream_id = handle.next_stream_id.fetch_add(1, Ordering::Relaxed);
            (handle.tx.clone(), stream_id)
        };
        let frame = ServerFrame {
            stream_id,
            payload: Some(server_frame::Payload::Command(Command {
                command_id,
                verb: verb as i32,
                target,
                issued_by,
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
        let epoch1 = hub.register(m, tx1);
        // Machine reconnects: a second register replaces the first.
        let (tx2, _rx2) = mpsc::channel(1);
        let _epoch2 = hub.register(m, tx2);
        // The OLD session's teardown must not remove the new connection.
        hub.unregister(m, epoch1);
        assert!(
            hub.conns.lock().unwrap().contains_key(&m),
            "newer connection must survive a stale unregister"
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
                "anonymous".into(),
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
            "anonymous".into(),
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
}
