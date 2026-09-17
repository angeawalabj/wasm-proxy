use hyper_util::rt::TokioIo;
use std::env;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use wasm_proxy::config::Config;
use wasm_proxy::proxy::{self, ProxyState};
use wasm_proxy::{hot_reload, plugin};

/// Délai laissé aux connexions en cours pour se terminer proprement après
/// un signal d'arrêt, avant qu'on abandonne et quitte quand même.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config_path = env::args().nth(1).unwrap_or_else(|| "config.yaml".to_string());
    let config = Config::load(&config_path)?;
    info!(routes = config.routes.len(), "configuration chargee");

    let plugins = plugin::load_plugins(&config.plugins)?;
    info!(plugins = plugins.len(), "plugins charges");

    let listen_addr = config.listen_addr;
    let listener = TcpListener::bind(listen_addr).await?;
    info!(addr = %listen_addr, "proxy en ecoute");

    let state = ProxyState::new(config, plugins);

    // Le watcher doit rester vivant tout le programme : s'il est droppe, la
    // surveillance s'arrete silencieusement (comportement de la crate notify).
    let _watcher = hot_reload::spawn_watcher(config_path, state.clone())?;

    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, peer_addr) = match accept_result {
                    Ok(pair) => pair,
                    Err(e) => {
                        error!(error = %e, "echec d'acceptation de connexion");
                        continue;
                    }
                };

                let io = TokioIo::new(stream);
                let state = state.clone();

                connections.spawn(async move {
                    proxy::serve_connection(io, peer_addr, state).await;
                });
            }
            _ = shutdown_signal() => {
                info!("signal d'arret recu, arret de l'acceptation de nouvelles connexions");
                break;
            }
        }
    }

    info!(
        connexions_actives = connections.len(),
        grace_period_secs = SHUTDOWN_GRACE_PERIOD.as_secs(),
        "attente de la fin des connexions en cours"
    );

    let drain = async {
        while connections.join_next().await.is_some() {}
    };

    if tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, drain).await.is_err() {
        warn!(
            connexions_restantes = connections.len(),
            "delai de grace depasse, arret force avec des connexions encore actives"
        );
    } else {
        info!("toutes les connexions se sont terminees proprement");
    }

    Ok(())
}

/// Attend un signal d'arrêt (Ctrl+C, ou SIGTERM sous Unix — le signal
/// standard envoyé par `docker stop`, `systemctl stop`, Kubernetes, etc.).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("echec d'installation du handler ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("echec d'installation du handler SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
