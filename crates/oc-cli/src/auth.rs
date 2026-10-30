//! Password resolution and the `service get` command.

use anyhow::Context;

use crate::paths::random_password;
use crate::service;

pub(crate) fn resolve_password(
    explicit: Option<String>,
    env_alias: Option<String>,
    service_mode: bool,
    registration: Option<&service::ServiceRegistration>,
) -> Option<String> {
    match explicit {
        Some(password) if !password.is_empty() => return Some(password),
        // `--password ""` is an explicit "no auth", not "look elsewhere".
        Some(_) => return None,
        None => {}
    }
    if let Some(password) = env_alias.filter(|password| !password.is_empty()) {
        return Some(password);
    }
    if service_mode {
        if let Some(password) = registration
            .and_then(|registration| registration.password.clone())
            .filter(|password| !password.is_empty())
        {
            return Some(password);
        }
        return Some(random_password());
    }
    None
}

/// `opencode service get <key>`: read the managed-service registration.
pub(crate) fn service_get(key: &str) -> anyhow::Result<()> {
    let channel = std::env::var("OPENCODE_CHANNEL").unwrap_or_else(|_| "latest".to_owned());
    let path = service::registration_path(&channel);
    let registration = service::read(&path)
        .with_context(|| format!("read {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("no service registration at {}", path.display()))?;
    match key {
        "url" => println!("{}", registration.url),
        "password" => println!("{}", registration.password.unwrap_or_default()),
        "pid" => println!("{}", registration.pid),
        "id" => println!("{}", registration.id),
        "version" => println!("{}", registration.version),
        other => anyhow::bail!(
            "unknown service key {other:?}; expected url, password, pid, id or version"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::service::ServiceRegistration;

    use super::resolve_password;

    fn registration(password: Option<&str>) -> ServiceRegistration {
        ServiceRegistration {
            id: "svc-test".to_owned(),
            version: "0.1.0".to_owned(),
            url: "http://127.0.0.1:4096".to_owned(),
            pid: 42,
            password: password.map(str::to_owned),
        }
    }

    #[test]
    fn explicit_password_wins_and_empty_disables_auth() {
        assert_eq!(
            resolve_password(Some("cli".to_owned()), Some("env".to_owned()), false, None)
                .as_deref(),
            Some("cli")
        );
        assert_eq!(
            resolve_password(Some(String::new()), Some("env".to_owned()), true, None),
            None,
            "an explicit empty password must not fall through to env or a random service password"
        );
    }

    #[test]
    fn server_password_alias_and_service_registration_are_fallbacks() {
        assert_eq!(
            resolve_password(None, Some("env".to_owned()), false, None).as_deref(),
            Some("env")
        );
        let stored = registration(Some("svc-pass"));
        assert_eq!(
            resolve_password(None, None, true, Some(&stored)).as_deref(),
            Some("svc-pass"),
            "a service restart must reuse the registered password"
        );
        assert!(
            resolve_password(
                None,
                Some(String::new()),
                true,
                Some(&registration(Some("")))
            )
            .is_some(),
            "an empty alias/stored password is not a credential; service mode generates one"
        );
        assert!(
            resolve_password(None, None, true, Some(&registration(None))).is_some(),
            "service mode without a stored password generates one"
        );
        assert_eq!(resolve_password(None, None, false, None), None);
    }
}
