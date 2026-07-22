//! Docker collection + verb execution (Docker slice). Talks ONLY to the local
//! daemon over its unix socket via bollard — never TCP/TLS, so the musl-static
//! build stays free of openssl (see Cargo.toml). Mapping is factored into pure
//! functions so it's unit-testable without a running daemon.

use argus_proto::v1::{CommandResult, Container, Verb};
use bollard::models::ContainerSummary;
use bollard::query_parameters::{
    ListContainersOptionsBuilder, RestartContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::Docker;
use std::time::Duration;

/// Cap any single daemon call so a hung dockerd can't stall the Session's
/// heartbeat/metrics sender.
const OP_TIMEOUT: Duration = Duration::from_secs(5);

/// A thin, cheaply-cloneable handle to the local Docker daemon. `inner` is
/// `None` on hosts without Docker (many LXC guests) — such a client reports an
/// empty container list and fails verbs with a clear message, never panicking.
#[derive(Clone)]
pub struct DockerClient {
    inner: Option<Docker>,
}

impl DockerClient {
    /// Best-effort connect to the local socket. Never fails.
    pub fn connect() -> DockerClient {
        match Docker::connect_with_socket_defaults() {
            Ok(d) => DockerClient { inner: Some(d) },
            Err(e) => {
                tracing::warn!(error = %e, "docker: no local daemon; container features disabled");
                DockerClient { inner: None }
            }
        }
    }

    /// All containers (running + stopped) mapped to proto `Container`. Empty on
    /// no-daemon or any listing error (logged) — the fleet view just shows none.
    pub async fn list_containers(&self) -> Vec<Container> {
        let Some(docker) = &self.inner else {
            return Vec::new();
        };
        let opts = ListContainersOptionsBuilder::new().all(true).build();
        match tokio::time::timeout(OP_TIMEOUT, docker.list_containers(Some(opts))).await {
            Ok(Ok(summaries)) => summaries.into_iter().map(summary_to_container).collect(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "docker: list_containers failed");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("docker: list_containers timed out");
                Vec::new()
            }
        }
    }

    /// Run a container verb against `target` (id or name), producing a
    /// `CommandResult` correlated by `command_id`.
    pub async fn run_verb(&self, command_id: String, verb: Verb, target: &str) -> CommandResult {
        let Some(docker) = &self.inner else {
            return result(
                command_id,
                false,
                "docker daemon not available on this host",
            );
        };
        let outcome = match verb {
            Verb::ContainerStart => {
                docker
                    .start_container(target, None::<StartContainerOptions>)
                    .await
            }
            Verb::ContainerStop => {
                docker
                    .stop_container(target, None::<StopContainerOptions>)
                    .await
            }
            Verb::ContainerRestart => {
                docker
                    .restart_container(target, None::<RestartContainerOptions>)
                    .await
            }
            other => return result(command_id, false, &format!("unsupported verb {other:?}")),
        };
        match outcome {
            Ok(()) => result(command_id, true, "ok"),
            Err(e) => result(command_id, false, &e.to_string()),
        }
    }
}

fn result(command_id: String, ok: bool, message: &str) -> CommandResult {
    CommandResult {
        command_id,
        ok,
        exit_code: if ok { 0 } else { 1 },
        message: message.to_string(),
    }
}

/// Map a bollard container summary to the proto. Pure — unit-tested below.
fn summary_to_container(s: ContainerSummary) -> Container {
    let name = s
        .names
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|n| n.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let status = s.status.unwrap_or_default();
    let health = parse_health(&status);
    Container {
        id: s.id.unwrap_or_default(),
        name,
        image: s.image.unwrap_or_default(),
        state: s.state.map(|st| st.to_string()).unwrap_or_default(),
        status,
        health,
    }
}

/// Extract docker's health hint from the ps-style status string, e.g.
/// "Up 2 minutes (healthy)" -> "healthy". "" when no health is present.
fn parse_health(status: &str) -> String {
    if status.contains("(healthy)") {
        "healthy".to_string()
    } else if status.contains("(unhealthy)") {
        "unhealthy".to_string()
    } else if status.contains("(health: starting)") {
        "starting".to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::ContainerSummaryStateEnum;

    #[test]
    fn parse_health_reads_the_status_parenthetical() {
        assert_eq!(parse_health("Up 2 minutes (healthy)"), "healthy");
        assert_eq!(parse_health("Up 5 seconds (health: starting)"), "starting");
        assert_eq!(parse_health("Up 3 hours (unhealthy)"), "unhealthy");
        assert_eq!(parse_health("Up 3 hours"), "");
        assert_eq!(parse_health("Exited (0) 2 days ago"), "");
    }

    #[test]
    fn summary_to_container_strips_leading_slash_and_maps_state() {
        let s = ContainerSummary {
            id: Some("abc123".into()),
            names: Some(vec!["/nginx".into()]),
            image: Some("nginx:latest".into()),
            state: Some(ContainerSummaryStateEnum::RUNNING),
            status: Some("Up 2 minutes (healthy)".into()),
            ..Default::default()
        };
        let c = summary_to_container(s);
        assert_eq!(c.id, "abc123");
        assert_eq!(c.name, "nginx");
        assert_eq!(c.image, "nginx:latest");
        assert_eq!(c.state, "running");
        assert_eq!(c.health, "healthy");
    }

    #[test]
    fn summary_to_container_tolerates_all_missing_fields() {
        let c = summary_to_container(ContainerSummary::default());
        assert_eq!(c.id, "");
        assert_eq!(c.name, "");
        assert_eq!(c.state, "");
        assert_eq!(c.health, "");
    }
}
