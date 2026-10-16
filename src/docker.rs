use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    process::Output,
};

use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn render(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

#[derive(Debug, Default)]
pub struct HealthReport {
    pub missing_healthchecks: Vec<String>,
    pub failed: Vec<String>,
    pub starting: Vec<String>,
}

pub async fn compose_up(compose_path: &Path) -> Result<CommandOutput, String> {
    compose_up_with_args(
        compose_path,
        &["up", "-d", "--build", "--pull", "always", "--quiet-pull"],
        "docker compose up -d --build --pull always --quiet-pull",
    )
    .await
}

pub async fn compose_rollback(compose_path: &Path) -> Result<CommandOutput, String> {
    compose_up_with_args(
        compose_path,
        &["up", "-d", "--no-build", "--pull", "always", "--quiet-pull"],
        "docker compose up -d --no-build --pull always --quiet-pull",
    )
    .await
}

async fn compose_up_with_args(
    compose_path: &Path,
    args: &[&str],
    description: &str,
) -> Result<CommandOutput, String> {
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(compose_path)
        .args(args);
    execute(command, description).await
}

pub async fn system_prune() -> Result<CommandOutput, String> {
    let mut command = Command::new("docker");
    command.args(["system", "prune", "--all", "--force"]);
    execute(command, "docker system prune --all --force").await
}

pub async fn running_versions(compose_path: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(compose_path)
        .args(["ps", "-q"]);
    let output = execute(command, "docker compose ps -q").await?;
    if !output.success {
        return Err(format!(
            "Could not list running containers: {}",
            output.render()
        ));
    }

    let ids: Vec<&str> = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut command = Command::new("docker");
    command.arg("inspect").args(ids);
    let output = execute(command, "docker inspect").await?;
    if !output.success {
        return Err(format!(
            "Could not inspect running containers: {}",
            output.render()
        ));
    }
    let containers: Vec<ContainerInspect> = serde_json::from_str(&output.stdout)
        .map_err(|error| format!("Could not parse docker inspect output: {error}"))?;

    let image_ids: Vec<&str> = containers
        .iter()
        .map(|container| container.image_id.as_str())
        .collect();
    if image_ids.iter().any(|id| id.is_empty()) {
        return Err("A running container has no Docker image ID".to_owned());
    }

    let mut command = Command::new("docker");
    command.arg("image").arg("inspect").args(image_ids);
    let output = execute(command, "docker image inspect").await?;
    if !output.success {
        return Err(format!(
            "Could not inspect running container images: {}",
            output.render()
        ));
    }
    let images: Vec<ImageInspect> = serde_json::from_str(&output.stdout)
        .map_err(|error| format!("Could not parse docker image inspect output: {error}"))?;
    let repo_digests: HashMap<String, Vec<String>> = images
        .into_iter()
        .map(|image| (image.id, image.repo_digests.unwrap_or_default()))
        .collect();

    let mut versions = BTreeMap::new();
    for container in containers {
        record_running_version(&container, &repo_digests, &mut versions)?;
    }

    Ok(versions)
}

fn record_running_version(
    container: &ContainerInspect,
    repo_digests: &HashMap<String, Vec<String>>,
    versions: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let service = container.service_name()?;
    let image_reference = container.rollback_image_reference(repo_digests)?;
    if let Some(existing) = versions.get(service) {
        if existing != &image_reference {
            return Err(format!(
                "Running replicas of service '{service}' use inconsistent image references: {} and {}",
                existing, image_reference
            ));
        }
    } else {
        versions.insert(service.to_owned(), image_reference);
    }
    Ok(())
}

pub async fn health_report(compose_path: &Path) -> Result<HealthReport, String> {
    let mut command = Command::new("docker");
    command
        .arg("compose")
        .arg("-f")
        .arg(compose_path)
        .args(["ps", "-a", "-q"]);
    let output = execute(command, "docker compose ps -a -q").await?;
    if !output.success {
        return Err(format!(
            "Could not list containers for health inspection: {}",
            output.render()
        ));
    }

    let ids: Vec<&str> = output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return Ok(HealthReport::default());
    }

    let mut command = Command::new("docker");
    command.arg("inspect").args(ids);
    let output = execute(command, "docker inspect").await?;
    if !output.success {
        return Err(format!(
            "Could not inspect container health: {}",
            output.render()
        ));
    }
    let containers: Vec<ContainerInspect> = serde_json::from_str(&output.stdout)
        .map_err(|error| format!("Could not parse docker inspect output: {error}"))?;

    let mut report = HealthReport::default();
    for container in containers {
        record_health(&container, &mut report);
    }

    Ok(report)
}

async fn execute(mut command: Command, description: &str) -> Result<CommandOutput, String> {
    let output = command
        .output()
        .await
        .map_err(|error| format!("Could not execute {description}: {error}"))?;
    Ok(command_output(output))
}

fn command_output(output: Output) -> CommandOutput {
    CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

fn record_health(container: &ContainerInspect, report: &mut HealthReport) {
    let name = container.display_name();
    let has_healthcheck = container
        .config
        .healthcheck
        .as_ref()
        .is_some_and(|healthcheck| {
            healthcheck.test.as_ref().is_some_and(|test| {
                !test.is_empty() && test.first().is_none_or(|value| value != "NONE")
            })
        });

    if !has_healthcheck {
        report.missing_healthchecks.push(name.clone());
    }
    if container.state.status != "running" {
        report
            .failed
            .push(format!("{name} (state: {})", container.state.status));
    } else if has_healthcheck {
        match container
            .state
            .health
            .as_ref()
            .map(|health| health.status.as_str())
        {
            Some("healthy") => {}
            Some("starting") => report.starting.push(name),
            Some(status) => report.failed.push(format!("{name} (health: {status})")),
            None => report
                .failed
                .push(format!("{name} (health status unavailable)")),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ContainerInspect {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Image", default)]
    image_id: String,
    #[serde(rename = "Config", default)]
    config: ContainerConfig,
    #[serde(rename = "State", default)]
    state: ContainerState,
}

#[cfg(test)]
mod tests {
    use super::{
        ContainerConfig, ContainerHealth, ContainerHealthcheck, ContainerInspect, ContainerState,
        HealthReport, ImageInspect, record_health, record_running_version,
    };
    use std::collections::BTreeMap;

    fn container(
        status: &str,
        healthcheck: Option<ContainerHealthcheck>,
        health: Option<&str>,
    ) -> ContainerInspect {
        ContainerInspect {
            name: "/api-1".to_owned(),
            image_id: "sha256:local123".to_owned(),
            config: ContainerConfig {
                healthcheck,
                ..ContainerConfig::default()
            },
            state: ContainerState {
                status: status.to_owned(),
                health: health.map(|status| ContainerHealth {
                    status: status.to_owned(),
                }),
            },
        }
    }

    fn configured_healthcheck() -> ContainerHealthcheck {
        ContainerHealthcheck {
            test: Some(vec!["CMD-SHELL".to_owned(), "true".to_owned()]),
        }
    }

    #[test]
    fn healthy_container_passes() {
        let mut report = HealthReport::default();
        record_health(
            &container("running", Some(configured_healthcheck()), Some("healthy")),
            &mut report,
        );
        assert!(report.failed.is_empty());
        assert!(report.missing_healthchecks.is_empty());
    }

    #[test]
    fn unhealthy_container_fails() {
        let mut report = HealthReport::default();
        record_health(
            &container("running", Some(configured_healthcheck()), Some("unhealthy")),
            &mut report,
        );
        assert_eq!(report.failed, ["api-1 (health: unhealthy)"]);
    }

    #[test]
    fn starting_container_is_pending_not_failed() {
        let mut report = HealthReport::default();
        record_health(
            &container("running", Some(configured_healthcheck()), Some("starting")),
            &mut report,
        );
        assert!(report.failed.is_empty());
        assert_eq!(report.starting, ["api-1"]);
    }

    #[test]
    fn stopped_container_fails_and_missing_healthcheck_is_reported() {
        let mut report = HealthReport::default();
        record_health(&container("exited", None, None), &mut report);
        assert_eq!(report.failed, ["api-1 (state: exited)"]);
        assert_eq!(report.missing_healthchecks, ["api-1"]);
    }

    #[test]
    fn running_version_pins_configured_image_to_matching_remote_digest() {
        let container: ContainerInspect = serde_json::from_str(
            r#"{
                "Name": "/api-1",
                "Image": "sha256:local123",
                "Config": {
                    "Image": "example/api:latest",
                    "Labels": {
                        "com.docker.compose.service": "api"
                    }
                }
            }"#,
        )
        .unwrap();
        let images: Vec<ImageInspect> = serde_json::from_str(
            r#"[
                {
                    "Id": "sha256:local123",
                    "RepoDigests": [
                        "docker.io/example/api@sha256:first",
                        "docker.io/other/api@sha256:second"
                    ]
                }
            ]"#,
        )
        .unwrap();
        let repo_digests = images
            .into_iter()
            .map(|image| (image.id, image.repo_digests.unwrap_or_default()))
            .collect();
        let mut versions = BTreeMap::new();

        record_running_version(&container, &repo_digests, &mut versions).unwrap();

        assert_eq!(
            versions.get("api"),
            Some(&"example/api@sha256:first".to_owned())
        );
    }
}

#[derive(Debug, Deserialize)]
struct ImageInspect {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "RepoDigests")]
    repo_digests: Option<Vec<String>>,
}

impl ContainerInspect {
    fn rollback_image_reference(
        &self,
        repo_digests: &HashMap<String, Vec<String>>,
    ) -> Result<String, String> {
        let image = &self.config.image;
        if image.is_empty() {
            return Err(format!(
                "Container {} has no configured image reference",
                self.name
            ));
        }
        let image_name = image
            .split_once('@')
            .map_or(image.as_str(), |(name, _)| name);
        let image_name = match image_name.rfind(':') {
            Some(colon) if colon > image_name.rfind('/').unwrap_or(0) => &image_name[..colon],
            _ => image_name,
        };
        let digest = repo_digests
            .get(&self.image_id)
            .and_then(|digests| digests.first())
            .and_then(|repo_digest| repo_digest.split_once('@').map(|(_, digest)| digest))
            .ok_or_else(|| format!("Container {} has no remote image digest", self.name))?;
        Ok(format!("{image_name}@{digest}"))
    }

    fn service_name(&self) -> Result<&str, String> {
        self.config
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.docker.compose.service"))
            .map(String::as_str)
            .ok_or_else(|| format!("Container {} has no Compose service label", self.name))
    }

    fn display_name(&self) -> String {
        let name = self.name.trim_start_matches('/');
        if name.is_empty() {
            "<unnamed container>".to_owned()
        } else {
            name.to_owned()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ContainerConfig {
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
    #[serde(rename = "Healthcheck")]
    healthcheck: Option<ContainerHealthcheck>,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerHealthcheck {
    #[serde(rename = "Test")]
    test: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerState {
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "Health")]
    health: Option<ContainerHealth>,
}

#[derive(Debug, Deserialize)]
struct ContainerHealth {
    #[serde(rename = "Status")]
    status: String,
}
