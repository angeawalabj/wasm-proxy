//! Métriques minimalistes : quelques compteurs atomiques, exposés en texte
//! au format Prometheus sur `GET /__metrics`. Pas de crate externe
//! (`prometheus-client` ou `metrics`) : pour ce volume de compteurs, des
//! `AtomicU64` suffisent et évitent une dépendance de plus. À réévaluer si
//! le nombre de métriques croît significativement (histogrammes de latence,
//! labels dynamiques par route...).

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_blocked_by_plugin: AtomicU64,
    pub backend_errors_total: AtomicU64,
    pub backend_timeouts_total: AtomicU64,
}

impl Metrics {
    pub fn render_prometheus(&self) -> String {
        format!(
            "# HELP wasm_proxy_requests_total Nombre total de requêtes reçues\n\
             # TYPE wasm_proxy_requests_total counter\n\
             wasm_proxy_requests_total {}\n\
             # HELP wasm_proxy_requests_blocked_by_plugin_total Requêtes bloquées par un plugin\n\
             # TYPE wasm_proxy_requests_blocked_by_plugin_total counter\n\
             wasm_proxy_requests_blocked_by_plugin_total {}\n\
             # HELP wasm_proxy_backend_errors_total Échecs de communication avec un backend\n\
             # TYPE wasm_proxy_backend_errors_total counter\n\
             wasm_proxy_backend_errors_total {}\n\
             # HELP wasm_proxy_backend_timeouts_total Requêtes backend ayant dépassé le timeout\n\
             # TYPE wasm_proxy_backend_timeouts_total counter\n\
             wasm_proxy_backend_timeouts_total {}\n",
            self.requests_total.load(Ordering::Relaxed),
            self.requests_blocked_by_plugin.load(Ordering::Relaxed),
            self.backend_errors_total.load(Ordering::Relaxed),
            self.backend_timeouts_total.load(Ordering::Relaxed),
        )
    }
}
