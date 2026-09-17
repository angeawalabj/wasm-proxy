use crate::config::Config;
use crate::metrics::Metrics;
use crate::plugin::{Plugin, PluginDecision, PluginRequestContext, PluginResponseContext};
use arc_swap::ArcSwap;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{HeaderMap, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub type ProxyBody = Full<Bytes>;
type HttpClient = Client<HttpConnector, Incoming>;

/// Ce qui est rechargé atomiquement au hot-reload : config et plugins vont
/// toujours de pair (un plugin fait référence à un chemin déclaré dans la
/// config), donc on les regroupe pour ne jamais avoir un état incohérent
/// où l'un aurait été rechargé sans l'autre.
pub struct ProxySnapshot {
    pub config: Config,
    pub plugins: Vec<Plugin>,
}

/// État partagé injecté dans chaque requête : un instantané (config +
/// plugins) remplaçable atomiquement via `ArcSwap`, le client HTTP
/// réutilisable (pooling de connexions géré par hyper-util, non concerné
/// par le hot-reload), et les compteurs de métriques.
#[derive(Clone)]
pub struct ProxyState {
    pub snapshot: Arc<ArcSwap<ProxySnapshot>>,
    pub client: HttpClient,
    pub metrics: Arc<Metrics>,
}

impl ProxyState {
    pub fn new(config: Config, plugins: Vec<Plugin>) -> Self {
        let client: HttpClient = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
        let snapshot = ProxySnapshot { config, plugins };
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            client,
            metrics: Arc::new(Metrics::default()),
        }
    }

    /// Remplace atomiquement la config et les plugins actifs. Les requêtes
    /// en cours de traitement continuent avec l'ancien `Arc` (chargé au
    /// début de `handle_request`) jusqu'à leur fin ; seules les nouvelles
    /// requêtes voient le nouvel état. Pas de coupure de service.
    pub fn replace_snapshot(&self, new_snapshot: ProxySnapshot) {
        self.snapshot.store(Arc::new(new_snapshot));
    }
}

/// Point d'entrée appelé pour chaque requête entrante.
pub async fn handle_request(
    state: ProxyState,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, hyper::Error> {
    state.metrics.requests_total.fetch_add(1, Ordering::Relaxed);

    let path = req.uri().path().to_string();

    // Endpoint de métriques : court-circuite tout le reste (routage,
    // plugins) puisque ce n'est pas une requête à proxifier.
    if path == "/__metrics" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; version=0.0.4")
            .body(Full::new(Bytes::from(state.metrics.render_prometheus())))
            .expect("réponse métriques infaillible"));
    }

    // On charge un instantané cohérent une fois pour toute la durée de la
    // requête : même si un hot-reload survient pendant le traitement, cette
    // requête reste sur la version de config/plugins avec laquelle elle a
    // commencé (pas de mélange ancien/nouveau en cours de route).
    let snapshot = state.snapshot.load();
    let max_body = snapshot.config.max_body_bytes;

    let route = match snapshot.config.match_route(&path) {
        Some(r) => r.clone(),
        None => {
            warn!(%path, "aucune route ne correspond");
            return Ok(text_response(StatusCode::NOT_FOUND, "no matching route"));
        }
    };

    // Vérification précoce via Content-Length quand il est présent : évite
    // de commencer à collecter un corps qu'on sait déjà trop volumineux.
    // N'attrape pas les corps en chunked-encoding sans Content-Length —
    // voir la vérification a posteriori plus bas pour ce cas.
    if let Some(len) = content_length(req.headers()) {
        if len > max_body {
            warn!(%path, content_length = len, max_body, "corps de requête trop volumineux (Content-Length)");
            return Ok(text_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"));
        }
    }

    // Chaîne de plugins : le premier qui demande un blocage arrête tout.
    // Chaque plugin tourne dans son propre Store wasmi (isolation totale,
    // pas d'état partagé entre plugins ni entre requêtes).
    let mut extra_response_headers: HashMap<String, String> = HashMap::new();

    for plugin in snapshot.plugins.iter() {
        let ctx = PluginRequestContext {
            method: req.method().as_str(),
            path: &path,
            headers: headers_to_map(req.headers()),
        };

        match plugin.filter_request(&ctx) {
            Ok(Some(PluginDecision::Block { status, body })) => {
                state.metrics.requests_blocked_by_plugin.fetch_add(1, Ordering::Relaxed);
                info!(%path, plugin = plugin.name(), status, "requête bloquée par un plugin");
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
                return Ok(text_response_owned(code, body));
            }
            Ok(Some(PluginDecision::Continue { add_headers })) => {
                extra_response_headers.extend(add_headers);
            }
            Ok(None) => {} // plugin n'implémente pas filter_request, on passe au suivant
            Err(e) => {
                warn!(error = %e, plugin = plugin.name(), "erreur d'exécution du plugin");
                return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "plugin error"));
            }
        }
    }

    // Reconstruit l'URI cible : backend + chemin + query d'origine
    let target_uri = match build_target_uri(&route.backend, &req) {
        Ok(uri) => uri,
        Err(e) => {
            warn!(error = %e, "échec de construction de l'URI cible");
            return Ok(text_response(StatusCode::BAD_GATEWAY, "invalid backend URI"));
        }
    };

    info!(%path, backend = %route.backend, "forwarding");

    // On reconstruit la requête sortante en changeant seulement l'URI.
    let (mut parts, body) = req.into_parts();
    parts.uri = target_uri;

    let outgoing = Request::from_parts(parts, body);
    let timeout = Duration::from_millis(snapshot.config.request_timeout_ms);

    let response_result = tokio::time::timeout(timeout, state.client.request(outgoing)).await;

    let resp = match response_result {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            state.metrics.backend_errors_total.fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, backend = %route.backend, "backend injoignable");
            return Ok(text_response(StatusCode::BAD_GATEWAY, "backend unreachable"));
        }
        Err(_elapsed) => {
            state.metrics.backend_timeouts_total.fetch_add(1, Ordering::Relaxed);
            warn!(backend = %route.backend, timeout_ms = snapshot.config.request_timeout_ms, "délai backend dépassé");
            return Ok(text_response(StatusCode::GATEWAY_TIMEOUT, "backend timeout"));
        }
    };

    let (mut parts, body) = resp.into_parts();

    if let Some(len) = content_length(&parts.headers) {
        if len > max_body {
            warn!(content_length = len, max_body, "corps de réponse trop volumineux (Content-Length)");
            return Ok(text_response(StatusCode::BAD_GATEWAY, "backend response too large"));
        }
    }

    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            warn!(error = %e, "échec de lecture de la réponse backend");
            return Ok(text_response(StatusCode::BAD_GATEWAY, "backend read error"));
        }
    };

    // Filet de sécurité pour le cas chunked-encoding (pas de Content-Length
    // connu à l'avance) : on a dû collecter le corps pour le savoir, mais on
    // refuse de le renvoyer au client s'il dépasse la limite.
    if collected.len() as u64 > max_body {
        warn!(actual_len = collected.len(), max_body, "corps de réponse trop volumineux (mesuré après collecte)");
        return Ok(text_response(StatusCode::BAD_GATEWAY, "backend response too large"));
    }

    // Deuxième passe de plugins, symétrique à la première, mais sur la
    // réponse cette fois. `filter_response` est optionnel : un plugin qui
    // ne l'exporte pas est simplement ignoré à cette étape.
    let response_headers = headers_to_map(&parts.headers);
    for plugin in snapshot.plugins.iter() {
        let ctx = PluginResponseContext {
            status: parts.status.as_u16(),
            headers: response_headers.clone(),
        };

        match plugin.filter_response(&ctx) {
            Ok(Some(PluginDecision::Block { status, body })) => {
                info!(
                    plugin = plugin.name(),
                    original_status = parts.status.as_u16(),
                    new_status = status,
                    "réponse remplacée par un plugin"
                );
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                return Ok(text_response_owned(code, body));
            }
            Ok(Some(PluginDecision::Continue { add_headers })) => {
                extra_response_headers.extend(add_headers);
            }
            Ok(None) => {} // plugin n'implémente pas filter_response
            Err(e) => {
                warn!(error = %e, plugin = plugin.name(), "erreur d'exécution du plugin (réponse)");
                return Ok(text_response(StatusCode::INTERNAL_SERVER_ERROR, "plugin error"));
            }
        }
    }

    for (key, value) in extra_response_headers {
        if let (Ok(name), Ok(val)) = (
            hyper::header::HeaderName::from_bytes(key.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            parts.headers.insert(name, val);
        } else {
            warn!(key, "header ajouté par un plugin ignoré (nom/valeur invalide)");
        }
    }
    Ok(Response::from_parts(parts, Full::new(collected)))
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(hyper::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect()
}

fn build_target_uri(backend: &str, req: &Request<Incoming>) -> anyhow::Result<Uri> {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let full = format!("{}{}", backend.trim_end_matches('/'), path_and_query);
    Ok(full.parse::<Uri>()?)
}

fn text_response(status: StatusCode, msg: &'static str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from_static(msg.as_bytes())))
        .expect("construction de réponse statique infaillible")
}

fn text_response_owned(status: StatusCode, msg: String) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(msg)))
        .expect("construction de réponse infaillible")
}
