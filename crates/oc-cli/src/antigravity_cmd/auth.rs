//! `opencode auth login`: the Google Antigravity OAuth handshake (loopback
//! listener or pasted callback), and plain API-key credentials.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use oc_antigravity::{
    AccountStatusSummary, AntigravityEndpoints, AntigravityService, OAuthAttemptStatusView,
};
use oc_store::NewCredential;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::format::format_identity;

pub(super) async fn open_antigravity(
    store: &Arc<oc_store::Store>,
    directory: &Path,
) -> anyhow::Result<AntigravityService> {
    AntigravityService::open(
        Some(Arc::clone(store)),
        None,
        Some(directory.to_path_buf()),
        AntigravityEndpoints::default(),
    )
    .await
    .map_err(anyhow::Error::new)
}

pub(super) fn resolve_auth_directory(directory: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let dir = match directory {
        Some(path) => path,
        None => std::env::current_dir().context("resolve working directory")?,
    };
    std::path::absolute(&dir).with_context(|| format!("resolve directory {}", dir.display()))
}

pub(super) fn is_antigravity_alias(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "antigravity" | "google-antigravity"
    )
}

pub(super) fn is_antigravity_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "google" | "antigravity" | "google-antigravity"
    )
}

pub(super) fn normalize_provider_name(provider: &str) -> String {
    let lower = provider.trim().to_ascii_lowercase();
    if is_antigravity_provider(&lower) {
        "google".to_owned()
    } else {
        lower
    }
}

pub(super) async fn cmd_login_antigravity(
    svc: &AntigravityService,
    project: Option<String>,
    label: Option<String>,
    no_browser: bool,
    explicit_code: Option<String>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let use_loopback = !no_browser && explicit_code.is_none();
    let attempt = svc
        .start_oauth(project, label, use_loopback, None)
        .await
        .context("start Google Antigravity OAuth")?;

    let completed = if let Some(code_input) = explicit_code {
        Some(
            svc.complete_oauth(&attempt.attempt_id, Some(code_input.trim()))
                .await?,
        )
    } else if attempt.mode == "auto" {
        eprintln!("Open this URL in your browser to sign in with Google:\n");
        eprintln!("  {}\n", attempt.url);
        open_browser_best_effort(&attempt.url);
        eprintln!("Waiting for OAuth callback on http://127.0.0.1:51121 ...");
        eprintln!("(Or paste the callback URL / authorization code and press Enter)");
        wait_for_loopback_or_stdin(svc, &attempt.attempt_id).await?
    } else {
        eprintln!("Open this URL in your browser to sign in with Google:\n");
        eprintln!("  {}\n", attempt.url);
        eprintln!("{}\n", attempt.instructions);
        let input = prompt_line("Paste callback URL or authorization code: ").await?;
        if input.is_empty() {
            let _ = svc.cancel_oauth(&attempt.attempt_id).await;
            bail!("OAuth cancelled: empty authorization input");
        }
        Some(
            svc.complete_oauth(&attempt.attempt_id, Some(&input))
                .await?,
        )
    };

    let mut refresh_warning = None;
    if let Err(err) = svc.refresh_quotas_and_models().await {
        refresh_warning = Some(err.to_string());
    }

    let accounts = svc.list_accounts_status()?;
    let active_summary = completed.or_else(|| accounts.last().cloned());

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "account": active_summary,
                "accountCount": accounts.len(),
                "usableCount": svc.usable_account_count(),
                "refreshWarning": refresh_warning,
            }))?
        );
    } else {
        if let Some(summary) = &active_summary {
            println!("Authenticated Google account: {}", format_identity(summary));
            if let Some(proj) = &summary.project_id {
                println!("Cloud Code project: {proj}");
            }
            if let Some(tier) = &summary.tier {
                println!("Subscription tier:  {tier}");
            }
        }
        println!(
            "Pool now has {} account(s) ({} usable).",
            accounts.len(),
            svc.usable_account_count()
        );
        if let Some(warn) = refresh_warning {
            eprintln!("Warning: initial quota/model refresh failed: {warn}");
        }
    }
    Ok(())
}

pub(super) async fn wait_for_loopback_or_stdin(
    svc: &AntigravityService,
    attempt_id: &str,
) -> anyhow::Result<Option<AccountStatusSummary>> {
    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);

    loop {
        if tokio::time::Instant::now() >= deadline {
            let _ = svc.cancel_oauth(attempt_id).await;
            bail!("OAuth login timed out after 5 minutes");
        }
        if let Some(status) = svc.poll_oauth(attempt_id).await {
            match status {
                OAuthAttemptStatusView::Complete { .. } => return Ok(None),
                OAuthAttemptStatusView::Failed { message, .. } => bail!("{message}"),
                OAuthAttemptStatusView::Pending { .. } => {}
            }
        }

        let sleep = tokio::time::sleep(Duration::from_millis(300));
        if stdin_open {
            tokio::select! {
                () = sleep => {}
                read = stdin_lines.next_line() => {
                    match read {
                        Ok(Some(line)) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                let sum = svc.complete_oauth(attempt_id, Some(trimmed)).await?;
                                return Ok(Some(sum));
                            }
                        }
                        Ok(None) | Err(_) => stdin_open = false,
                    }
                }
            }
        } else {
            sleep.await;
        }
    }
}

pub(super) async fn cmd_login_api_key(
    store: &oc_store::Store,
    provider: &str,
    label: Option<String>,
    api_key: Option<String>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let secret = match api_key {
        Some(key) if !key.trim().is_empty() => key.trim().to_owned(),
        _ => {
            if !std::io::stdin().is_terminal() {
                bail!("pass `--api-key` when stdin is not interactive");
            }
            let entered = prompt_line(&format!("Enter API key for `{provider}`: ")).await?;
            if entered.is_empty() {
                bail!("API key must not be empty");
            }
            entered
        }
    };

    let created = store
        .create_credential(NewCredential {
            id: None,
            integration_id: provider.to_owned(),
            label,
            value: json!({ "type": "key", "key": secret }),
            active: Some(true),
        })
        .await
        .context("save credential")?;

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&created)?);
    } else {
        let id = created
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("cred");
        println!("Saved credential `{id}` for `{provider}`.");
    }
    Ok(())
}

pub(super) async fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let line = reader.next_line().await?.unwrap_or_default();
    Ok(line.trim().to_owned())
}

pub(super) fn open_browser_best_effort(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
