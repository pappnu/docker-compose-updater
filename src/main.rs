mod compose;
mod docker;
mod email;

use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

use clap::Parser;
use tokio::task::JoinHandle;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Update a Docker Compose stack and roll back unhealthy updates"
)]
struct Args {
    #[arg(long)]
    compose_file: PathBuf,
    #[arg(long)]
    wait_seconds: u64,
    #[arg(long, default_value_t = 300)]
    max_wait_seconds: u64,
    #[arg(long, requires_all = ["email_to", "email_from"])]
    smtp_url: Option<String>,
    #[arg(long, requires_all = ["smtp_url", "email_from"])]
    email_to: Option<String>,
    #[arg(long, requires_all = ["smtp_url", "email_to"])]
    email_from: Option<String>,
    #[arg(long)]
    prune: bool,
}

impl Args {
    fn email_settings(&self) -> Option<(&str, &str, &str)> {
        match (&self.smtp_url, &self.email_from, &self.email_to) {
            (Some(smtp_url), Some(email_from), Some(email_to)) => {
                Some((smtp_url, email_from, email_to))
            }
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut email_tasks: Vec<JoinHandle<Result<(), String>>> = Vec::new();
    let result = run_update(&args, &mut email_tasks).await;

    if let Err(error) = &result {
        eprintln!("Update failed: {error}");
        queue_email(
            &args,
            &mut email_tasks,
            "Docker Compose update failed".to_owned(),
            error.clone(),
        );
    }

    let mut email_failed = false;
    for task in email_tasks {
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("Could not send email: {error}");
                email_failed = true;
            }
            Err(error) => {
                eprintln!("Email task failed: {error}");
                email_failed = true;
            }
        }
    }

    if result.is_ok() && !email_failed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn run_update(
    args: &Args,
    email_tasks: &mut Vec<JoinHandle<Result<(), String>>>,
) -> Result<(), String> {
    let compose_path = std::fs::canonicalize(&args.compose_file).map_err(|error| {
        format!(
            "Could not access compose file {}: {error}",
            args.compose_file.display()
        )
    })?;
    let backup_path = compose::backup_path(&compose_path);

    if backup_path.exists() {
        match compose::backup_matches(&compose_path, &backup_path) {
            Ok(true) => {
                compose::restore_backup(&compose_path, &backup_path)?;
                eprintln!("Restored compatible compose backup.");
            }
            Ok(false) => eprintln!(
                "Ignoring compose backup {} because its service/image bases differ.",
                backup_path.display()
            ),
            Err(error) => eprintln!(
                "Ignoring unreadable compose backup {}: {error}",
                backup_path.display()
            ),
        }
    }

    let services = compose::service_base_images(&compose_path)?;
    for (service, image) in &services {
        if image.is_none() {
            eprintln!("Service '{service}' has no image and cannot be version-snapshotted.");
        }
    }

    let versions = docker::running_versions(&compose_path).await?;
    if versions.is_empty() {
        eprintln!("No running container versions were found to use for rollback.");
    }

    let update_output = match docker::compose_up(&compose_path).await {
        Ok(output) => output,
        Err(error) => {
            return Err(rollback_after_failure(
                &compose_path,
                &versions,
                format!("Could not start the Compose update: {error}"),
            )
            .await);
        }
    };
    if !update_output.success {
        let failed_containers = match docker::health_report(&compose_path).await {
            Ok(report) => {
                queue_missing_health_email(args, &report.missing_healthchecks, email_tasks);
                if report.failed.is_empty() {
                    "none detected".to_owned()
                } else {
                    report.failed.join(", ")
                }
            }
            Err(error) => format!("could not inspect containers: {error}"),
        };
        return Err(rollback_after_failure(
            &compose_path,
            &versions,
            format!(
                "Compose update command failed.\nFailed containers: {failed_containers}\nCommand output:\n{}",
                update_output.render(),
            ),
        )
        .await);
    }

    let health_wait_started = tokio::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(args.wait_seconds)).await;

    let health = loop {
        let health = match docker::health_report(&compose_path).await {
            Ok(health) => health,
            Err(error) => {
                return Err(rollback_after_failure(
                    &compose_path,
                    &versions,
                    format!(
                        "Could not inspect container health after the update.\nUpdate command output:\n{}\nHealth inspection error: {error}",
                        update_output.render()
                    ),
                )
                .await);
            }
        };
        if !health.failed.is_empty() || health.starting.is_empty() {
            break health;
        }

        let remaining = std::time::Duration::from_secs(args.max_wait_seconds)
            .saturating_sub(health_wait_started.elapsed());
        if remaining.is_zero() {
            queue_missing_health_email(args, &health.missing_healthchecks, email_tasks);
            return Err(rollback_after_failure(
                &compose_path,
                &versions,
                format!(
                    "Container health checks did not settle within {} seconds. Still starting: {}\nUpdate command output:\n{}",
                    args.max_wait_seconds,
                    health.starting.join(", "),
                    update_output.render()
                ),
            )
            .await);
        }
        tokio::time::sleep(std::time::Duration::from_secs(args.wait_seconds)).await;
    };
    queue_missing_health_email(args, &health.missing_healthchecks, email_tasks);

    if !health.failed.is_empty() {
        return Err(rollback_after_failure(
            &compose_path,
            &versions,
            format!(
                "Container health checks failed: {}\nUpdate command output:\n{}",
                health.failed.join(", "),
                update_output.render()
            ),
        )
        .await);
    }

    match std::fs::remove_file(&backup_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(rollback_after_failure(
                &compose_path,
                &versions,
                format!(
                    "Could not remove compose backup {}: {error}",
                    backup_path.display()
                ),
            )
            .await);
        }
    }

    if args.prune {
        let output = docker::system_prune().await?;
        if !output.success {
            return Err(format!(
                "Docker system prune command failed after the Compose update succeeded.\nCommand output:\n{}",
                output.render()
            ));
        }
    }

    let version_details = match docker::running_versions(&compose_path).await {
        Ok(updated_versions) => {
            let changes: Vec<String> = updated_versions
                .iter()
                .filter_map(|(service, new_version)| {
                    let old_version = versions.get(service);
                    if old_version == Some(new_version) {
                        None
                    } else {
                        Some(format!(
                            "{service}: {} -> {new_version}",
                            old_version.map_or("(previous version unavailable)", String::as_str)
                        ))
                    }
                })
                .collect();

            if changes.is_empty() {
                "No container image version changes were detected.".to_owned()
            } else {
                format!("Updated container image versions:\n{}", changes.join("\n"))
            }
        }
        Err(error) => format!("Could not inspect updated container versions: {error}"),
    };
    queue_email(
        args,
        email_tasks,
        "Docker Compose update succeeded".to_owned(),
        format!(
            "Compose file updated: {}\n\n{version_details}",
            compose_path.display()
        ),
    );

    println!("Compose update completed successfully.");
    Ok(())
}

fn queue_missing_health_email(
    args: &Args,
    missing_healthchecks: &[String],
    email_tasks: &mut Vec<JoinHandle<Result<(), String>>>,
) {
    if missing_healthchecks.is_empty() {
        return;
    }
    queue_email(
        args,
        email_tasks,
        "Docker containers without health checks".to_owned(),
        format!(
            "These containers have no health check configured:\n{}",
            missing_healthchecks.join("\n")
        ),
    );
}

fn queue_email(
    args: &Args,
    email_tasks: &mut Vec<JoinHandle<Result<(), String>>>,
    subject: String,
    body: String,
) {
    let Some((smtp_url, email_from, email_to)) = args.email_settings() else {
        return;
    };
    email_tasks.push(email::queue(
        smtp_url.to_owned(),
        email_from.to_owned(),
        email_to.to_owned(),
        subject,
        body,
    ));
}

async fn rollback_after_failure(
    compose_path: &std::path::Path,
    versions: &BTreeMap<String, String>,
    mut details: String,
) -> String {
    if versions.is_empty() {
        details.push_str("\nRollback was not possible because no running versions were captured.");
        return details;
    }

    let backup = compose::backup_path(compose_path);
    if let Err(error) = compose::backup_current(compose_path) {
        details.push_str(&format!(
            "\nCould not back up the current compose file: {error}"
        ));
    }

    match compose::apply_image_overrides(compose_path, versions) {
        Ok(()) => match docker::compose_rollback(compose_path).await {
            Ok(output) => {
                details.push_str(&format!("\nRollback command output:\n{}", output.render()));
                if output.success {
                    details.push_str("\nRollback to the noted container versions completed.");
                } else {
                    details.push_str("\nRollback command failed.");
                }
            }
            Err(error) => details.push_str(&format!("\nCould not run rollback command: {error}")),
        },
        Err(error) => details.push_str(&format!(
            "\nCould not apply rollback image versions: {error}"
        )),
    }

    details.push_str(&format!("\nCompose backup: {}", backup.display()));
    details
}
