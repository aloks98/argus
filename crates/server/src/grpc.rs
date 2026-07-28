//! Agent-facing gRPC surface (PRD §4, §5).
//!
//! Served on the MetalLB LoadBalancer address with mTLS terminated here -- never
//! via Traefik (PRD §2.4). The rustls listener accepts OPTIONAL client auth
//! (Task 6): a client MAY present a cert, validated against the internal CA via
//! `client_ca_root`, but is not REQUIRED to, so `Enroll` (no client cert) keeps
//! working. `Session` (this task, Task 7) is the opposite: it REQUIRES a
//! presented, active agent cert and rejects the call otherwise --
//! `identity::agent_id_from_peer` turns the cert's CN into an `agent_id`, which
//! is cross-checked against `repo::cert_is_active`'s fingerprint lookup before
//! the bidi loop starts.

use crate::ca::CertAuthority;
use crate::config::Config;
use crate::hub::Hub;
use crate::identity;
use crate::repo::{self, AgentInfoRow, TokenCheck};
use anyhow::Result;
use argus_proto::v1::agent_service_server::{AgentService, AgentServiceServer};
use argus_proto::v1::{
    agent_frame, server_frame, AgentFrame, EnrollRequest, EnrollResponse, HelloAck, ServerFrame,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

/// The agent-facing gRPC service. `Enroll` signs CSRs against the internal CA;
/// `Session` runs the multiplexed bidi loop over an already-authenticated
/// client cert -> agent_id lookup (Task 6's `identity::agent_id_from_peer`).
pub struct AgentSvc {
    pub ca: Arc<CertAuthority>,
    pub pool: PgPool,
    pub hub: Arc<Hub>,
}

impl AgentSvc {
    pub fn new(ca: Arc<CertAuthority>, pool: PgPool, hub: Arc<Hub>) -> Self {
        Self { ca, pool, hub }
    }
}

#[tonic::async_trait]
impl AgentService for AgentSvc {
    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let req = request.into_inner();

        // The token check + the machine/cert writes below all happen inside
        // one transaction: if anything past the token check fails, the
        // rollback un-burns the (possibly single-use) token and removes any
        // partial machine/cert writes, so a retry sees a clean slate. Audit
        // rows for failures are written to the POOL (outside the tx) so they
        // survive the rollback -- every code path is audited, per CLAUDE.md.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal_error("beginning enroll transaction", &e.into()))?;

        // The token's `name` no longer has anywhere to go AS THE ACTOR: with
        // `actor` now a closed `Actor` enum, there is no principal to
        // attribute an unauthenticated `Enroll` call to but `Actor::System`
        // -- true of the denied/error paths below (invalid token, missing
        // info.machine_id, a failed write), since at that point a client
        // cert (the only other thing that could mint an `Actor::Agent`)
        // doesn't exist yet and no machine has been committed either. It is
        // NOT true of the success path further down: once `agent_id` is
        // committed inside this still-open transaction, that row mints
        // `Actor::Agent(agent_id)` instead (see the comment there). Either
        // way, `Actor::System` (denied/error) is still the only
        // server-verified fact those calls have this early, so every
        // `agent.enroll` row from here on carries the token name in `detail`
        // too (`audit_with_detail`) -- without that, "which join token was
        // used" becomes permanently unanswerable the moment the row is
        // written: `enrollment_tokens` keeps no per-use history, only a bare
        // `uses` counter.
        let (token_name, token_display_name, token_tags) =
            match repo::consume_enrollment_token(&mut *tx, &req.join_token)
                .await
                .map_err(|e| internal_error("checking enrollment token", &e))?
            {
                TokenCheck::Valid {
                    token_name,
                    display_name,
                    tags,
                } => (token_name, display_name, tags),
                TokenCheck::Invalid => {
                    drop(tx);
                    // No token was ever resolved here, so unlike the paths below
                    // there is no name to put in `detail` either.
                    repo::audit(
                        &self.pool,
                        repo::Actor::System,
                        "agent.enroll",
                        None,
                        "denied",
                    )
                    .await
                    .map_err(|e| internal_error("writing audit log", &e))?;
                    return Err(Status::unauthenticated(
                        "invalid or exhausted enrollment token",
                    ));
                }
            };

        let info = match &req.info {
            Some(info) if !info.machine_id.is_empty() => info,
            _ => {
                drop(tx);
                repo::audit_with_detail(
                    &self.pool,
                    repo::Actor::System,
                    "agent.enroll",
                    None,
                    "denied",
                    serde_json::json!({ "enrollment_token": token_name }),
                )
                .await
                .map_err(|e| internal_error("writing audit log", &e))?;
                return Err(Status::invalid_argument(
                    "info.machine_id is required for enrollment",
                ));
            }
        };

        let info_row = agent_info_row(info);

        // The three fallible write/sign steps share one rollback+audit-error
        // path: on any failure, roll back the tx (machine_id is necessarily
        // None afterward -- the machine upsert itself may have been rolled
        // back, and `audit_log.machine_id` is an FK) and audit "error" on the
        // pool before returning the mapped Status.
        let result: Result<(uuid::Uuid, crate::ca::SignedCert), Status> = async {
            let agent_id = repo::upsert_machine(&mut *tx, &info_row)
                .await
                .map_err(|e| internal_error("upserting machine", &e))?;

            repo::apply_token_identity(
                &mut *tx,
                agent_id,
                token_display_name.as_deref(),
                &token_tags,
            )
            .await
            .map_err(|e| internal_error("applying token identity", &e))?;

            let signed = self.ca.sign_csr(&req.csr_pem, agent_id).map_err(|e| {
                tracing::warn!(error = %e, "enroll: CSR rejected");
                Status::invalid_argument("malformed certificate signing request")
            })?;

            repo::insert_agent_cert(
                &mut *tx,
                agent_id,
                &signed.serial,
                &signed.fingerprint_hex,
                signed.not_before,
                signed.not_after,
            )
            .await
            .map_err(|e| internal_error("recording issued cert", &e))?;

            Ok((agent_id, signed))
        }
        .await;

        let (agent_id, signed) = match result {
            Ok(pair) => pair,
            Err(status) => {
                drop(tx);
                repo::audit_with_detail(
                    &self.pool,
                    repo::Actor::System,
                    "agent.enroll",
                    None,
                    "error",
                    serde_json::json!({ "enrollment_token": token_name }),
                )
                .await
                .map_err(|e| internal_error("writing audit log", &e))?;
                return Err(status);
            }
        };

        // By this point `agent_id` is committed inside the still-open `tx`, so
        // unlike the denied/error paths above, there IS a machine to attribute
        // this to: the agent enrolling itself. `Actor::Agent(agent_id)` alone
        // only records the CLAIM (`info.machine_id`, self-reported and not yet
        // cert-verified -- see `update_machine_inventory`'s doc comment); the
        // enrollment token's name in `detail` is what records the CREDENTIAL
        // that actually authorized it.
        repo::audit_with_detail(
            &mut *tx,
            repo::Actor::Agent(agent_id),
            "agent.enroll",
            Some(agent_id),
            "ok",
            serde_json::json!({ "enrollment_token": token_name }),
        )
        .await
        .map_err(|e| internal_error("writing audit log", &e))?;

        tx.commit()
            .await
            .map_err(|e| internal_error("committing enroll transaction", &e.into()))?;

        Ok(Response::new(EnrollResponse {
            client_cert_pem: signed.cert_pem,
            ca_cert_pem: self.ca.ca_cert_pem().to_string(),
            agent_id: agent_id.to_string(),
        }))
    }

    type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerFrame, Status>> + Send>>;

    async fn session(
        &self,
        request: Request<Streaming<AgentFrame>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        // AuthN: unlike `Enroll`, `Session` REQUIRES a client cert (PRD §5.4).
        // `client_auth_optional(true)` in `serve` below means `peer_certs()` is
        // only `Some` when the caller actually presented one.
        let certs = request
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;

        let agent_id = identity::agent_id_from_peer(&certs)
            .map_err(|_| Status::unauthenticated("bad client certificate"))?;

        // `agent_id_from_peer` above already proved `certs` is non-empty, so
        // the leaf is `&certs[0]` -- no need to re-check for an empty chain.
        // The fingerprint must be computed the same way `ca::sign_csr` /
        // `repo::insert_agent_cert` stored it at enrollment time: sha256 of the
        // leaf's raw DER bytes, hex-encoded.
        let fingerprint = hex::encode(Sha256::digest(certs[0].as_ref()));

        let machine_id = repo::cert_is_active(&self.pool, &fingerprint)
            .await
            .map_err(|e| internal_error("checking cert status", &e))?
            .ok_or_else(|| Status::unauthenticated("certificate revoked or expired"))?;

        // Defense-in-depth: the CN-derived agent_id and the fingerprint-derived
        // machine_id must agree -- they're two independent looks at "who is
        // this," and a mismatch means something is wrong (e.g. a CN collision).
        if machine_id != agent_id {
            return Err(Status::unauthenticated("certificate identity mismatch"));
        }

        let (tx, rx) = mpsc::channel::<Result<ServerFrame, Status>>(16);
        let pool = self.pool.clone();
        let hub = self.hub.clone();
        let (epoch, shutdown) = hub.register(machine_id, tx.clone());
        let mut inbound = request.into_inner();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    item = inbound.next() => {
                        match item {
                            Some(Ok(frame)) => {
                                if let Err(e) =
                                    handle_agent_frame(&pool, &hub, machine_id, frame, &tx).await
                                {
                                    tracing::warn!(error = %e, %machine_id, "session: error handling agent frame");
                                }
                            }
                            Some(Err(e)) => {
                                tracing::warn!(error = %e, %machine_id, "session: inbound stream error");
                                break;
                            }
                            None => break,
                        }
                    }
                    // Fired by `Hub::register` when a later Session for this
                    // same machine replaces this one: this connection can no
                    // longer be routed to (the hub's `conns` entry is already
                    // the new session's), so continuing to read `inbound`
                    // here would just keep pumping heartbeats for a session
                    // the hub can't dispatch verbs/logs/terminal to anymore.
                    // Exit through the SAME teardown path a normal disconnect
                    // takes, below.
                    _ = shutdown.notified() => {
                        tracing::info!(%machine_id, "session: superseded by a new connection for this machine");
                        break;
                    }
                }
            }
            hub.unregister(machine_id, epoch);
            // The agent-side tails are already aborted on teardown; without
            // this, any server-side tail sinks for this machine would hang
            // their SSE streams open forever with no eof (a frozen "live"
            // view in the browser).
            hub.close_tails_for(machine_id);
            // Same reasoning for open terminals: dropping the ptys registry
            // entries drops their byte-sink `Sender`s, which is what makes
            // the WS handler's `rx.recv()` observe closure and end the
            // browser session instead of hanging silently forever.
            hub.close_ptys_for(machine_id);
            tracing::info!(%machine_id, "session: agent disconnected");
        });

        Ok(Response::new(
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)) as Self::SessionStream,
        ))
    }
}

/// Handle one inbound `AgentFrame` on an authenticated session: the testable
/// seam for the bidi loop above (no TLS/transport needed to exercise it).
async fn handle_agent_frame(
    pool: &PgPool,
    hub: &Hub,
    machine_id: Uuid,
    frame: AgentFrame,
    tx: &mpsc::Sender<Result<ServerFrame, Status>>,
) -> anyhow::Result<()> {
    match frame.payload {
        Some(agent_frame::Payload::Hello(hello)) => {
            if let Some(info) = &hello.info {
                // Keyed by the cert-authenticated `machine_id` UUID, NEVER by
                // `info.machine_id` (self-reported) -- see
                // `repo::update_machine_inventory`'s doc comment for why.
                repo::update_machine_inventory(pool, machine_id, &agent_info_row(info)).await?;
            }
            // Stamps `last_seen_at = now()` as well as the status, so a
            // freshly-online machine isn't immediately eligible for the 45s
            // offline sweeper.
            repo::mark_online(pool, machine_id).await?;
            repo::audit(
                pool,
                repo::Actor::Agent(machine_id),
                "agent.online",
                Some(machine_id),
                "ok",
            )
            .await?;

            tx.send(Ok(ServerFrame {
                stream_id: 0,
                payload: Some(server_frame::Payload::HelloAck(HelloAck {
                    server_version: env!("CARGO_PKG_VERSION").into(),
                    heartbeat_secs: argus_common::DEFAULT_HEARTBEAT_SECS,
                })),
            }))
            .await
            .ok();
        }
        // The periodic push frames below are all proof of life, so each one
        // both stamps `last_seen_at` and re-asserts `online` — see
        // `repo::mark_online` for why re-asserting matters: without it a
        // machine the sweeper flipped during a stall stays `offline` forever
        // even as its heartbeats resume.
        Some(agent_frame::Payload::Heartbeat(_)) => {
            repo::mark_online(pool, machine_id).await?;
        }
        Some(agent_frame::Payload::Metrics(m)) => {
            let row = metrics_row_from_proto(&m);
            repo::insert_metrics(pool, machine_id, &row).await?;
            repo::mark_online(pool, machine_id).await?;
        }
        Some(agent_frame::Payload::DockerState(ds)) => {
            hub.set_docker(machine_id, ds.containers);
            repo::mark_online(pool, machine_id).await?;
        }
        Some(agent_frame::Payload::SystemdState(ss)) => {
            hub.set_systemd(machine_id, ss.units);
            repo::mark_online(pool, machine_id).await?;
        }
        Some(agent_frame::Payload::LogChunk(chunk)) => {
            // Deliberately no `mark_online` here: a log tail is not evidence
            // that the agent's heartbeat path is healthy, and refreshing on log
            // traffic would let a busy log mask a wedged agent.
            hub.deliver_chunk(&chunk.request_id.clone(), machine_id, chunk);
        }
        Some(agent_frame::Payload::PtyOutput(out)) => {
            hub.deliver_pty_output(&out.session_id, machine_id, out.data, out.eof);
        }
        Some(agent_frame::Payload::CommandResult(cr)) => {
            let command_id = cr.command_id.clone();
            match Uuid::parse_str(&command_id) {
                Ok(uuid) => {
                    // Log-don't-propagate: a failed audit update must not stop us
                    // from waking the HTTP waiter with the real result below (the
                    // verb already ran on the agent).
                    if let Err(e) = repo::update_command_result(
                        pool,
                        uuid,
                        machine_id,
                        if cr.ok { "ok" } else { "error" },
                    )
                    .await
                    {
                        tracing::error!(
                            error = %e,
                            %machine_id,
                            command_id = %command_id,
                            "failed to update command result audit row"
                        );
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        %machine_id,
                        command_id = %command_id,
                        "command result carried an unparseable command_id; skipping audit update"
                    );
                }
            }
            hub.complete(&command_id, machine_id, cr);
        }
        _ => {
            // No payload, or `Ack` -- a low-level stream-control
            // acknowledgement the server doesn't currently act on. Every
            // other `agent_frame::Payload` variant is matched above; note
            // PtyOpen/PtyInput/PtyResize/PtyClose are `server_frame::Payload`
            // variants (server -> agent) and cannot appear in this match at
            // all.
        }
    }

    Ok(())
}

/// Map the proto `MetricsSample` -> the repo's `MetricsSampleRow`. The proto
/// carries mem/swap/disk/net counters as `uint64` (never negative on the
/// wire), but `metrics` stores them as `bigint` (`i64`) -- Postgres has no
/// unsigned integer type -- so each is cast `as i64` here. `cpu_pct`/`load*`
/// are already `f32` on both sides.
fn metrics_row_from_proto(m: &argus_proto::v1::MetricsSample) -> repo::MetricsSampleRow {
    repo::MetricsSampleRow {
        cpu_pct: m.cpu_pct,
        mem_used: m.mem_used as i64,
        mem_total: m.mem_total as i64,
        swap_used: m.swap_used as i64,
        swap_total: m.swap_total as i64,
        load1: m.load1,
        load5: m.load5,
        load15: m.load15,
        disk_used: m.disk_used as i64,
        disk_total: m.disk_total as i64,
        net_rx_bytes: m.net_rx_bytes as i64,
        net_tx_bytes: m.net_tx_bytes as i64,
        uptime_secs: m.uptime_secs as i64,
    }
}

/// Map the proto `AgentInfo` -> the repo's `AgentInfoRow`, shared by `enroll`
/// (initial inventory) and `handle_agent_frame`'s `Hello` handling (inventory
/// refresh on every reconnect) so the mapping isn't duplicated.
fn agent_info_row(info: &argus_proto::v1::AgentInfo) -> AgentInfoRow {
    AgentInfoRow {
        machine_id: info.machine_id.clone(),
        hostname: info.hostname.clone(),
        os: non_empty(&info.os),
        kernel: non_empty(&info.kernel),
        arch: non_empty(&info.arch),
        primary_ip: non_empty(&info.primary_ip),
        agent_version: non_empty(&info.agent_version),
        // Only an agent that SAYS it is reporting produces a non-NULL value.
        // proto3 decodes an absent repeated field and an empty one identically,
        // so this flag is the only thing separating "old agent, gate nothing"
        // from "bare host, gate everything".
        capabilities: info
            .capabilities_reported
            .then(|| info.capabilities.clone()),
    }
}

/// Empty proto strings mean "unset" for the optional `AgentInfo` fields.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn internal_error(context: &str, err: &anyhow::Error) -> Status {
    tracing::error!(error = %err, context, "enroll: internal error");
    Status::internal(format!("{context} failed"))
}

/// Serve the agent-facing gRPC surface with mTLS: server-authenticated always,
/// client-authenticated OPTIONALLY (PRD §5.4). `server_identity` is
/// `(cert_pem, key_pem)` for the control plane's own leaf, issued by the
/// internal CA at startup. A presented client cert is validated against the
/// internal CA (`client_ca_root`), but `client_auth_optional(true)` means one
/// is not required at the transport level -- `Enroll` (no client cert) keeps
/// working, while `session` (above) enforces the requirement itself and
/// rejects calls with no presented cert.
pub async fn serve(cfg: &Config, svc: AgentSvc, server_identity: (String, String)) -> Result<()> {
    let (cert_pem, key_pem) = server_identity;
    // `svc.ca` is moved into `AgentServiceServer::new(svc)` below, so the CA
    // cert PEM needed for `client_ca_root` is captured first.
    let ca_cert_pem = svc.ca.ca_cert_pem().to_string();
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert_pem, key_pem))
        .client_ca_root(Certificate::from_pem(ca_cert_pem))
        .client_auth_optional(true);

    tracing::info!(addr = %cfg.agent_addr, "agent gRPC surface listening");

    Server::builder()
        .tls_config(tls)?
        .add_service(AgentServiceServer::new(svc))
        .serve(cfg.agent_addr.parse()?)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::FieldCipher;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use rcgen::{CertificateParams, KeyPair};
    use sha2::{Digest, Sha256};
    use x509_parser::prelude::{FromDer, X509Certificate};

    fn make_csr() -> String {
        let kp = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["agent".into()]).unwrap();
        params.serialize_request(&kp).unwrap().pem().unwrap()
    }

    /// `agent_info_row` is the SINGLE conversion point from the proto message
    /// to the row, and the only place `capabilities_reported` becomes the
    /// `Option`. Pin all three tri-state outcomes directly, independent of any
    /// DB round trip.
    #[test]
    fn agent_info_row_capabilities_tri_state() {
        // Not reported -> None (NULL, gate nothing) -- even though the proto
        // wire can't distinguish an empty repeated field from an absent one,
        // this must not be confused with a genuinely-empty report below.
        let not_reported = argus_proto::v1::AgentInfo {
            machine_id: "m".into(),
            hostname: "h".into(),
            capabilities: vec!["systemd".into()],
            capabilities_reported: false,
            ..Default::default()
        };
        assert_eq!(
            agent_info_row(&not_reported).capabilities,
            None,
            "capabilities_reported == false must map to None regardless of the repeated field's contents"
        );

        // Reported, but the host genuinely has none -> Some(vec![]), not None.
        let reported_empty = argus_proto::v1::AgentInfo {
            machine_id: "m".into(),
            hostname: "h".into(),
            capabilities: vec![],
            capabilities_reported: true,
            ..Default::default()
        };
        assert_eq!(
            agent_info_row(&reported_empty).capabilities,
            Some(vec![]),
            "capabilities_reported == true with an empty vec must map to Some(vec![]), not None"
        );

        // Reported with entries -> Some(entries).
        let reported_some = argus_proto::v1::AgentInfo {
            machine_id: "m".into(),
            hostname: "h".into(),
            capabilities: vec!["systemd".into(), "journal".into()],
            capabilities_reported: true,
            ..Default::default()
        };
        assert_eq!(
            agent_info_row(&reported_some).capabilities,
            Some(vec!["systemd".to_string(), "journal".to_string()])
        );
    }

    async fn seed_token(pool: &PgPool, plain: &str, name: &str) {
        let hash = Sha256::digest(plain.as_bytes()).to_vec();
        sqlx::query!(
            "INSERT INTO enrollment_tokens (name, token_hash, max_uses, uses, expires_at, revoked)
             VALUES ($1, $2, NULL, 0, NULL, false)",
            name,
            hash,
        )
        .execute(pool)
        .await
        .expect("seed enrollment token");
    }

    #[sqlx::test]
    async fn enroll_signs_csr_and_records_machine_cert_and_audit(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([3u8; 32]))?;
        let ca = Arc::new(CertAuthority::load_or_init(&pool, &cipher).await?);
        let svc = AgentSvc::new(ca, pool.clone(), Arc::new(crate::hub::Hub::new()));

        seed_token(&pool, "devtoken", "dev-token").await;

        let resp = svc
            .enroll(Request::new(EnrollRequest {
                join_token: "devtoken".to_string(),
                csr_pem: make_csr(),
                info: Some(argus_proto::v1::AgentInfo {
                    machine_id: "m-1".to_string(),
                    hostname: "h1".to_string(),
                    ..Default::default()
                }),
            }))
            .await
            .expect("enroll should succeed")
            .into_inner();

        // The signed cert's CN matches the returned agent_id.
        let pem = ::pem::parse(resp.client_cert_pem.as_bytes())?;
        let (_, cert) = X509Certificate::from_der(pem.contents())?;
        let cn = cert.subject().iter_common_name().next().unwrap().as_str()?;
        assert_eq!(cn, resp.agent_id);
        assert!(!resp.ca_cert_pem.is_empty());

        let machine = sqlx::query!("SELECT id, hostname FROM machines WHERE machine_id = 'm-1'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(machine.id.to_string(), resp.agent_id);
        assert_eq!(machine.hostname, "h1");

        let cert_row = sqlx::query!(
            "SELECT machine_id FROM agent_certs WHERE machine_id = $1",
            machine.id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(cert_row.machine_id, machine.id);

        let audit_row = sqlx::query!(
            r#"SELECT actor, result, detail->>'enrollment_token' AS "enrollment_token!"
               FROM audit_log WHERE action = 'agent.enroll' AND result = 'ok'"#
        )
        .fetch_one(&pool)
        .await?;
        // The actor is the newly-minted agent itself (Actor::Agent), not the
        // enrollment token's name -- the token has no representation in the
        // closed Actor enum, and by this point a machine genuinely exists.
        assert_eq!(audit_row.actor, machine.id.to_string());
        // But the token IS the server-verified credential that authorized
        // this enrollment, unlike the self-reported `agent_id` -- it must
        // still be recoverable, from `detail`.
        assert_eq!(audit_row.enrollment_token, "dev-token");

        Ok(())
    }

    #[sqlx::test]
    async fn enroll_with_bad_token_is_denied_and_audited(pool: PgPool) -> anyhow::Result<()> {
        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([5u8; 32]))?;
        let ca = Arc::new(CertAuthority::load_or_init(&pool, &cipher).await?);
        let svc = AgentSvc::new(ca, pool.clone(), Arc::new(crate::hub::Hub::new()));

        let result = svc
            .enroll(Request::new(EnrollRequest {
                join_token: "not-a-real-token".to_string(),
                csr_pem: make_csr(),
                info: Some(argus_proto::v1::AgentInfo {
                    machine_id: "m-2".to_string(),
                    hostname: "h2".to_string(),
                    ..Default::default()
                }),
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);

        let audit_row = sqlx::query!(
            "SELECT actor, result, detail FROM audit_log WHERE action = 'agent.enroll' AND result = 'denied'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, "system");
        // Unlike the missing-`info.machine_id` denial below, no token was
        // ever resolved on this path (the hash never matched a row), so
        // there is no name to have put in `detail` -- it stays the `'{}'`
        // default.
        assert_eq!(audit_row.detail, serde_json::json!({}));

        Ok(())
    }

    /// The OTHER `agent.enroll`/`denied` path: unlike a bad token (above),
    /// the token here IS valid and consumed -- the request itself is what's
    /// rejected (no usable `info.machine_id`). Regression coverage for the
    /// audit-detail fix: this row previously carried zero identifying
    /// information (actor `system`, every other column `NULL`), making
    /// "which join token was presented" permanently unanswerable.
    #[sqlx::test]
    async fn enroll_with_missing_machine_id_is_denied_and_audited(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([11u8; 32]))?;
        let ca = Arc::new(CertAuthority::load_or_init(&pool, &cipher).await?);
        let svc = AgentSvc::new(ca, pool.clone(), Arc::new(crate::hub::Hub::new()));

        seed_token(&pool, "missing-info-token", "missing-info").await;

        let result = svc
            .enroll(Request::new(EnrollRequest {
                join_token: "missing-info-token".to_string(),
                csr_pem: make_csr(),
                info: None,
            }))
            .await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);

        let audit_row = sqlx::query!(
            r#"SELECT actor, result, detail->>'enrollment_token' AS "enrollment_token!"
               FROM audit_log WHERE action = 'agent.enroll' AND result = 'denied'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, "system");
        // The still-valid, still-consumed token is the one piece of
        // server-verified identifying information this row has; it must
        // survive in `detail` even though the request itself was rejected.
        assert_eq!(audit_row.enrollment_token, "missing-info");

        Ok(())
    }

    /// A malformed CSR must fail *after* the token has been consumed inside
    /// the transaction; the rollback has to restore the token's `uses` count
    /// (a single-use token must not be permanently burned by a signing
    /// failure) and must leave no `agent_certs` row behind, while still
    /// producing an `agent.enroll` / `error` audit row on the pool.
    #[sqlx::test]
    async fn enroll_with_malformed_csr_rolls_back_and_audits_error(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([7u8; 32]))?;
        let ca = Arc::new(CertAuthority::load_or_init(&pool, &cipher).await?);
        let svc = AgentSvc::new(ca, pool.clone(), Arc::new(crate::hub::Hub::new()));

        let hash = Sha256::digest("single-use-token".as_bytes()).to_vec();
        sqlx::query!(
            "INSERT INTO enrollment_tokens (name, token_hash, max_uses, uses, expires_at, revoked)
             VALUES ($1, $2, 1, 0, NULL, false)",
            "single-use",
            hash,
        )
        .execute(&pool)
        .await
        .expect("seed single-use token");

        let result = svc
            .enroll(Request::new(EnrollRequest {
                join_token: "single-use-token".to_string(),
                csr_pem: "not-a-csr".to_string(),
                info: Some(argus_proto::v1::AgentInfo {
                    machine_id: "m-3".to_string(),
                    hostname: "h3".to_string(),
                    ..Default::default()
                }),
            }))
            .await;

        assert!(result.is_err());

        let token_row = sqlx::query!(
            "SELECT uses FROM enrollment_tokens WHERE token_hash = $1",
            hash,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            token_row.uses, 0,
            "rollback must restore the token's uses count"
        );

        let audit_row = sqlx::query!(
            r#"SELECT actor, result, detail->>'enrollment_token' AS "enrollment_token!"
               FROM audit_log WHERE action = 'agent.enroll' AND result = 'error'"#
        )
        .fetch_one(&pool)
        .await?;
        // No machine survives the rollback, so this is Actor::System, not the
        // burned token's name -- same reasoning as the denied path. The
        // burned token's name is still recoverable from `detail`, though.
        assert_eq!(audit_row.actor, "system");
        assert_eq!(audit_row.enrollment_token, "single-use");

        let cert_count = sqlx::query!("SELECT count(*) AS count FROM agent_certs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            cert_count.count,
            Some(0),
            "no agent_certs row must survive the rollback"
        );

        Ok(())
    }

    /// The testable seam for `session`'s bidi loop, exercised without any TLS
    /// handshake: `Hello` must refresh inventory, flip the machine online,
    /// stamp `last_seen_at`, audit `agent.online`/`ok`, and reply with a
    /// `HelloAck`; a subsequent `Heartbeat` must advance `last_seen_at` again
    /// without touching `status`.
    #[sqlx::test]
    async fn handle_agent_frame_hello_then_heartbeat(pool: PgPool) -> anyhow::Result<()> {
        // Seed a machine directly (pre-`Hello`, `status = 'pending'` per the
        // schema default) rather than going through `enroll` -- `Session`'s
        // authn is out of scope for this seam test (Task 11 covers it
        // end-to-end over real mTLS).
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-session-1".to_string(),
                hostname: "pre-hello".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::Hello(argus_proto::v1::Hello {
                    info: Some(argus_proto::v1::AgentInfo {
                        machine_id: "m-session-1".to_string(),
                        hostname: "hello-host".to_string(),
                        ..Default::default()
                    }),
                })),
            },
            &tx,
        )
        .await?;

        let row = sqlx::query!(
            "SELECT status, hostname, last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.status, "online");
        assert_eq!(row.hostname, "hello-host", "Hello must refresh inventory");
        let first_seen = row.last_seen_at.expect("Hello must stamp last_seen_at");

        let ack = rx
            .try_recv()
            .expect("Hello must send a HelloAck on the outbound channel")
            .expect("HelloAck send must be Ok");
        match ack.payload {
            Some(server_frame::Payload::HelloAck(hello_ack)) => {
                assert_eq!(
                    hello_ack.heartbeat_secs,
                    argus_common::DEFAULT_HEARTBEAT_SECS
                );
                assert!(!hello_ack.server_version.is_empty());
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        let audit_row = sqlx::query!(
            "SELECT actor, result FROM audit_log WHERE action = 'agent.online' AND result = 'ok'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, machine_id.to_string());

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::Heartbeat(
                    argus_proto::v1::Heartbeat {
                        unix_ms: 0,
                        uptime_secs: 0,
                    },
                )),
            },
            &tx,
        )
        .await?;

        let row = sqlx::query!(
            "SELECT status, last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.status, "online", "Heartbeat must not change status");
        let second_seen = row.last_seen_at.expect("Heartbeat must stamp last_seen_at");
        assert!(
            second_seen > first_seen,
            "Heartbeat must advance last_seen_at"
        );

        Ok(())
    }

    /// Regression test for the fleet-integrity hole: an authenticated agent A
    /// that sends a `Hello` whose self-reported `info.machine_id` is machine
    /// B's `machine_id` string must NOT be able to overwrite B's inventory (or
    /// create a phantom third row) -- inventory updates are keyed by the
    /// cert-AUTHENTICATED `machine_id` UUID (the `handle_agent_frame` param),
    /// never by the self-reported string inside the `Hello` payload.
    #[sqlx::test]
    async fn handle_agent_frame_hello_does_not_overwrite_another_machine_by_self_reported_id(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Seed two independently-enrolled machines, A and B.
        let machine_a = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-victim-a".to_string(),
                hostname: "host-a".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let machine_b = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-victim-b".to_string(),
                hostname: "host-b".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let machine_count_before = sqlx::query!("SELECT count(*) AS count FROM machines")
            .fetch_one(&pool)
            .await?
            .count
            .unwrap_or(0);

        let hub = crate::hub::Hub::new();
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        // Authenticated as A (per the cert-derived `machine_id` param, exactly
        // as `session()` would pass it), but the Hello's self-reported
        // `info.machine_id` is B's machine_id string -- a spoofed/misreported
        // identity inside an already-authenticated session.
        handle_agent_frame(
            &pool,
            &hub,
            machine_a,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::Hello(argus_proto::v1::Hello {
                    info: Some(argus_proto::v1::AgentInfo {
                        machine_id: "m-victim-b".to_string(),
                        hostname: "pwned-by-a".to_string(),
                        ..Default::default()
                    }),
                })),
            },
            &tx,
        )
        .await?;

        // (a) B's hostname is unchanged.
        let row_b = sqlx::query!("SELECT hostname FROM machines WHERE id = $1", machine_b)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row_b.hostname, "host-b",
            "B's inventory must not be touched by A's Hello"
        );

        // (b) no new machines row was created -- still exactly the 2 seeded.
        let machine_count_after = sqlx::query!("SELECT count(*) AS count FROM machines")
            .fetch_one(&pool)
            .await?
            .count
            .unwrap_or(0);
        assert_eq!(
            machine_count_after, machine_count_before,
            "no phantom machine row must be created from the self-reported machine_id"
        );

        // (c) A's own hostname WAS updated -- inventory refresh still works,
        // just keyed by the authenticated identity rather than the payload.
        let row_a = sqlx::query!("SELECT hostname FROM machines WHERE id = $1", machine_a)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            row_a.hostname, "pwned-by-a",
            "A's own inventory must still be refreshed, keyed by its authenticated id"
        );

        Ok(())
    }

    /// A `Metrics` frame must insert one `metrics` row for the authenticated
    /// machine (server-stamped `ts`, so the row's presence/values are what we
    /// check) and advance `last_seen_at`, exactly like `Heartbeat` does.
    #[sqlx::test]
    async fn handle_agent_frame_metrics_inserts_row_and_touches_last_seen(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        // Seed an enrolled machine directly, same pattern as the Hello/
        // Heartbeat seam test above -- Session's authn is out of scope here.
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-metrics-1".to_string(),
                hostname: "metrics-host".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::Metrics(
                    argus_proto::v1::MetricsSample {
                        cpu_pct: 12.5,
                        mem_used: 100,
                        mem_total: 200,
                        ..Default::default()
                    },
                )),
            },
            &tx,
        )
        .await?;

        let count = sqlx::query!(
            "SELECT count(*) AS count FROM metrics WHERE machine_id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?
        .count
        .unwrap_or(0);
        assert_eq!(count, 1, "Metrics frame must insert exactly one row");

        let row = sqlx::query!(
            "SELECT cpu_pct, mem_used, mem_total FROM metrics WHERE machine_id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.cpu_pct, Some(12.5));
        assert_eq!(row.mem_used, Some(100));
        assert_eq!(row.mem_total, Some(200));

        let machine_row = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            machine_row.last_seen_at.is_some(),
            "Metrics frame must stamp last_seen_at"
        );

        Ok(())
    }

    /// A `DockerState` frame must cache the reported containers in the hub, keyed
    /// by the authenticated machine_id.
    #[sqlx::test]
    async fn handle_agent_frame_docker_state_caches_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-docker-1".to_string(),
                hostname: "docker-host".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::DockerState(
                    argus_proto::v1::DockerState {
                        containers: vec![argus_proto::v1::Container {
                            id: "abc123".into(),
                            name: "nginx".into(),
                            image: "nginx:latest".into(),
                            state: "running".into(),
                            status: "Up 2 minutes (healthy)".into(),
                            health: "healthy".into(),
                        }],
                    },
                )),
            },
            &tx,
        )
        .await?;

        let cached = hub.get_docker(machine_id);
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].name, "nginx");
        assert_eq!(cached[0].health, "healthy");

        Ok(())
    }

    /// A `SystemdState` frame must cache the reported units in the hub, keyed by
    /// the authenticated machine_id, and refresh `last_seen_at`.
    #[sqlx::test]
    async fn handle_agent_frame_systemd_state_caches_snapshot(pool: PgPool) -> anyhow::Result<()> {
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-systemd-1".to_string(),
                hostname: "systemd-host".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 0,
                payload: Some(agent_frame::Payload::SystemdState(
                    argus_proto::v1::SystemdState {
                        units: vec![
                            argus_proto::v1::Unit {
                                name: "nginx.service".into(),
                                load_state: "loaded".into(),
                                active_state: "active".into(),
                                sub_state: "running".into(),
                                description: "A high performance web server".into(),
                            },
                            argus_proto::v1::Unit {
                                name: "backup.service".into(),
                                load_state: "loaded".into(),
                                active_state: "failed".into(),
                                sub_state: "failed".into(),
                                description: "Nightly backup".into(),
                            },
                        ],
                    },
                )),
            },
            &tx,
        )
        .await?;

        let cached = hub.get_systemd(machine_id);
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].name, "nginx.service");
        assert_eq!(hub.failed_unit_count(machine_id), 1);

        let row = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            row.last_seen_at.is_some(),
            "a SystemdState frame must refresh last_seen_at"
        );

        Ok(())
    }

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
                capabilities: None,
            },
        )
        .await?;

        let hub = crate::hub::Hub::new();
        let (rid, mut rx) = hub.open_tail(machine_id);
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        let before = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_id
        )
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

        let after = sqlx::query!(
            "SELECT last_seen_at FROM machines WHERE id = $1",
            machine_id
        )
        .fetch_one(&pool)
        .await?
        .last_seen_at;
        assert_eq!(before, after, "a log chunk must not refresh last_seen_at");

        Ok(())
    }

    /// A `CommandResult` frame must (a) update the dispatched audit row to its final
    /// result and (b) wake any pending waiter registered for that command_id.
    #[sqlx::test]
    async fn handle_agent_frame_command_result_updates_audit_and_wakes_waiter(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_id = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-cmdres-1".to_string(),
                hostname: "cmd-host".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;

        let command_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
             VALUES ('anonymous', 'container.stop', $1, 'web', $2, 'dispatched')",
            machine_id,
            command_id,
        )
        .execute(&pool)
        .await?;

        let hub = crate::hub::Hub::new();
        let waiter = hub.register_pending(command_id.to_string(), machine_id);
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_id,
            AgentFrame {
                stream_id: 7,
                payload: Some(agent_frame::Payload::CommandResult(
                    argus_proto::v1::CommandResult {
                        command_id: command_id.to_string(),
                        ok: true,
                        exit_code: 0,
                        message: "ok".into(),
                    },
                )),
            },
            &tx,
        )
        .await?;

        // (a) waiter woken with the result
        let delivered = waiter.await.expect("waiter must be woken");
        assert!(delivered.ok);
        // (b) audit row updated
        let row = sqlx::query!(
            "SELECT result FROM audit_log WHERE command_id = $1",
            command_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.result.as_deref(), Some("ok"));

        Ok(())
    }

    /// A `CommandResult` reported while authenticated as machine B, carrying a
    /// command_id owned by machine A, must NOT resolve A's audit row or A's waiter.
    #[sqlx::test]
    async fn handle_agent_frame_command_result_is_scoped_to_authenticated_machine(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let machine_a = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-cmd-a".to_string(),
                hostname: "a".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;
        let machine_b = repo::upsert_machine(
            &pool,
            &AgentInfoRow {
                machine_id: "m-cmd-b".to_string(),
                hostname: "b".to_string(),
                os: None,
                kernel: None,
                arch: None,
                primary_ip: None,
                agent_version: None,
                capabilities: None,
            },
        )
        .await?;
        let command_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO audit_log (actor, action, machine_id, target_ref, command_id, result)
             VALUES ('anonymous', 'container.stop', $1, 'web', $2, 'dispatched')",
            machine_a,
            command_id,
        )
        .execute(&pool)
        .await?;

        let hub = crate::hub::Hub::new();
        let mut waiter = hub.register_pending(command_id.to_string(), machine_a);
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
            &hub,
            machine_b,
            AgentFrame {
                stream_id: 7,
                payload: Some(agent_frame::Payload::CommandResult(
                    argus_proto::v1::CommandResult {
                        command_id: command_id.to_string(),
                        ok: true,
                        exit_code: 0,
                        message: "spoof".into(),
                    },
                )),
            },
            &tx,
        )
        .await?;

        let row = sqlx::query!(
            "SELECT result FROM audit_log WHERE command_id = $1",
            command_id
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            row.result.as_deref(),
            Some("dispatched"),
            "B must not resolve A's command"
        );
        assert!(waiter.try_recv().is_err(), "B must not wake A's waiter");

        Ok(())
    }
}
