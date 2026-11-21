//! Google Antigravity OAuth login, multi-account CLI (`opencode auth`),
//! and interactive `/antigravity` (`/ag`) REPL commands.
//!
//! This facade owns the two dispatch entry points and re-exports the account
//! formatters. Login flows live in [`auth`], pool management in [`pool`],
//! quota rendering in [`quota`], account/model formatting in [`format`] and
//! the `/ag` help text in [`chat`].

mod auth;
mod chat;
mod format;
mod pool;
mod quota;

pub(crate) use format::format_accounts_text;
pub(crate) use quota::{format_cached_quotas_text, format_quota_report};

use oc_antigravity::AntigravityService;

use crate::bootstrap::open_store;
use crate::cli::{AuthArgs, AuthCommand};

use self::auth::{
    cmd_login_antigravity, cmd_login_api_key, is_antigravity_provider, normalize_provider_name,
    open_antigravity, resolve_auth_directory,
};
use self::chat::print_chat_ag_help;
use self::format::{format_identity, format_models_text};
use self::pool::{cmd_list, cmd_models, cmd_remove, cmd_status, print_single_account};
use self::quota::cmd_quota;

// The unit tests below reach helpers directly through `use super::*`.
#[cfg(test)]
use self::format::format_duration_ms;
#[cfg(test)]
use self::quota::remaining_percent;
#[cfg(test)]
use oc_antigravity::{AccountStatusSummary, QuotaGroupSnapshot};
#[cfg(test)]
use std::sync::Arc;

/// Execute `opencode auth ...`.
pub(crate) async fn run_auth(args: AuthArgs) -> anyhow::Result<()> {
    let dir = resolve_auth_directory(args.directory)?;
    let store = open_store(args.db).await?;
    let json_mode = args.json;

    match args.command {
        AuthCommand::Login {
            provider,
            no_browser,
            project,
            label,
            code,
            api_key,
        } => {
            if is_antigravity_provider(&provider) && api_key.is_none() {
                let svc = open_antigravity(&store, &dir).await?;
                cmd_login_antigravity(&svc, project, label, no_browser, code, json_mode).await
            } else {
                let resolved_provider = normalize_provider_name(&provider);
                cmd_login_api_key(&store, &resolved_provider, label, api_key, json_mode).await
            }
        }
        AuthCommand::List { provider } => {
            let svc = open_antigravity(&store, &dir).await?;
            cmd_list(&store, &svc, provider.as_deref(), json_mode).await
        }
        AuthCommand::Status { refresh } => {
            let svc = open_antigravity(&store, &dir).await?;
            cmd_status(&svc, refresh, json_mode).await
        }
        AuthCommand::Quota { cached } => {
            let svc = open_antigravity(&store, &dir).await?;
            cmd_quota(&svc, cached, json_mode).await
        }
        AuthCommand::Use { account } => {
            let svc = open_antigravity(&store, &dir).await?;
            let summary = svc.select_account(&account).await?;
            print_single_account("Active account selected:", &summary, json_mode)
        }
        AuthCommand::Enable { account } => {
            let svc = open_antigravity(&store, &dir).await?;
            let summary = svc.set_account_enabled(&account, true)?;
            print_single_account("Account enabled:", &summary, json_mode)
        }
        AuthCommand::Disable { account } => {
            let svc = open_antigravity(&store, &dir).await?;
            let summary = svc.set_account_enabled(&account, false)?;
            print_single_account("Account disabled:", &summary, json_mode)
        }
        AuthCommand::Remove { account } => {
            let svc = open_antigravity(&store, &dir).await?;
            cmd_remove(&store, &svc, &account, json_mode).await
        }
        AuthCommand::Models { refresh } => {
            let svc = open_antigravity(&store, &dir).await?;
            cmd_models(&svc, refresh, json_mode).await
        }
    }
}

/// Dispatch `/antigravity` or `/ag` inside `opencode chat`.
pub(crate) async fn handle_chat_command(
    service: Option<&AntigravityService>,
    arg: &str,
) -> anyhow::Result<()> {
    let Some(svc) = service else {
        eprintln!("\x1b[33m[antigravity service unavailable]\x1b[0m");
        return Ok(());
    };
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    match tokens.as_slice() {
        [] | ["status"] => {
            let accounts = svc.list_accounts_status()?;
            eprint!("{}", format_accounts_text(&accounts));
        }
        ["quota" | "quotas"] => match svc.refresh_quotas_and_models().await {
            Ok(report) => eprint!("{}", format_quota_report(&report)),
            Err(err) => {
                eprintln!("\x1b[33m[live quota fetch failed ({err}); showing cache]\x1b[0m");
                let accounts = svc.list_accounts_status()?;
                eprint!("{}", format_cached_quotas_text(&accounts));
            }
        },
        ["quota" | "quotas", "cached"] => {
            let accounts = svc.list_accounts_status()?;
            eprint!("{}", format_cached_quotas_text(&accounts));
        }
        ["use", rest @ ..] if !rest.is_empty() => {
            let target = rest.join(" ");
            let summary = svc.select_account(&target).await?;
            eprintln!("\x1b[2m[selected {}]\x1b[0m", format_identity(&summary));
        }
        ["enable", rest @ ..] if !rest.is_empty() => {
            let target = rest.join(" ");
            let summary = svc.set_account_enabled(&target, true)?;
            eprintln!("\x1b[2m[enabled {}]\x1b[0m", format_identity(&summary));
        }
        ["disable", rest @ ..] if !rest.is_empty() => {
            let target = rest.join(" ");
            let summary = svc.set_account_enabled(&target, false)?;
            eprintln!("\x1b[2m[disabled {}]\x1b[0m", format_identity(&summary));
        }
        ["models"] => {
            let models = svc.antigravity_model_infos();
            eprint!("{}", format_models_text(&models));
        }
        ["models", "refresh"] => {
            let report = svc.refresh_quotas_and_models().await?;
            let models = svc.antigravity_model_infos();
            eprintln!(
                "\x1b[2m[refreshed {} upstream model(s); total {} antigravity models]\x1b[0m",
                report.discovered_model_count,
                models.len()
            );
            eprint!("{}", format_models_text(&models));
        }
        _ => print_chat_ag_help(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use oc_antigravity::{
        AntigravityConfig, AntigravityEndpoints, AntigravityTokenExchange, CachedAccountQuota,
    };

    use super::*;

    #[test]
    fn quota_and_duration_formatting_handle_fractions_and_percentages() {
        assert_eq!(format_duration_ms(5_400), "5s");
        assert_eq!(format_duration_ms(125_000), "2m 5s");
        assert_eq!(format_duration_ms(3_720_000), "1h 2m");

        assert_eq!(
            remaining_percent(&QuotaGroupSnapshot {
                remaining_fraction: Some(0.42),
                used_percent: None,
                reset_time_ms: None,
                ..QuotaGroupSnapshot::default()
            }),
            Some(42.0)
        );
        assert_eq!(
            remaining_percent(&QuotaGroupSnapshot {
                remaining_fraction: None,
                used_percent: Some(30.0),
                reset_time_ms: None,
                ..QuotaGroupSnapshot::default()
            }),
            Some(70.0)
        );

        let mut groups = BTreeMap::new();
        groups.insert(
            "claude".to_owned(),
            QuotaGroupSnapshot {
                remaining_fraction: Some(0.8),
                used_percent: None,
                reset_time_ms: None,
                ..QuotaGroupSnapshot::default()
            },
        );
        let accounts = vec![AccountStatusSummary {
            index: 0,
            id: "hash1".to_owned(),
            email: Some("dev@example.com".to_owned()),
            label: Some("Work".to_owned()),
            active_claude: true,
            active_gemini: false,
            enabled: true,
            verification_required: false,
            verification_url: None,
            revoked: false,
            project_id: Some("proj-1".to_owned()),
            tier: Some("pro".to_owned()),
            rate_limits: std::collections::HashMap::new(),
            quota: Some(CachedAccountQuota {
                updated_at: 100,
                groups,
            }),
            added_at: 100,
            last_used: 100,
        }];
        let txt = format_accounts_text(&accounts);
        assert!(txt.contains("dev@example.com [Work]"));
        assert!(txt.contains("claude"));

        let qtxt = format_cached_quotas_text(&accounts);
        assert!(qtxt.contains("80% left"));
    }

    #[tokio::test]
    async fn offline_auth_pool_management_and_chat_commands_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            oc_store::Store::open(dir.path().join("test.sqlite"))
                .await
                .unwrap(),
        );
        let svc = AntigravityService::in_memory_for_test(
            Some(Arc::clone(&store)),
            AntigravityConfig::default(),
            AntigravityEndpoints::default(),
        )
        .await
        .unwrap();

        svc.upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//r1|proj-a".to_owned(),
                refresh_token: "1//r1".to_owned(),
                access: "ya29.a1".to_owned(),
                expires: 4_000_000_000_000,
                email: Some("first@example.com".to_owned()),
                project_id: "proj-a".to_owned(),
                explicit_project_id: Some("proj-a".to_owned()),
                managed_project_id: None,
            },
            Some("First".to_owned()),
        )
        .await
        .unwrap();
        svc.upsert_exchange_result(
            AntigravityTokenExchange {
                refresh: "1//r2|proj-b".to_owned(),
                refresh_token: "1//r2".to_owned(),
                access: "ya29.a2".to_owned(),
                expires: 4_000_000_000_000,
                email: Some("second@example.com".to_owned()),
                project_id: "proj-b".to_owned(),
                explicit_project_id: Some("proj-b".to_owned()),
                managed_project_id: None,
            },
            None,
        )
        .await
        .unwrap();

        handle_chat_command(Some(&svc), "use second@example.com")
            .await
            .unwrap();
        handle_chat_command(Some(&svc), "disable first@example.com")
            .await
            .unwrap();
        let accounts = svc.list_accounts_status().unwrap();
        assert!(!accounts[0].enabled);
        assert!(accounts[1].active_claude);

        handle_chat_command(Some(&svc), "enable first@example.com")
            .await
            .unwrap();
        assert!(svc.list_accounts_status().unwrap()[0].enabled);

        cmd_remove(&store, &svc, "second@example.com", false)
            .await
            .unwrap();
        assert_eq!(svc.account_count(), 1);
    }
}
