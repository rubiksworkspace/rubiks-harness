//! `prime-agent serve uhp`: wires the `pa-uhp` server to this binary's model
//! catalog, settings and agent directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_uhp::{ModelCatalog, ModelEntry, ServerConfig};

use crate::config::{get_agent_dir, APP_NAME};

/// The bearer token: from the environment only, never argv, which any local
/// user can read in a process list.
const TOKEN_ENV: &str = "PRIME_AGENT_UHP_KEY";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8787;

#[derive(Debug, PartialEq, Eq)]
struct ServeArgs {
    host: String,
    port: u16,
    workspace: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    allow_remote: bool,
}

fn parse_args(args: &[String]) -> Result<ServeArgs, String> {
    let mut parsed = ServeArgs {
        host: DEFAULT_HOST.to_string(),
        port: DEFAULT_PORT,
        workspace: None,
        provider: None,
        model: None,
        allow_remote: false,
    };
    let mut rest = args.iter();
    match rest.next().map(String::as_str) {
        Some("uhp") => {}
        Some(other) => return Err(format!("Unknown server: {other}")),
        None => return Err(format!("Usage: {APP_NAME} serve uhp [options]")),
    }
    while let Some(arg) = rest.next() {
        let mut value = |flag: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--host" => parsed.host = value("--host")?,
            "--port" => {
                parsed.port = value("--port")?
                    .parse()
                    .map_err(|_| "--port must be a port number".to_string())?;
            }
            "--workspace" => parsed.workspace = Some(PathBuf::from(value("--workspace")?)),
            "--provider" => parsed.provider = Some(value("--provider")?),
            "--model" => parsed.model = Some(value("--model")?),
            "--allow-remote" => parsed.allow_remote = true,
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok(parsed)
}

/// The catalog as tasks would see it now. The default follows the order a
/// print run uses: the flag, then settings, then the preferred available model.
fn load_catalog(
    agent_dir: &Path,
    args_provider: Option<&str>,
    args_model: Option<&str>,
) -> ModelCatalog {
    let auth = pa_core::auth::AuthStorage::create(agent_dir);
    let mut registry = pa_core::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
    registry.load_private_authorization_from_cache();
    let available: Vec<pa_types::ai::Model> =
        registry.get_available().into_iter().cloned().collect();
    let is_available = |model: &pa_types::ai::Model| {
        available
            .iter()
            .any(|other| other.id == model.id && other.provider == model.provider)
    };
    let mut entries: Vec<ModelEntry> = registry
        .get_all()
        .iter()
        .map(|model| ModelEntry {
            id: model.id.clone(),
            provider: model.provider.clone(),
            label: model.name.clone(),
            available: is_available(model),
        })
        .collect();

    let cwd = std::env::current_dir().unwrap_or_else(|_| agent_dir.to_path_buf());
    let settings = pa_core::settings::SettingsManager::create(&cwd, agent_dir);
    let preferred_provider = args_provider
        .map(String::from)
        .or_else(|| settings.get_default_provider().map(String::from));
    let mut default = args_model
        .map(String::from)
        .or_else(|| settings.get_default_model().map(String::from));
    let candidates: Vec<pa_types::ai::Model> = available
        .iter()
        .filter(|model| args_provider.is_none_or(|provider| model.provider == provider))
        .cloned()
        .collect();
    let fallback = pa_core::models::find_preferred_default_model(&candidates)
        .or_else(|| candidates.first())
        .map(|model| format!("{}/{}", model.provider, model.id));

    // The faux seam: a scripted run has one model that is always ready, so the
    // protocol can be exercised end to end with no provider and no key.
    if std::env::var_os("PRIME_AGENT_FAUX_SCRIPT").is_some() {
        entries.insert(
            0,
            ModelEntry {
                id: "faux-1".to_string(),
                provider: "faux".to_string(),
                label: "Faux".to_string(),
                available: true,
            },
        );
        default = default.or_else(|| Some("faux-1".to_string()));
    }
    let mut catalog = ModelCatalog {
        default,
        preferred_provider,
        entries,
    };
    // A configured default that cannot run must not hide one that can.
    if catalog.default_entry().is_none() {
        catalog.default = fallback;
    }
    catalog
}

pub(crate) fn run_serve_command(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return 2;
        }
    };
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    if token.is_empty() {
        eprintln!("Set {TOKEN_ENV} to the bearer token clients must present.");
        return 2;
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("Could not find the {APP_NAME} binary: {error}");
            return 1;
        }
    };
    let agent_dir = get_agent_dir();
    let catalog_dir = agent_dir.clone();
    let (provider, model) = (parsed.provider, parsed.model);
    let config = ServerConfig {
        host: parsed.host,
        port: parsed.port,
        allow_remote: parsed.allow_remote,
        token,
        token_env: TOKEN_ENV.to_string(),
        state_dir: agent_dir.join("uhp"),
        workspace: parsed.workspace,
        executable,
        version: env!("CARGO_PKG_VERSION").to_string(),
        catalog: Arc::new(move || {
            load_catalog(&catalog_dir, provider.as_deref(), model.as_deref())
        }),
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Could not start the server: {error}");
            return 1;
        }
    };
    let result = runtime.block_on(pa_uhp::serve(config));
    // Tasks already got the shutdown wait; a stuck stream must not hold exit.
    runtime.shutdown_background();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn serving_stays_on_loopback_unless_told_otherwise() {
        let parsed = parse_args(&strings(&["uhp"])).unwrap();
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8787);
        assert!(!parsed.allow_remote);
    }

    #[test]
    fn options_are_read() {
        let parsed = parse_args(&strings(&[
            "uhp",
            "--port",
            "0",
            "--model",
            "m",
            "--provider",
            "p",
            "--workspace",
            "/w",
            "--host",
            "0.0.0.0",
            "--allow-remote",
        ]))
        .unwrap();
        assert_eq!(parsed.port, 0);
        assert_eq!(parsed.model.as_deref(), Some("m"));
        assert_eq!(parsed.provider.as_deref(), Some("p"));
        assert_eq!(parsed.workspace, Some(PathBuf::from("/w")));
        assert!(parsed.allow_remote);
    }

    #[test]
    fn bad_invocations_are_refused() {
        assert!(parse_args(&strings(&[])).is_err());
        assert!(parse_args(&strings(&["acp"])).is_err());
        assert!(parse_args(&strings(&["uhp", "--port", "nope"])).is_err());
        assert!(parse_args(&strings(&["uhp", "--token", "x"])).is_err());
        assert!(parse_args(&strings(&["uhp", "--model"])).is_err());
    }
}
