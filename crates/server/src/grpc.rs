//! Agent-facing gRPC surface (PRD §4, §5).
//!
//! Served on the MetalLB LoadBalancer address with mTLS terminated here -- never
//! via Traefik (PRD §2.4). The rustls listener uses OPTIONAL client auth; a tonic
//! interceptor enforces per-RPC policy: `Enroll` is the only RPC allowed without a
//! client cert, everything else requires a validated cert whose subject yields the
//! `agent_id`.

#![allow(dead_code)]

use crate::config::Config;
use anyhow::Result;
use argus_proto::v1::agent_service_server::AgentService;
use argus_proto::v1::{AgentFrame, EnrollRequest, EnrollResponse, ServerFrame};
use std::pin::Pin;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

#[derive(Default)]
pub struct AgentSvc;

#[tonic::async_trait]
impl AgentService for AgentSvc {
    async fn enroll(
        &self,
        _request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        // TODO(spine): validate join token, sign CSR, upsert machine, audit (§5.3).
        Err(Status::unimplemented("enroll -- build slice #1, PRD §5.3"))
    }

    type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerFrame, Status>> + Send>>;

    async fn session(
        &self,
        _request: Request<Streaming<AgentFrame>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        // TODO(spine): require a validated client cert -> agent_id, then run the
        // multiplexed bidi loop (§4, §5.4).
        Err(Status::unimplemented("session -- build slice #1, PRD §5.4"))
    }
}

/// Serve the agent-facing mTLS gRPC surface. Enable tonic's `tls-ring` feature and
/// build a rustls `ServerConfig` with optional client auth when wiring this.
pub async fn serve(_cfg: &Config) -> Result<()> {
    todo!("agent gRPC serve -- build slice #1, PRD §5.4")
}
