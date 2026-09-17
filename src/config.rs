use anyhow::{Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;

/// Une règle de routage : tout chemin commençant par `path_prefix`
/// est redirigé vers `backend`.
#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub path_prefix: String,
    pub backend: String,
}

fn default_request_timeout_ms() -> u64 {
    5_000
}

fn default_max_body_bytes() -> u64 {
    10 * 1024 * 1024 // 10 Mio
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub routes: Vec<Route>,
    /// Chemins vers des modules .wasm ou .wat appliqués à toutes les requêtes,
    /// dans l'ordre déclaré. Optionnel : un proxy sans entrée `plugins` dans
    /// le YAML fonctionne exactement comme en Phase 1.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Délai max d'attente d'une réponse backend avant de renvoyer 504.
    /// Protège contre un backend qui ne répond jamais (fuite de connexions,
    /// tâches bloquées indéfiniment).
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Taille max acceptée pour le corps d'une requête ou d'une réponse.
    /// Comme tout est actuellement collecté en mémoire (voir `proxy.rs`),
    /// c'est aussi une protection basique contre l'épuisement mémoire par
    /// des corps volumineux (accidentels ou malveillants).
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("impossible de lire le fichier de config '{path}'"))?;
        let config: Config = serde_yaml::from_str(&raw)
            .with_context(|| format!("YAML invalide dans '{path}'"))?;

        if config.routes.is_empty() {
            anyhow::bail!("la configuration doit contenir au moins une route");
        }

        Ok(config)
    }

    /// Trouve la première route dont le préfixe correspond au chemin demandé.
    /// L'ordre du fichier YAML fait foi (premier match gagne) — c'est volontairement
    /// simple pour l'instant ; on pourra trier par longueur de préfixe plus tard.
    pub fn match_route(&self, path: &str) -> Option<&Route> {
        self.routes.iter().find(|r| path.starts_with(&r.path_prefix))
    }
}
