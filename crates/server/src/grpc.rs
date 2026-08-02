//! Agent-facing gRPC surface (PRD §4, §5). Served on the MetalLB LoadBalancer
//! address with mTLS terminated here -- never via Traefik (PRD §2.4).
//!
//! The rustls listener accepts OPTIONAL client auth (`Enroll` needs none);
//! `Session` REQUIRES a presented, active agent cert --
//! `identity::agent_id_from_peer` turns its CN into an `agent_id`,
//! cross-checked against `repo::cert_is_active`'s fingerprint lookup.

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
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

/// `Enroll` signs CSRs against the internal CA; `Session` runs the
/// multiplexed bidi loop over an authenticated agent_id (see module doc).
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

        // Token check + machine/cert writes share one transaction: any
        // failure past the token check rolls back (un-burning a single-use
        // token, undoing partial writes). Audit rows for failures go to the
        // POOL (outside the tx), so they survive the rollback.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| internal_error("beginning enroll transaction", &e.into()))?;

        // `Actor::System`: no machine is committed yet, so there's no
        // `Actor::Agent` to attribute this to (the success path below uses
        // one once committed). Since `System` carries no identifying detail,
        // the token name goes in `detail` instead -- `enrollment_tokens`
        // keeps no per-use history, only a bare `uses` counter.
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

        // The three fallible steps share one rollback+audit-error path: on
        // failure, roll back (`machine_id` is None afterward -- `audit_log.machine_id`
        // is an FK) and audit "error" on the pool before returning.
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

        // `agent_id` is committed inside the still-open `tx`, so unlike the
        // paths above there IS a machine to attribute this to. `Actor::Agent`
        // alone records only the self-reported CLAIM; the token name in
        // `detail` records the CREDENTIAL that actually authorized it.
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

        // `agent_id_from_peer` already proved `certs` is non-empty. Must
        // match how `ca::sign_csr`/`repo::insert_agent_cert` computed the
        // fingerprint at enrollment: sha256 of the leaf's raw DER, hex-encoded.
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
        // The one positive "this host actually connected" line, symmetric
        // with the disconnect log in the teardown below. Every earlier
        // return in this fn is a rejection; without this, a HEALTHY
        // enrollment is indistinguishable from nothing happening in the
        // control plane's own logs -- confirmable only from the agent side.
        tracing::info!(%machine_id, epoch, "session: agent connected and authenticated");
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
                    // Fired when a later Session for this machine supersedes
                    // this one (see `ConnHandle`'s doc in hub.rs) -- exit
                    // through the normal teardown path below.
                    _ = shutdown.notified() => {
                        tracing::info!(%machine_id, "session: superseded by a new connection for this machine");
                        break;
                    }
                }
            }
            hub.unregister(machine_id, epoch);
            // Forget the debounce stamp so the next session's first liveness
            // frame writes immediately. Unconditional (unlike `unregister`'s
            // epoch check): worst case a newer session's window is cleared
            // and it writes one extra time -- harmless.
            hub.clear_online_stamp(machine_id);
            // Closes server-side sinks so they don't hang open forever; see
            // `Hub::close_tails_for`'s doc comment.
            hub.close_tails_for(machine_id);
            // Dropping the ptys registry entries drops their byte-sink `Sender`s,
            // which is what makes the WS handler's `rx.recv()` observe closure and
            // end the browser session instead of hanging silently forever.
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
            // offline sweeper. Unconditional (a fresh session must flip
            // pending/offline immediately), but it opens the debounce window
            // so the snapshot frames sent right behind Hello don't each
            // write again (see `Hub::should_mark_online`).
            hub.stamp_online(machine_id);
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
        // Every push frame below is proof of life: stamps `last_seen_at` and
        // re-asserts `online` (see `repo::mark_online` for why re-asserting
        // matters -- without it a stall-flipped machine never recovers).
        // Debounced through `Hub::should_mark_online`: the agent sends all
        // four back-to-back each tick, and one write per window carries the
        // same information as four.
        Some(agent_frame::Payload::Heartbeat(_)) => {
            if hub.should_mark_online(machine_id) {
                repo::mark_online(pool, machine_id).await?;
            }
        }
        Some(agent_frame::Payload::Metrics(m)) => {
            let row = metrics_row_from_proto(&m);
            repo::insert_metrics(pool, machine_id, &row).await?;
            if hub.should_mark_online(machine_id) {
                repo::mark_online(pool, machine_id).await?;
            }
        }
        Some(agent_frame::Payload::DockerState(ds)) => {
            hub.set_docker(machine_id, ds.containers);
            if hub.should_mark_online(machine_id) {
                repo::mark_online(pool, machine_id).await?;
            }
        }
        Some(agent_frame::Payload::SystemdState(ss)) => {
            hub.set_systemd(machine_id, ss.units);
            if hub.should_mark_online(machine_id) {
                repo::mark_online(pool, machine_id).await?;
            }
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
            // No payload, or `Ack` (a low-level ack the server doesn't act
            // on). PtyOpen/PtyInput/PtyResize/PtyClose are
            // `server_frame::Payload` (server -> agent) and cannot appear here.
        }
    }

    Ok(())
}

/// Proto carries mem/swap/disk/net counters as `uint64` (never negative on
/// the wire), but `metrics` stores `bigint` (`i64`) -- Postgres has no
/// unsigned type -- so each is cast `as i64` here. `cpu_pct`/`load*` are
/// already `f32` on both sides.
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
        // Non-NULL only if the agent SAYS it's reporting: proto3 can't tell
        // an absent repeated field from an empty one, so this flag alone
        // separates "old agent, gate nothing" from "bare host, gate everything".
        capabilities: info
            .capabilities_reported
            .then(|| info.capabilities.clone()),
        cpu_model: non_empty(&info.cpu_model),
        cpu_cores: (info.cpu_cores > 0).then_some(info.cpu_cores as i32),
        // An absurd value from a bad clock fails `from_unix_timestamp` --
        // degrade to None (not-reported) rather than erroring the whole
        // session over one clock-skewed field.
        boot_time: (info.boot_time > 0)
            .then(|| OffsetDateTime::from_unix_timestamp(info.boot_time).ok())
            .flatten(),
        virt: non_empty(&info.virt),
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

/// mTLS: server-authenticated always, client-authenticated OPTIONALLY (PRD
/// §5.4) -- `session` (above) enforces its own stricter requirement.
/// `server_identity` is the control plane's own leaf, issued by the internal
/// CA at startup.
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

    /// `agent_info_row` is the SINGLE conversion point where
    /// `capabilities_reported` becomes the `Option`. Pins all three
    /// tri-state outcomes directly, independent of any DB round trip.
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

    /// The OTHER `agent.enroll`/`denied` path: the token here IS valid and
    /// consumed -- the request itself is rejected (no usable
    /// `info.machine_id`). The token name must still survive in `detail`.
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

    /// A malformed CSR fails *after* the token is consumed inside the
    /// transaction; the rollback must restore the token's `uses` count (a
    /// single-use token must not be burned by a signing failure), leave no
    /// `agent_certs` row, and still produce an `agent.enroll`/`error` row.
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

    /// The testable seam for `session`'s bidi loop, without any TLS
    /// handshake: `Hello` must refresh inventory, flip online, stamp
    /// `last_seen_at`, audit `agent.online`/`ok`, and reply `HelloAck`; a
    /// following `Heartbeat` must advance `last_seen_at` without touching `status`.
    #[sqlx::test]
    async fn handle_agent_frame_hello_then_heartbeat(pool: PgPool) -> anyhow::Result<()> {
        // Seed a machine directly (pre-`Hello`, `status = 'pending'` per the
        // schema default) rather than going through `enroll` -- `Session`'s
        // authn (over real mTLS) is out of scope for this seam test.
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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

        let heartbeat = || AgentFrame {
            stream_id: 0,
            payload: Some(agent_frame::Payload::Heartbeat(
                argus_proto::v1::Heartbeat {
                    unix_ms: 0,
                    uptime_secs: 0,
                },
            )),
        };

        // Inside the debounce window the heartbeat deliberately does NOT
        // write: Hello just opened the window, and one write per window
        // carries the same information (see `MARK_ONLINE_DEBOUNCE`).
        handle_agent_frame(&pool, &hub, machine_id, heartbeat(), &tx).await?;

        let row = sqlx::query!(
            "SELECT status, last_seen_at FROM machines WHERE id = $1",
            machine_id,
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.status, "online", "Heartbeat must not change status");
        assert_eq!(
            row.last_seen_at.expect("Hello stamped last_seen_at"),
            first_seen,
            "a heartbeat inside the debounce window must not write"
        );

        // A cleared stamp is exactly the state once the window expires (or a
        // session ends) -- simulated rather than sleeping out the real 5s.
        hub.clear_online_stamp(machine_id);
        handle_agent_frame(&pool, &hub, machine_id, heartbeat(), &tx).await?;

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
            "a heartbeat past the debounce window must advance last_seen_at"
        );

        Ok(())
    }

    /// Regression: agent A sending a `Hello` whose self-reported
    /// `info.machine_id` is B's must NOT overwrite B's inventory or create a
    /// phantom row -- updates are keyed by the cert-AUTHENTICATED
    /// `machine_id` UUID, never the self-reported string in the payload.
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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

        // Authenticated as A (the cert-derived param, as `session()` would
        // pass it), but the Hello's self-reported `info.machine_id` is B's --
        // a spoofed identity inside an already-authenticated session.
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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

    /// A LogChunk must reach the tail's sink and must NOT refresh
    /// last_seen_at (same trap as `handle_agent_frame`'s `LogChunk` arm).
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
                cpu_model: None,
                cpu_cores: None,
                boot_time: None,
                virt: None,
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
