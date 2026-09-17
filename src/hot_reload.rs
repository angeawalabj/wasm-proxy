//! Hot-reload : surveille le répertoire contenant la config (et donc, en
//! pratique, les fichiers `.wasm` des plugins qui vivent à côté) et
//! recharge l'ensemble (config + plugins) à chaque changement détecté.
//!
//! Approche volontairement grossière : n'importe quel événement dans le
//! répertoire déclenche une tentative de rechargement complet, plutôt que
//! d'essayer de distinguer précisément "seul tel plugin a changé". Plus
//! simple, et le coût d'un rechargement complet (quelques lecteurs de
//! fichiers + réinstanciation wasm) est négligeable face à la fréquence
//! réelle des changements de config en production.
//!
//! Si le rechargement échoue (YAML invalide, plugin cassé), l'ancien
//! snapshot reste actif et le proxy continue de fonctionner normalement :
//! une erreur de config ne doit jamais faire tomber un serveur qui tourne.

use crate::config::Config;
use crate::plugin;
use crate::proxy::{ProxySnapshot, ProxyState};
use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use tracing::{info, warn};

/// Démarre la surveillance en arrière-plan. Le `RecommendedWatcher` retourné
/// doit être conservé vivant par l'appelant (ex: `let _watcher = ...`) —
/// s'il est droppé, la surveillance s'arrête silencieusement.
pub fn spawn_watcher(config_path: String, state: ProxyState) -> Result<RecommendedWatcher> {
    let watch_dir = Path::new(&config_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res| {
        // On ignore les erreurs d'envoi : si le récepteur est fermé, le
        // thread de rechargement s'est arrêté (ex: pendant les tests), rien
        // à faire de plus côté callback du watcher.
        let _ = tx.send(res);
    })
    .context("échec de création du watcher de fichiers")?;

    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("échec de surveillance de '{}'", watch_dir.display()))?;

    info!(dir = %watch_dir.display(), "surveillance de la config activée (hot-reload)");

    std::thread::spawn(move || {
        watch_loop(rx, config_path, state);
    });

    Ok(watcher)
}

fn watch_loop(rx: mpsc::Receiver<notify::Result<notify::Event>>, config_path: String, state: ProxyState) {
    loop {
        // Bloque jusqu'au premier événement.
        match rx.recv() {
            Ok(Ok(_event)) => {}
            Ok(Err(e)) => {
                warn!(error = %e, "erreur du watcher de fichiers");
                continue;
            }
            Err(_) => {
                // Émetteur fermé : le watcher a été droppé, on arrête ce thread.
                return;
            }
        }

        // Debounce grossier : un enregistrement déclenche souvent plusieurs
        // événements rapprochés (create + modify, plusieurs writes...). On
        // attend une accalmie de 200ms avant de recharger, en absorbant
        // tous les événements qui arrivent entre-temps.
        while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}

        reload(&config_path, &state);
    }
}

fn reload(config_path: &str, state: &ProxyState) {
    let new_config = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "rechargement de la config échoué, config précédente conservée");
            return;
        }
    };

    let new_plugins = match plugin::load_plugins(&new_config.plugins) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "rechargement des plugins échoué, config précédente conservée");
            return;
        }
    };

    let routes = new_config.routes.len();
    let plugins = new_plugins.len();

    state.replace_snapshot(ProxySnapshot {
        config: new_config,
        plugins: new_plugins,
    });

    info!(routes, plugins, "config et plugins rechargés à chaud");
}
