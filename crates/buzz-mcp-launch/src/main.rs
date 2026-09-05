//! Entry point for `buzz-mcp-launch`.

use std::process::ExitCode;
use std::sync::Arc;

use buzz_mcp_launch::cli::{parse_env_pairs, Cli, Mode};
use buzz_mcp_launch::env::build_child_env;
use buzz_mcp_launch::launch::{self, LaunchSpec};
use buzz_mcp_launch::proxy::limits::ProxyLimits;
use buzz_mcp_launch::proxy::upstream::{validate_upstream, AuthScheme};
use buzz_mcp_launch::proxy::{Proxy, ProxyConfig};
use buzz_mcp_launch::{inherited_environment, resolve_references};
use buzz_secret_store::{AgentCapability, McpSecretLookup, McpSecretRef};
use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            // Every failure reaches the operator on stderr with its context;
            // the adapter captures this stream.
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.mode {
        Mode::Launch(args) => {
            let (literals, references) = parse_env_pairs(&args.set, &args.secret)?;
            let inherited = inherited_environment();
            let capability = AgentCapability::from_env(|name| inherited.get(name).cloned());
            let lookup = McpSecretLookup::new(blob_source(&cli.service));
            let secrets = resolve_references(&references, capability, &lookup)?;
            let env = build_child_env(&inherited, &literals, &secrets);

            let mut command = args.command.into_iter();
            let program = command
                .next()
                .ok_or("launch mode needs a command after `--`")?;
            tracing::info!(server = %args.server, "starting mcp server");
            Ok(launch::run(LaunchSpec {
                command: program,
                args: command.collect(),
                env,
            })
            .await?)
        }
        Mode::Proxy(args) => {
            let url = validate_upstream(&args.url)?;
            let auth = args
                .auth_scheme
                .as_deref()
                .map(str::parse::<AuthScheme>)
                .transpose()?;
            let secret = args
                .secret
                .as_deref()
                .map(McpSecretRef::parse)
                .transpose()?;
            let capability = AgentCapability::from_env(|name| std::env::var(name).ok()).ok();
            if secret.is_some() && capability.is_none() {
                return Err(
                    "the upstream needs a credential but no capability was inherited".into(),
                );
            }
            let proxy = Arc::new(Proxy::new(ProxyConfig {
                url,
                auth,
                secret,
                capability,
                secrets: blob_source(&cli.service),
                limits: ProxyLimits::default(),
            })?);
            proxy.run(tokio::io::stdin(), tokio::io::stdout()).await?;
            Ok(0)
        }
    }
}

#[cfg(feature = "system-keyring")]
fn blob_source(service: &str) -> buzz_secret_store::keyring_source::KeyringBlobSource {
    buzz_secret_store::keyring_source::KeyringBlobSource::new(service)
}

/// Without the keyring backend there is no store to read, and a reference must
/// fail loudly rather than resolve to nothing.
#[cfg(not(feature = "system-keyring"))]
fn blob_source(_service: &str) -> UnavailableStore {
    UnavailableStore
}

#[cfg(not(feature = "system-keyring"))]
struct UnavailableStore;

#[cfg(not(feature = "system-keyring"))]
impl buzz_secret_store::SecretBlobSource for UnavailableStore {
    fn read_blob(&self) -> Result<Option<Vec<u8>>, String> {
        Err("this build has no keyring backend (`system-keyring` is off)".to_string())
    }
}
