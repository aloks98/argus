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
}

impl AgentSvc {
    pub fn new(ca: Arc<CertAuthority>, pool: PgPool) -> Self {
        Self { ca, pool }
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

        let token_name = match repo::consume_enrollment_token(&mut *tx, &req.join_token)
            .await
            .map_err(|e| internal_error("checking enrollment token", &e))?
        {
            TokenCheck::Valid { token_name } => token_name,
            TokenCheck::Invalid => {
                drop(tx);
                repo::audit(&self.pool, "system", "agent.enroll", None, "denied")
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
                repo::audit(&self.pool, &token_name, "agent.enroll", None, "denied")
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
                repo::audit(&self.pool, &token_name, "agent.enroll", None, "error")
                    .await
                    .map_err(|e| internal_error("writing audit log", &e))?;
                return Err(status);
            }
        };

        repo::audit(&mut *tx, &token_name, "agent.enroll", Some(agent_id), "ok")
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
        let mut inbound = request.into_inner();

        tokio::spawn(async move {
            while let Some(item) = inbound.next().await {
                match item {
                    Ok(frame) => {
                        if let Err(e) = handle_agent_frame(&pool, machine_id, frame, &tx).await {
                            tracing::warn!(error = %e, %machine_id, "session: error handling agent frame");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, %machine_id, "session: inbound stream error");
                        break;
                    }
                }
            }
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
            // `mark_online` already stamps `last_seen_at = now()`, so a
            // freshly-online machine isn't immediately eligible for the 45s
            // offline sweeper -- no extra `touch_last_seen` needed here.
            repo::mark_online(pool, machine_id).await?;
            repo::audit(
                pool,
                &machine_id.to_string(),
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
        Some(agent_frame::Payload::Heartbeat(_)) => {
            repo::touch_last_seen(pool, machine_id).await?;
        }
        _ => {
            // Metrics/docker/systemd/logs/pty/etc. are later slices; ignore
            // for the Spine.
        }
    }

    Ok(())
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
        let svc = AgentSvc::new(ca, pool.clone());

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
            "SELECT actor, result FROM audit_log WHERE action = 'agent.enroll' AND result = 'ok'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, "dev-token");

        Ok(())
    }

    #[sqlx::test]
    async fn enroll_with_bad_token_is_denied_and_audited(pool: PgPool) -> anyhow::Result<()> {
        let cipher = FieldCipher::from_b64_key(&STANDARD.encode([5u8; 32]))?;
        let ca = Arc::new(CertAuthority::load_or_init(&pool, &cipher).await?);
        let svc = AgentSvc::new(ca, pool.clone());

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
            "SELECT actor, result FROM audit_log WHERE action = 'agent.enroll' AND result = 'denied'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, "system");

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
        let svc = AgentSvc::new(ca, pool.clone());

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
            "SELECT actor, result FROM audit_log WHERE action = 'agent.enroll' AND result = 'error'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(audit_row.actor, "single-use");

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
            },
        )
        .await?;

        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        handle_agent_frame(
            &pool,
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
            },
        )
        .await?;

        let machine_count_before = sqlx::query!("SELECT count(*) AS count FROM machines")
            .fetch_one(&pool)
            .await?
            .count
            .unwrap_or(0);

        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(4);

        // Authenticated as A (per the cert-derived `machine_id` param, exactly
        // as `session()` would pass it), but the Hello's self-reported
        // `info.machine_id` is B's machine_id string -- a spoofed/misreported
        // identity inside an already-authenticated session.
        handle_agent_frame(
            &pool,
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
}
