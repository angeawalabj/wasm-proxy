# wasm-proxy

[![CI](https://github.com/angeawalabj/wasm-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/angeawalabj/wasm-proxy/actions/workflows/ci.yml)

Reverse proxy HTTP en Rust (Tokio + Hyper), avec un système de plugins
WebAssembly pour filtrer les requêtes et les réponses.

## Fonctionnalités

- Serveur HTTP async, routage par préfixe de chemin (config YAML)
- Forwarding transparent (headers, méthode, corps) vers le backend correspondant
- Plugins WASM appliqués sur la requête et/ou la réponse, via un ABI JSON
- Hot-reload de la config et des plugins, sans redémarrage
- Timeout backend, limite de taille de corps, endpoint de métriques Prometheus,
  arrêt propre sur SIGTERM
- Gestion d'erreurs propre (404 si aucune route, 502 si backend injoignable)
- Logs structurés via `tracing`

## Runtime WASM : wasmi

Le proxy utilise [`wasmi`](https://github.com/wasmi-labs/wasmi) (interpréteur
WASM pur Rust) plutôt que `wasmtime`. `wasmtime` reste le choix recommandé en
production pour sa compilation JIT, nettement plus rapide qu'un interpréteur ;
ici le compilateur Rust disponible est en 1.75, trop ancien pour les
dernières versions de `wasmtime` (dépendances en édition 2024). La logique
d'ABI et l'intégration dans `proxy.rs` ne changeraient pas avec `wasmtime` :
seul `plugin.rs` verrait son API remplacée (`wasmi::Store/Linker/Instance` →
leurs équivalents `wasmtime`).

## ABI des plugins

### v2 (utilisée aujourd'hui)

- export de la mémoire linéaire sous le nom `memory`
- `alloc(len: i32) -> i32`
- `filter_request(ptr: i32, len: i32) -> i64`
  - **Entrée** : JSON écrit par l'hôte dans la mémoire du plugin :
    ```json
    {"method":"GET","path":"/admin","headers":{"x-api-key":"secret123"}}
    ```
  - **Sortie** : un i64 empaqueté `(out_ptr << 32) | out_len`, pointant vers
    du JSON écrit par le plugin lui-même :
    ```json
    {"action":"continue","add_headers":{"x-plugin-checked":"admin_guard"}}
    {"action":"block","status":403,"body":"missing or invalid api key"}
    ```
  - Empaqueter pointeur et longueur dans un seul i64 évite d'exporter une
    fonction séparée pour récupérer la longueur — une seule valeur de retour
    suffit, wasmi/wasmtime supportent nativement i64.
- `filter_response(ptr: i32, len: i32) -> i64` : même mécanique, côté réponse
  (voir plus bas)

Les deux hooks sont indépendants et optionnels : un plugin peut n'exporter
que l'un des deux (ex. `admin_guard` n'exporte que `filter_request`,
`error_masker` que `filter_response`).

Côté hôte (`plugin.rs`), `PluginRequestContext` (serde `Serialize`) et
`PluginDecision` (serde `Deserialize`, enum taguée sur `"action"`)
encapsulent l'échange — pas de JSON manipulé à la main côté Rust.

### v1 (historique)

Conservée en référence dans `Plugin::filter_request_v1` : `filter_request(ptr,
len) -> i32` sur le chemin brut en UTF-8, retourne 0 (laisser passer) ou 1+
(bloquer, 403 générique). Abandonnée parce qu'elle ne permettait pas
d'inspecter les headers, la méthode, ni de personnaliser la réponse de
blocage.

## Plugin de démonstration : admin_guard

Règle : `/admin` nécessite le header `x-api-key: secret123`. Absent ou
incorrect → bloqué (403). Correct → laissé passer, avec
`x-plugin-checked: admin_guard` ajouté à la réponse (démontre `add_headers`).
Tout autre chemin → toujours `continue`.

Deux implémentations :

1. **`admin_guard.c`** : C freestanding (sans libc), compilé avec `clang`
   ciblant `wasm32-unknown-unknown` :
   ```bash
   clang-18 --target=wasm32-unknown-unknown -nostdlib -ffreestanding \
     -fno-builtin -Wl,--no-entry -Wl,--export=alloc -Wl,--export=filter_request \
     -O2 -o admin_guard.wasm admin_guard.c
   ```
2. **`example_plugin_v2.rs`** : équivalent en Rust `no_std` :
   ```bash
   rustup target add wasm32-unknown-unknown
   rustc --target wasm32-unknown-unknown -O --crate-type=cdylib \
         -o admin_guard_rust.wasm example_plugin_v2.rs
   ```
   Même logique (parsing JSON minimal à la main, pas de crate externe), avec
   les bounds-checks et la sûreté mémoire de Rust — recommandé pour un vrai
   plugin plutôt que le C.

### `src/bin/test_plugin.rs`

Harnais de test qui charge un `.wasm` et appelle son `filter_request`, sans
passer par le serveur HTTP complet :

```bash
cargo run --bin test_plugin -- admin_guard.wasm v2   # ABI v2 (JSON)
cargo run --bin test_plugin -- admin_blocker.wasm    # ABI v1 (chemin brut)
```

### `admin_blocker.wasm` / `build_admin_blocker.py`

Premier plugin écrit, ABI v1, généré octet par octet en Python (sections WASM
+ LEB128) avant de compiler directement du C vers wasm32 avec clang. Gardé
comme référence — utile pour comprendre ce que fait un vrai toolchain wasm
sous le capot.

## filter_response : error_masker

Hook symétrique de `filter_request`, et optionnel. `error_masker` n'exporte
que `filter_response` : il masque toute réponse backend avec un statut ≥ 500
(remplacée par un 502 générique + message neutre), pour éviter de divulguer
des détails internes (stack traces, versions de framework) au client.

Contexte envoyé au plugin pour ce hook :

```json
{"status": 503, "headers": {"content-type": "text/plain"}}
```

**Limite assumée** : le corps de la réponse n'est pas transmis au plugin
(coût mémoire/CPU non négligeable pour de gros corps, complexité d'encodage
binaire/texte). Un plugin `filter_response` peut donc changer le statut,
ajouter des headers, ou remplacer entièrement la réponse — mais pas inspecter
ou réécrire le corps existant. Une v3 pourrait lever cette limite derrière un
flag explicite.

Deux implémentations, comme pour `admin_guard` : **`error_masker.c`** (C
freestanding) et **`example_plugin_response.rs`** (Rust `no_std`).

## Hot-reload

Le proxy surveille (`notify`) le répertoire contenant `config.yaml` et
recharge **config + plugins ensemble** (jamais l'un sans l'autre, pour éviter
un état incohérent) dès qu'un fichier change dans ce répertoire.

- **Debounce grossier** (`src/hot_reload.rs`) : après un premier événement, on
  attend 200ms d'accalmie avant de recharger, pour absorber les rafales
  qu'un même enregistrement de fichier déclenche souvent.
- **Échec = pas de coupure** : si le nouveau YAML est invalide ou qu'un
  plugin ne charge pas, l'ancien snapshot reste actif et un warning est
  loggé — jamais de crash sur une erreur de config pendant que le proxy
  tourne.
- **`ArcSwap<ProxySnapshot>`** plutôt qu'un `Mutex`/`RwLock` : chaque requête
  charge un instantané cohérent de `(config, plugins)` en une seule
  opération lock-free au début de son traitement. Une requête en cours ne
  voit jamais un mélange ancien/nouveau, et le hot-reload n'ajoute aucune
  contention sur le chemin chaud.

Modifier `config.yaml` pendant que le proxy tourne (ajouter/retirer un
plugin, changer une route) prend effet en ~200-500ms, sans redémarrage, sans
requête perdue.

## Durcissement production

**Timeout backend** — `request_timeout_ms` (défaut 5000) enveloppe l'appel au
backend dans `tokio::time::timeout`. Un backend qui ne répond jamais renvoie
504 côté client plutôt que de faire fuir une connexion et une tâche
indéfiniment.

**Limite de taille de corps** — `max_body_bytes` (défaut 10 Mio), vérifiée à
trois moments : avant de collecter la requête entrante via `Content-Length`
quand il est présent (413), avant de collecter la réponse backend (502), puis
après collecte en filet de sécurité pour le cas `chunked` où la taille n'est
connue qu'une fois le corps entièrement lu. Le corps reste de toute façon
chargé intégralement en mémoire (`body.collect()`) : cette limite protège
contre les cas les plus flagrants, mais un vrai streaming avec rejet à la
volée serait nécessaire pour une protection complète contre des corps
chunked volumineux.

**Métriques** — `GET /__metrics` expose des compteurs simples au format texte
Prometheus (`AtomicU64`, pas de crate externe) : requêtes totales, bloquées
par un plugin, erreurs backend, timeouts backend. Court-circuite le routage
normal.

**Arrêt propre** — sur `SIGTERM` ou `Ctrl+C` : le proxy arrête d'accepter de
nouvelles connexions, les connexions en cours continuent jusqu'à leur
terminaison naturelle (suivies via un `tokio::task::JoinSet`), puis abandon
forcé après 10s (`SHUTDOWN_GRACE_PERIOD`) si des connexions traînent encore.

## Lancer le proxy

```bash
cargo build
./target/debug/wasm-proxy config.yaml
```

`config.yaml` :

```yaml
listen_addr: "127.0.0.1:8080"
request_timeout_ms: 5000   # optionnel, defaut 5000
max_body_bytes: 10485760   # optionnel, defaut 10 Mio
routes:
  - path_prefix: "/api"
    backend: "http://127.0.0.1:9001"
  - path_prefix: "/"
    backend: "http://127.0.0.1:9002"
plugins:
  - "admin_guard.wasm"
  - "error_masker.wasm"
```

## Tests

```bash
cargo test
```

Tests d'intégration (`tests/proxy_integration.rs`) : proxy réel + backend(s)
réels sur des ports éphémères, requêtes envoyées par un vrai client HTTP.
Couvrent :

- `/admin` sans header ou avec mauvaise clé → 403 ; avec la bonne clé → passe,
  header `x-plugin-checked: admin_guard` ajouté
- Backend renvoyant 503 avec stack trace → masqué en 502 générique par
  `error_masker`, indépendamment de `filter_request`
- `/api/*` et `/*` → forward normal, jamais affectés par les plugins ci-dessus
- Backend injoignable → 502 ; aucune route ne matche → 404
- Backend qui dort 500ms avec `request_timeout_ms: 100` → 504 en moins de
  400ms
- Corps de requête dépassant `max_body_bytes` → 413

Exécutés en CI à chaque push (`.github/workflows/ci.yml`) avec `cargo test`
et `cargo clippy`.

**Vérifié manuellement, pas encore automatisé :**

- Arrêt propre : requête de 3s en vol, `SIGTERM` envoyé à 0.5s → la requête
  se termine avec son 200, puis le process quitte
- Hot-reload : ajout/retrait d'un plugin ou changement de route pris en
  compte en ~200-500ms, sans requête perdue, sans crash si le nouveau YAML
  est invalide

## Pistes d'amélioration

1. Passer le corps au `filter_response` (ABI v3), derrière un flag explicite
   pour ne payer le coût de copie que quand un plugin le demande
2. Pooling d'instances si le coût d'instanciation par requête devient un
   goulot d'étranglement mesuré (actuellement : une instance wasm neuve à
   chaque appel de plugin, par simplicité et isolation)
3. Migration vers `wasmtime` pour la compilation JIT
4. Tester le hot-reload et l'arrêt propre sur SIGTERM en automatisé
