# wasm-proxy — Phase 1 + Phase 2 (ABI v1 et v2)

## Phase 1 : reverse proxy basique
- Serveur HTTP async (Tokio + Hyper 1.x + hyper-util)
- Routage par préfixe de chemin, config chargée depuis un fichier YAML
- Forwarding transparent (headers, méthode, corps) vers le backend correspondant
- Gestion d'erreurs propre (404 si aucune route, 502 si backend injoignable)
- Logs structurés via `tracing`

## Phase 2 : extensibilité par plugins WASM

### Runtime : wasmi, pas wasmtime
Ce sandbox de développement a un toolchain Rust ancien (1.75, installé via
apt, sans `rustup`). Les dernières versions de `wasmtime` nécessitent un
compilateur plus récent (dépendances en edition 2024). J'ai donc utilisé
`wasmi` (interpréteur WASM pur Rust, version 0.31, la dernière compatible
avec rustc 1.75).

**Pour votre usage réel**, avec un poste de dev classique (rustup à jour),
`wasmtime` reste le choix recommandé en production : compilation JIT native,
bien plus rapide que l'interprétation. La logique d'ABI et le code
d'intégration dans `proxy.rs` restent quasiment identiques ; seul `plugin.rs`
changerait d'API (`wasmi::Store/Linker/Instance` → leurs équivalents
`wasmtime`).

### ABI v1 (historique — gardée en référence dans `Plugin::filter_request_v1`)
- `alloc(len: i32) -> i32`
- `filter_request(ptr: i32, len: i32) -> i32` : lit le chemin brut en UTF-8,
  retourne 0 (laisser passer) ou 1+ (bloquer, 403 générique)

Limite qui a motivé la v2 : impossible d'inspecter les headers, la méthode,
ou de personnaliser la réponse de blocage.

### ABI v2 (celle utilisée aujourd'hui par le proxy)
- export de la mémoire linéaire sous le nom `memory`
- `alloc(len: i32) -> i32`
- `filter_request(ptr: i32, len: i32) -> i64`
  - **Entrée** : JSON écrit par l'hôte dans la mémoire du plugin :
    ```json
    {"method":"GET","path":"/admin","headers":{"x-api-key":"secret123"}}
    ```
  - **Sortie** : un i64 empaqueté `(out_ptr << 32) | out_len`, pointant vers
    du JSON écrit par le plugin lui-même dans sa propre mémoire :
    ```json
    {"action":"continue","add_headers":{"x-plugin-checked":"admin_guard"}}
    {"action":"block","status":403,"body":"missing or invalid api key"}
    ```
  - Empaqueter le pointeur et la longueur dans un seul i64 évite d'avoir à
    exporter une fonction séparée pour récupérer la longueur : une seule
    valeur de retour suffit, wasmi/wasmtime supportent nativement i64.

Côté hôte (`plugin.rs`), `PluginRequestContext` (serde `Serialize`) et
`PluginDecision` (serde `Deserialize`, enum taguée sur le champ `"action"`)
encapsulent cet échange proprement — pas de JSON manipulé à la main côté Rust.

### Le plugin de démo : `admin_guard`
Règle : `/admin` nécessite le header `x-api-key: secret123`.
- Absent ou incorrect → bloqué, JSON `{"action":"block","status":403,...}`
- Correct → laissé passer, avec le header `x-plugin-checked: admin_guard`
  ajouté à la réponse (démontre `add_headers`)
- Tout autre chemin → toujours `{"action":"continue"}`

Deux implémentations, au choix :

1. **`admin_guard.c`** : C freestanding (sans libc), compilé directement
   dans ce sandbox avec `clang` ciblant `wasm32-unknown-unknown` — cela a
   fonctionné là où le toolchain Rust wasm32 manquait :
   ```bash
   clang-18 --target=wasm32-unknown-unknown -nostdlib -ffreestanding \
     -fno-builtin -Wl,--no-entry -Wl,--export=alloc -Wl,--export=filter_request \
     -O2 -o admin_guard.wasm admin_guard.c
   ```
2. **`example_plugin_v2.rs`** : l'équivalent en Rust `no_std`, à compiler
   chez vous avec `rustup target add wasm32-unknown-unknown` :
   ```bash
   rustc --target wasm32-unknown-unknown -O --crate-type=cdylib \
         -o admin_guard_rust.wasm example_plugin_v2.rs
   ```
   Même logique (parsing JSON minimal à la main, pas de crate externe), mais
   avec les bounds-checks et la sûreté mémoire de Rust — recommandé pour un
   vrai plugin plutôt que le C.

### `src/bin/test_plugin.rs`
Harnais de test qui charge un `.wasm` et appelle son `filter_request`,
sans passer par le serveur HTTP complet :
```bash
cargo run --bin test_plugin -- admin_guard.wasm v2   # ABI v2 (JSON)
cargo run --bin test_plugin -- admin_blocker.wasm    # ABI v1 (chemin brut)
```

### `admin_blocker.wasm` / `build_admin_blocker.py`
Le tout premier plugin, ABI v1, écrit octet par octet en Python (sections
WASM + LEB128) avant qu'on découvre que `clang` pouvait compiler du C vers
wasm32 dans ce sandbox. Gardé comme référence — c'est un bon exercice pour
comprendre ce que fait un vrai toolchain wasm sous le capot.

### `filter_response` : symétrique de `filter_request`, et optionnel
Un plugin peut n'implémenter que l'un des deux hooks. Démontré ici avec
**`error_masker`** : n'exporte que `filter_response`, masque toute réponse
backend avec un statut ≥ 500 (remplacée par un 502 générique + message
neutre), pour éviter de divulguer des détails internes (stack traces,
versions de framework) au client. `admin_guard`, à l'inverse, n'exporte que
`filter_request`.

Le contexte envoyé au plugin pour ce hook :
```json
{"status": 503, "headers": {"content-type": "text/plain"}}
```
**Limite assumée** : le corps de la réponse n'est pas transmis au plugin
(coût mémoire/CPU non négligeable pour de gros corps, complexité
d'encodage binaire/texte). Un plugin `filter_response` peut donc changer le
statut, ajouter des headers, ou remplacer entièrement la réponse — mais pas
inspecter ou réécrire le corps existant. Voir le commentaire en tête de
`plugin.rs` pour l'idée d'une v3 qui lèverait cette limite à la demande.

Deux implémentations, comme pour `admin_guard` :
- **`error_masker.c`** (C freestanding, compilé avec `clang-18` dans ce
  sandbox)
- **`example_plugin_response.rs`** (Rust `no_std`, à compiler chez vous)

### Hot-reload (`notify`)
Le proxy surveille le répertoire contenant `config.yaml` et recharge
**config + plugins ensemble** (jamais l'un sans l'autre, pour éviter un état
incohérent) dès qu'un fichier change dans ce répertoire — que ce soit
`config.yaml` lui-même ou un `.wasm` de plugin recompilé.

Points clés de l'implémentation (`src/hot_reload.rs`) :
- **Debounce grossier** : après un premier événement, on attend 200ms
  d'accalmie avant de recharger, pour absorber les rafales d'événements
  qu'un même enregistrement de fichier déclenche souvent.
- **Échec = pas de coupure** : si le nouveau YAML est invalide ou qu'un
  plugin ne charge pas, l'ancien snapshot reste actif et un warning est
  loggé — jamais de crash sur une erreur de config pendant que le proxy
  tourne.
- **`ArcSwap<ProxySnapshot>`** (plutôt qu'un `Mutex`/`RwLock`) : chaque
  requête charge un instantané cohérent de `(config, plugins)` en une seule
  opération lock-free au tout début de son traitement. Une requête en cours
  ne "voit" jamais un mélange ancien/nouveau, et le hot-reload n'ajoute
  aucune contention sur le chemin chaud.

Testé : modifier `config.yaml` pendant que le proxy tourne (ajouter/retirer
un plugin, changer une route) prend effet en ~200-500ms, sans redémarrage,
sans requête perdue.



## Durcissement production

Quatre ajouts, tous testés en conditions réelles dans ce sandbox :

### Timeout backend
`request_timeout_ms` (défaut 5000) enveloppe l'appel au backend dans
`tokio::time::timeout`. Un backend qui ne répond jamais renvoie 504 côté
client plutôt que de faire fuir une connexion et une tâche indéfiniment.
Testé : backend qui dort 3s avec `request_timeout_ms: 1000` → 504 en
~1.0s pile.

### Limite de taille de corps
`max_body_bytes` (défaut 10 Mio) est vérifié à trois moments :
1. **Avant** de collecter la requête entrante, via l'en-tête
   `Content-Length` quand il est présent (rejet rapide, 413)
2. **Avant** de collecter la réponse backend, même logique (502)
3. **Après** collecte, en filet de sécurité pour le cas `chunked` où la
   taille n'est connue qu'une fois le corps entièrement lu

Limite assumée : le corps est de toute façon entièrement chargé en mémoire
(`body.collect()`) — cette limite protège contre les cas les plus flagrants
mais un vrai streaming avec rejet à la volée serait nécessaire pour une
protection complète contre l'épuisement mémoire par des corps chunked
volumineux. Notée comme piste d'amélioration.

### Endpoint de métriques
`GET /__metrics` expose des compteurs simples au format texte Prometheus
(`AtomicU64`, pas de crate externe) : requêtes totales, bloquées par un
plugin, erreurs backend, timeouts backend. Court-circuite le routage
normal — ce n'est pas une requête à proxifier.

### Arrêt propre (graceful shutdown)
Sur `SIGTERM` (le signal standard envoyé par `docker stop`, `systemctl
stop`, ou un pod Kubernetes en cours de terminaison) ou `Ctrl+C` :
1. Le proxy arrête d'accepter de **nouvelles** connexions
2. Les connexions **en cours** continuent jusqu'à leur terminaison
   naturelle (suivies via un `tokio::task::JoinSet`)
3. Après 10s (`SHUTDOWN_GRACE_PERIOD`), abandon forcé si des connexions
   traînent encore, avec un warning explicite

Testé : requête de 3s en vol, `SIGTERM` envoyé à 0.5s → la requête se
termine bien avec son 200 après ses 3 secondes complètes, *puis* le
process quitte. Aucune requête interrompue en plein vol.

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

## Testé de bout en bout
- `/admin` sans header → 403, corps `"missing or invalid api key"`
- `/admin` avec mauvaise clé → 403
- `/admin` avec `x-api-key: secret123` → passe, réponse avec header
  `x-plugin-checked: admin_guard` ajouté
- backend renvoyant 503 avec stack trace → masqué en 502 générique par
  `error_masker` (hook `filter_response`, indépendant de `filter_request`)
- `/api/*` et `/*` → forward normal, jamais affectés par les plugins
  ci-dessus
- Backend injoignable → 502
- Aucune route ne matche → 404
- Hot-reload : ajout/retrait d'un plugin ou changement de route pris en
  compte en ~200-500ms sans redémarrer le process, sans requête perdue, et
  sans crash si le nouveau YAML est invalide

## Prochaines étapes possibles
1. **Passer le corps au `filter_response`** (ABI v3), derrière un flag
   explicite pour ne payer le coût de copie que quand un plugin le demande
2. **Pooling d'instances** si le coût d'instanciation par requête devient
   un goulot d'étranglement mesuré (actuellement : une instance wasm neuve
   à chaque appel de plugin, par simplicité et isolation)
3. **Migration vers wasmtime** une fois hors de ce sandbox, pour la
   compilation JIT (bien plus rapide que l'interprétation `wasmi`)
4. **Durcissement production** : timeouts sur les requêtes backend, arrêt
   propre (graceful shutdown) sur SIGTERM, limites de taille de corps,
   métriques exposées (Prometheus), tests d'intégration automatisés
