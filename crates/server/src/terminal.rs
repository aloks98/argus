//! The browser side of the interactive terminal: a WebSocket that the server
//! upgrades, mints a `session_id` for, and bridges to the agent's PTY through
//! the `Hub`. `Binary` frames are keystrokes; `Text` frames are a tiny JSON
//! control channel (resize). Output is flow-controlled per session. A keystroke
//! resets the idle timer. The periodic Ping is a liveness *probe*, not a
//! detector: we do not track Pongs, so a dead peer is only noticed when a
//! socket write finally errors (on a half-open TCP connection the kernel can
//! take ~15 min to give up). The 30-minute idle timer is the real backstop.

use crate::auth::AuthUser;
use crate::http::AppState;
use crate::hub::Hub;
use crate::repo::{self, Identity};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

pub async fn terminal_ws(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    AuthUser(identity): AuthUser,
    ws: WebSocketUpgrade,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin, host) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    ws.on_upgrade(move |socket| handle(state, id, identity, socket))
}

/// Same-origin check for the terminal's WS upgrade. Browsers apply neither
/// same-origin policy nor CORS preflight to a WebSocket handshake, so
/// without this any page the operator's browser has open could silently
/// open a root shell to a machine -- the UUID in the path is an accidental
/// capability token today, and a future cookie-based OIDC session would
/// make it worse by attaching real credentials to a cross-site request.
///
/// Deliberately reuses the request's own `Host` header rather than a
/// configured allowlist: Traefik forwards the browser's original `Host`
/// unchanged to the upstream, so comparing `Origin`'s authority against
/// `Host` answers "did this request come from a page this server itself
/// served" with no new config surface. A real browser ALWAYS sends `Origin`
/// on a WS handshake, so a MISSING `Origin` means a non-browser client (e.g.
/// `websocat`/`curl`) and is let through; only a PRESENT and MISMATCHED
/// `Origin` is rejected.
fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let (Some(origin), Some(host)) = (origin, host) else {
        return true;
    };
    // Strip the scheme (`https://`, `http://`, `wss://`, ...) to compare
    // against the schemeless `Host` header's authority.
    let origin_authority = origin
        .split_once("://")
        .map_or(origin, |(_, authority)| authority);
    origin_authority.eq_ignore_ascii_case(host)
}

/// Distinct reasons the three concurrent loops in `handle` below can end a
/// session, each mapped to a WS close code + human-readable text sent to the
/// browser: a bare `sink.close()` with no code/reason would make shell-exit,
/// idle-timeout, agent-disconnect and overrun all read as a generic "session
/// closed" in the UI. `TerminalView` already prefers a non-empty
/// `CloseEvent.reason` over its own fallback text, so this is purely a
/// server-side change.
enum CloseReason {
    /// The agent sent the `eof` marker: the shell exited on its own.
    ShellExited,
    /// `deliver_pty_output`'s full-channel backstop tore down just this ONE
    /// session; the agent connection itself stayed up.
    Overrun,
    /// The agent's whole Session ended (redeploy, reconnect, crash):
    /// `close_ptys_for` closed every PTY for the machine, this one included.
    AgentDisconnected,
    /// No keystroke for `TERMINAL_IDLE_SECS`.
    IdleTimeout,
    /// The browser closed its side (tab/nav) or sent a WS Close frame; it
    /// already knows why it closed, so this text is never actually read.
    ClientInitiated,
    /// A send to the browser's own socket failed -- the peer is presumably
    /// already gone, so nothing will receive this close frame either.
    SendFailed,
}

impl CloseReason {
    fn frame(&self) -> (u16, &'static str) {
        match self {
            CloseReason::ShellExited => (1000, "shell exited"),
            CloseReason::Overrun => (1011, "terminal closed: output overrun"),
            CloseReason::AgentDisconnected => (1001, "session closed (agent disconnected)"),
            CloseReason::IdleTimeout => (1000, "closed after 30 min idle"),
            CloseReason::ClientInitiated => (1000, "session closed"),
            CloseReason::SendFailed => (1006, "session closed"),
        }
    }
}

/// Outcome of the ordering-sensitive dispatch-then-audit sequence in
/// `open_and_audit`. Two distinct failure states because they need different
/// teardown: a dispatch failure never reached the agent, so there is nothing
/// to send `PtyClose` for; an audit failure happened after a live PTY was
/// actually opened, so the caller still owes it a `PtyClose`.
enum OpenOutcome {
    /// The agent was offline (or gone) — `PtyOpen` never reached it. The
    /// session was minted and already unwound (`close_pty` already called).
    DispatchFailed,
    /// `PtyOpen` reached the agent, but the `terminal.open` audit write
    /// failed. Carries `session_id` so the caller can still send `PtyClose`
    /// for the PTY that IS live on the agent.
    AuditFailed { session_id: String },
    /// Both succeeded; the terminal is live.
    Ready {
        session_id: String,
        rx: mpsc::Receiver<Vec<u8>>,
    },
}

/// The ordering-sensitive part of opening a terminal session: mint a
/// session, dispatch `PtyOpen`, and only write the `terminal.open` audit row
/// if dispatch reached a live agent (dispatch before audit, fail closed —
/// mirrors `logs.open`). Factored out of `handle` so a test can
/// drive this exact sequence directly without a live WebSocket upgrade: a
/// regression that swapped the dispatch and audit calls would make this
/// function write an audit row for a session whose dispatch never reached
/// the agent, which the test in this module below asserts against.
async fn open_and_audit(state: &AppState, machine_id: Uuid, identity: &Identity) -> OpenOutcome {
    let (session_id, rx, _pty_state) = state.hub.open_pty(machine_id);
    if state
        .hub
        .send_pty_open(machine_id, session_id.clone(), 80, 24)
        .await
        .is_err()
    {
        state.hub.close_pty(&session_id);
        return OpenOutcome::DispatchFailed;
    }

    let command_id = Uuid::new_v4();
    if let Err(e) = repo::audit_command(
        &state.pool,
        repo::Actor::User(identity),
        "terminal.open",
        Some(machine_id),
        &session_id,
        command_id,
        "ok",
    )
    .await
    {
        tracing::error!(error = %e, "terminal: audit write failed; closing");
        return OpenOutcome::AuditFailed { session_id };
    }

    OpenOutcome::Ready { session_id, rx }
}

/// Cleans up a PTY session — `close_pty` plus a best-effort `PtyClose`
/// dispatch — on every exit past a successful `PtyOpen` dispatch, including a
/// panic unwind. A plain call after the joint `select!` below would miss a
/// panic and leak a live shell on the guest until agent redeploy. Mirrors
/// `TailGuard` (`crate::http`) for SSE log tails: `Drop` is sync, so the
/// `PtyClose` send is moved into a spawned task and only logged, never
/// awaited.
///
/// This is the ONLY place `close_pty`/`send_pty_close` are called for a
/// session that made it this far in `handle`, so teardown cannot run twice.
struct PtyCloseGuard {
    hub: Arc<Hub>,
    machine_id: Uuid,
    session_id: String,
}

impl Drop for PtyCloseGuard {
    fn drop(&mut self) {
        self.hub.close_pty(&self.session_id);
        let hub = self.hub.clone();
        let machine_id = self.machine_id;
        let session_id = self.session_id.clone();
        // Drop is sync; the close is a send, so it needs a task.
        tokio::spawn(async move {
            if let Err(e) = hub.send_pty_close(machine_id, session_id).await {
                tracing::debug!(error = ?e, "terminal: PtyClose not delivered (agent gone)");
            }
        });
    }
}

async fn handle(state: AppState, machine_id: Uuid, identity: Identity, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    let (session_id, mut rx) = match open_and_audit(&state, machine_id, &identity).await {
        OpenOutcome::DispatchFailed => {
            let _ = sink.send(Message::Text("agent not connected".into())).await;
            let _ = sink.close().await;
            return;
        }
        OpenOutcome::AuditFailed { session_id } => {
            // A PTY genuinely exists on the agent even though the audit
            // write failed; the guard's Drop sends PtyClose + close_pty even
            // though this scope never reaches the joint select! below.
            let _guard = PtyCloseGuard {
                hub: state.hub.clone(),
                machine_id,
                session_id,
            };
            let _ = sink.close().await;
            return;
        }
        OpenOutcome::Ready { session_id, rx } => (session_id, rx),
    };

    // From here a PTY genuinely exists on the agent and is fully audited.
    // `_guard`'s Drop is the single place `PtyClose` + `close_pty` happen for
    // this session — see its doc comment.
    let _guard = PtyCloseGuard {
        hub: state.hub.clone(),
        machine_id,
        session_id: session_id.clone(),
    };

    // Idle timer shared with the input loop: reset on each keystroke.
    let last_input = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));

    // Outbound: drains the session buffer to the socket (accounting bytes for
    // flow control) and emits periodic keepalive pings on the same sink. Both
    // must share one task since `WebSocket::split` gives a single sink;
    // `select!` multiplexes "next buffered chunk" and "ping tick". The ping is
    // what detects a crashed tab or slept laptop that never sent a clean
    // close -- without it the PTY and shell would live until agent redeploy.
    //
    // `sink` stays owned by `handle` (not moved into `outbound`) so it remains
    // usable after the joint select! below to send a final Close frame;
    // `outbound` only reborrows it, and that borrow ends once its future
    // completes or drops.
    let hub_out = state.hub.clone();
    let sid_out = session_id.clone();
    let sink_ref = &mut sink;
    let outbound = async move {
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick is immediate; skip it
        loop {
            tokio::select! {
                next = rx.recv() => {
                    let Some(bytes) = next else {
                        // The channel closed with no eof marker: either the
                        // whole agent Session ended (`close_ptys_for`, which
                        // grpc.rs only ever calls AFTER `unregister`) or THIS
                        // one session alone overran (`deliver_pty_output`'s
                        // full-channel backstop, which never touches
                        // `conns`). `is_connected` tells the two apart.
                        break if hub_out.is_connected(machine_id) {
                            CloseReason::Overrun
                        } else {
                            CloseReason::AgentDisconnected
                        };
                    };
                    if bytes.is_empty() {
                        break CloseReason::ShellExited; // eof marker
                    }
                    let n = bytes.len();
                    if sink_ref.send(Message::Binary(bytes.into())).await.is_err() {
                        break CloseReason::SendFailed;
                    }
                    hub_out.on_pty_drained(&sid_out, n).await;
                }
                _ = ping.tick() => {
                    // A dead peer fails the send (or the TCP keepalive kills the
                    // connection), which ends the loop and tears the session down.
                    if sink_ref.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break CloseReason::SendFailed;
                    }
                }
            }
        }
    };

    // Inbound: keystrokes (Binary) -> PtyInput (+reset idle); resize (Text) ->
    // PtyResize; pong resets nothing here (handled by the keepalive loop below).
    let hub_in = state.hub.clone();
    let sid_in = session_id.clone();
    let last_input_in = last_input.clone();
    let inbound = async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(data) => {
                    *last_input_in.lock().unwrap() = tokio::time::Instant::now();
                    if hub_in
                        .send_pty_input(machine_id, sid_in.clone(), data.into())
                        .await
                        .is_err()
                    {
                        return CloseReason::AgentDisconnected;
                    }
                }
                Message::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(r) = v.get("resize") {
                            // Clamped to a sane range: a malformed or hostile
                            // client can send 0 or near-u64::MAX. The agent
                            // already truncates to u16 and swallows ioctl
                            // errors, but there's no reason to forward
                            // nonsense to the PTY.
                            let cols = r
                                .get("cols")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(80)
                                .clamp(1, 1000) as u32;
                            let rows = r
                                .get("rows")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(24)
                                .clamp(1, 1000) as u32;
                            let _ = hub_in
                                .send_pty_resize(machine_id, sid_in.clone(), cols, rows)
                                .await;
                        }
                    }
                }
                Message::Close(_) => return CloseReason::ClientInitiated,
                _ => {}
            }
        }
        // The stream ended with no explicit Close frame (e.g. the TCP
        // connection just dropped): the browser side is gone either way.
        CloseReason::ClientInitiated
    };

    // Idle watchdog: close once TERMINAL_IDLE_SECS pass with no keystroke.
    let idle = async move {
        let window = Duration::from_secs(argus_common::TERMINAL_IDLE_SECS);
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if last_input.lock().unwrap().elapsed() >= window {
                break CloseReason::IdleTimeout;
            }
        }
    };

    // Whichever finishes first (socket closed, eof, overrun, agent gone, or
    // idle) ends the session; send the browser a distinct, human-readable
    // close reason before closing (see `CloseReason`). `sink` is still owned
    // by this scope (see the comment above `outbound`), so it's usable here
    // regardless of which branch won.
    let reason = tokio::select! {
        r = outbound => r,
        r = inbound => r,
        r = idle => r,
    };
    let (code, text) = reason.frame();
    let _ = sink
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: text.into(),
        })))
        .await;
    let _ = sink.close().await;

    // `_guard` drops here (and on any earlier return or panic unwind above),
    // sending PtyClose and calling close_pty exactly once.
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sqlx::PgPool;

    fn app_state_with_hub(pool: PgPool) -> (AppState, Arc<Hub>) {
        let hub = Arc::new(Hub::new());
        let oidc = Arc::new(crate::config::OidcConfig {
            issuer: "https://idp.invalid".into(),
            client_id: "cid".into(),
            client_secret: "secret".into(),
            required_role: crate::config::RequiredRole::Named("argus-admin".into()),
            roles_claim: "groups".into(),
            scopes: vec!["openid".into()],
            public_url: "http://localhost:8080".into(),
            ca_cert_path: None,
        });
        // Never triggers discovery: building the client is local-only, so
        // this is cheap and touches no network.
        let oidc_client = Arc::new(
            crate::auth::oidc::OidcClient::new(oidc.clone()).expect("build test OIDC client"),
        );
        (
            AppState {
                pool,
                hub: hub.clone(),
                oidc: Some(oidc),
                cipher: Arc::new(
                    crate::crypto::FieldCipher::from_b64_key(
                        &base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
                    )
                    .expect("build test field cipher"),
                ),
                oidc_client: Some(oidc_client),
                public_url: "http://localhost:8080".into(),
                limiter: Arc::new(crate::auth::ratelimit::LoginLimiter::new()),
            },
            hub,
        )
    }

    fn test_identity() -> Identity {
        Identity {
            subject: "term-user".into(),
            email: Some("term-user@example.com".into()),
            display_name: None,
        }
    }

    /// Drives the REAL production entry point (`open_and_audit`, the exact
    /// function `handle` calls before ever touching the socket) against an
    /// offline machine. Deliberately NOT a test that calls
    /// `Hub::open_pty`/`send_pty_open` directly and asserts no audit row
    /// exists -- that would be trivially true regardless of
    /// `open_and_audit`'s internal ordering. By calling the shared function
    /// itself, a regression that swapped its dispatch and audit calls would
    /// leave a row behind and fail this test.
    #[sqlx::test]
    async fn open_and_audit_dispatches_before_auditing(pool: PgPool) -> anyhow::Result<()> {
        let machine_id: Uuid = sqlx::query!(
            "INSERT INTO machines (machine_id, hostname, status) VALUES ('term-offline','h','offline') RETURNING id"
        ).fetch_one(&pool).await?.id;
        let (state, _hub) = app_state_with_hub(pool.clone());

        let outcome = open_and_audit(&state, machine_id, &test_identity()).await;
        assert!(
            matches!(outcome, OpenOutcome::DispatchFailed),
            "no agent connected -> dispatch must fail"
        );

        let rows = sqlx::query!(
            "SELECT count(*) as n FROM audit_log WHERE machine_id=$1 AND action='terminal.open'",
            machine_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            rows.n,
            Some(0),
            "open_and_audit returned before writing the terminal.open row on a failed \
             dispatch -- swapping the dispatch and audit calls inside it would leave a row here"
        );

        Ok(())
    }

    #[test]
    fn origin_allowed_accepts_a_same_origin_request() {
        assert!(origin_allowed(
            Some("https://argus.lab.example"),
            Some("argus.lab.example")
        ));
        // Host header carries a port; Origin must match it exactly.
        assert!(origin_allowed(
            Some("http://localhost:8080"),
            Some("localhost:8080")
        ));
    }

    #[test]
    fn origin_allowed_rejects_a_mismatched_origin() {
        assert!(!origin_allowed(
            Some("https://evil.example"),
            Some("argus.lab.example")
        ));
        // Same host, different port -- browsers treat this as a different
        // origin, and so must this check.
        assert!(!origin_allowed(
            Some("http://argus.lab.example:1234"),
            Some("argus.lab.example")
        ));
    }

    #[test]
    fn origin_allowed_rejects_the_opaque_null_origin() {
        // Sent by e.g. a sandboxed iframe or a `file://` page -- never a
        // legitimate same-origin browser request.
        assert!(!origin_allowed(Some("null"), Some("argus.lab.example")));
    }

    #[test]
    fn origin_allowed_lets_a_missing_origin_through() {
        // A missing Origin is let through -- see `origin_allowed`'s doc.
        assert!(origin_allowed(None, Some("argus.lab.example")));
        assert!(origin_allowed(None, None));
    }

    #[test]
    fn origin_allowed_is_case_insensitive_on_the_host() {
        assert!(origin_allowed(
            Some("https://Argus.Lab.Example"),
            Some("argus.lab.example")
        ));
    }
}
