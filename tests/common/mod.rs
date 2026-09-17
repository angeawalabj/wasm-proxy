//! Aides partagées entre les tests d'intégration : démarrer un proxy réel
//! sur un port éphémère, démarrer un backend HTTP minimal dont le
//! comportement est fourni par le test, et envoyer une requête au proxy
//! sans dépendance externe (on réutilise `hyper-util`, déjà utilisé par le
//! proxy lui-même).

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use wasm_proxy::config::{Config, Route};
use wasm_proxy::plugin::Plugin;
use wasm_proxy::proxy::{self, ProxyState};

/// Chemin absolu vers un fichier `.wasm` du dépôt (déjà compilé, committé
/// tel quel — pas besoin de toolchain wasm32 pour faire tourner les tests).
pub fn wasm_fixture(name: &str) -> String {
    format!("{}/{}", env!("CARGO_MANIFEST_DIR"), name)
}

pub fn config_with_routes(routes: Vec<Route>) -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        routes,
        plugins: Vec::new(),
        request_timeout_ms: 5_000,
        max_body_bytes: 10 * 1024 * 1024,
    }
}

/// Démarre le proxy sur un port éphémère et retourne son adresse. La boucle
/// d'accept tourne en tâche de fond jusqu'à la fin du process de test — pas
/// de mécanisme d'arrêt propre ici, inutile pour un test.
pub async fn spawn_proxy(config: Config, plugin_paths: &[&str]) -> SocketAddr {
    let plugins: Vec<Plugin> = plugin_paths
        .iter()
        .map(|p| Plugin::load(p).expect("chargement du plugin de test"))
        .collect();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ProxyState::new(config, plugins);

    tokio::spawn(async move {
        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let io = TokioIo::new(stream);
            let state = state.clone();
            tokio::spawn(async move {
                proxy::serve_connection(io, peer_addr, state).await;
            });
        }
    });

    addr
}

/// Démarre un backend HTTP minimal dont chaque réponse est calculée par
/// `handler`. Sert à simuler les différents comportements backend (succès,
/// erreur 5xx, lenteur, etc.) sans dépendre d'un vrai service externe.
pub async fn spawn_backend<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _peer_addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let io = TokioIo::new(stream);
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(req).await) }
                });
                let _ = ConnBuilder::new(TokioExecutor::new())
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    addr
}

/// Réserve un port TCP puis ferme immédiatement le listener : personne
/// n'écoute dessus, ce qui simule un backend injoignable.
pub async fn unreachable_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Bytes,
) -> TestResponse {
    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build(HttpConnector::new());

    let mut builder = Request::builder().method(method).uri(format!("http://{addr}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = builder.body(Full::new(body)).unwrap();

    let resp = client.request(req).await.expect("requête vers le proxy échouée");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();

    TestResponse { status, headers, body }
}

pub async fn get(addr: SocketAddr, path: &str) -> TestResponse {
    request(addr, "GET", path, &[], Bytes::new()).await
}
