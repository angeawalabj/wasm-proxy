//! Chargement et execution de plugins WebAssembly pour le proxy.
//!
//! ABI v1 (historique, gardee pour reference) :
//!   - `filter_request(ptr: i32, len: i32) -> i32` sur le chemin brut
//!   - voir `filter_request_v1` plus bas
//!
//! ABI v2 (celle utilisee par le proxy aujourd'hui), deux hooks symetriques
//! et tous deux optionnels : un plugin peut n'implementer que l'un des deux
//! (ex: un plugin "response-only" qui masque des erreurs backend sans jamais
//! inspecter la requete entrante) :
//!   - export de la memoire lineaire sous le nom `memory`
//!   - `alloc(len: i32) -> i32` : reserve `len` octets, retourne un pointeur
//!   - `filter_request(ptr: i32, len: i32) -> i64` (optionnel a l'usage,
//!         mais si absent le plugin ne fait jamais rien sur la requete)
//!         lit `len` octets de JSON a `ptr` :
//!           {"method":"GET","path":"/admin","headers":{"x-api-key":"..."}}
//!   - `filter_response(ptr: i32, len: i32) -> i64` (optionnel)
//!         lit `len` octets de JSON a `ptr` :
//!           {"status":200,"headers":{"content-type":"text/html"}}
//!         (le corps de la reponse n'est PAS transmis au plugin dans cette
//!         version : voir la note "Limite assumee" plus bas)
//!
//!   Les deux hooks retournent un i64 empaquete (out_ptr << 32) | out_len,
//!   pointant vers du JSON de decision ecrit par le plugin :
//!     {"action":"continue","add_headers":{...}}
//!     {"action":"block","status":403,"body":"..."}
//!   Cote reponse, "block" sert a *remplacer* la reponse (masquer une erreur
//!   backend, censurer un corps sensible) plutot qu'a "bloquer" au sens
//!   propre, mais reutiliser le meme type de decision evite de dupliquer
//!   toute la logique hote pour un besoin conceptuellement identique
//!   ("court-circuiter avec ce statut/corps" vs "laisser passer, avec ces
//!   headers en plus").
//!
//! Limite assumee : le corps de la reponse backend n'est pas envoye au
//! plugin. Le transmettre demanderait de copier potentiellement beaucoup de
//! donnees dans la memoire wasm a chaque requete (cout CPU/memoire non
//! negligeable pour de gros corps), et complexifie le contrat (encodage
//! binaire vs texte, gestion de la troncature). Pour l'instant les plugins
//! de reponse ne peuvent donc agir que sur le statut et les headers - c'est
//! deja suffisant pour des cas reels comme masquer les erreurs 5xx ou
//! ajouter des headers de securite. Une v3 pourrait ajouter le corps derriere
//! un flag explicite ("needs_body: true" declare par le plugin) pour ne
//! payer ce cout que quand c'est necessaire.
//!
//! Runtime : wasmi (interpreteur pur Rust). Choix pratique pour cet
//! environnement de demo (toolchain Rust ancien sans acces a rustup) ; en
//! production, wasmtime (JIT, bien plus rapide) est le choix standard et
//! partage exactement la meme logique d'ABI decrite ici.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use wasmi::{Engine, Extern, Instance, Linker, Module, Store};

/// Ce que l'hote envoie au plugin pour le hook `filter_request`.
#[derive(Debug, Serialize)]
pub struct PluginRequestContext<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: HashMap<String, String>,
}

/// Ce que l'hote envoie au plugin pour le hook `filter_response`.
#[derive(Debug, Serialize)]
pub struct PluginResponseContext {
    pub status: u16,
    pub headers: HashMap<String, String>,
}

/// Ce que le plugin renvoie a l'hote, pour l'un ou l'autre hook.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PluginDecision {
    Continue {
        #[serde(default)]
        add_headers: HashMap<String, String>,
    },
    Block {
        status: u16,
        #[serde(default)]
        body: String,
    },
}

pub struct Plugin {
    engine: Engine,
    module: Module,
    name: String,
}

impl Plugin {
    pub fn load(path: &str) -> Result<Self> {
        let engine = Engine::default();
        let bytes = std::fs::read(path)
            .with_context(|| format!("impossible de lire le plugin '{path}'"))?;

        let module = Module::new(&engine, &bytes[..])
            .with_context(|| format!("module wasm invalide '{path}'"))?;

        Ok(Self {
            engine,
            module,
            name: path.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn new_instance(&self) -> Result<(Store<()>, Instance)> {
        let mut store: Store<()> = Store::new(&self.engine, ());
        let linker = Linker::new(&self.engine);
        let instance = linker
            .instantiate(&mut store, &self.module)
            .with_context(|| format!("echec d'instanciation du plugin '{}'", self.name))?
            .start(&mut store)
            .with_context(|| format!("echec de demarrage du plugin '{}'", self.name))?;
        Ok((store, instance))
    }

    /// ABI v1 : gardee pour reference / retro-compatibilite eventuelle.
    #[allow(dead_code)]
    pub fn filter_request_v1(&self, path: &str) -> Result<bool> {
        let (mut store, instance) = self.new_instance()?;
        let memory = get_memory(&instance, &mut store, &self.name)?;

        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .with_context(|| format!("le plugin '{}' doit exporter 'alloc'", self.name))?;
        let filter = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "filter_request")
            .with_context(|| {
                format!("le plugin '{}' doit exporter 'filter_request(ptr, len) -> i32'", self.name)
            })?;

        let bytes = path.as_bytes();
        let ptr = alloc.call(&mut store, bytes.len() as i32)?;
        memory
            .write(&mut store, ptr as usize, bytes)
            .map_err(|e| anyhow::anyhow!("echec d'ecriture memoire plugin: {e:?}"))?;

        let result = filter.call(&mut store, (ptr, bytes.len() as i32))?;
        Ok(result != 0)
    }

    /// Hook requete (ABI v2). `Ok(None)` si le plugin n'exporte pas
    /// `filter_request` (un plugin peut n'implementer que `filter_response`,
    /// par exemple pour masquer des erreurs backend sans jamais bloquer de
    /// requete entrante).
    pub fn filter_request(&self, ctx: &PluginRequestContext) -> Result<Option<PluginDecision>> {
        self.call_hook("filter_request", ctx)
    }

    /// Hook reponse (ABI v2). `Ok(None)` si le plugin n'exporte pas
    /// `filter_response` : c'est un hook optionnel, son absence n'est pas
    /// une erreur (voir la doc de module plus haut).
    pub fn filter_response(&self, ctx: &PluginResponseContext) -> Result<Option<PluginDecision>> {
        self.call_hook("filter_response", ctx)
    }

    /// Logique partagee entre `filter_request` et `filter_response` : les
    /// deux hooks ont exactement la meme forme (JSON en entree, JSON de
    /// decision en sortie via un i64 empaquete), seul le nom de la fonction
    /// exportee et le type du contexte changent.
    fn call_hook<T: Serialize>(&self, hook_name: &str, ctx: &T) -> Result<Option<PluginDecision>> {
        let (mut store, instance) = self.new_instance()?;

        // Hook optionnel : si le plugin ne l'exporte pas, ce n'est pas une
        // erreur, on saute simplement ce plugin pour ce hook.
        let hook = match instance.get_typed_func::<(i32, i32), i64>(&mut store, hook_name) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };

        let memory = get_memory(&instance, &mut store, &self.name)?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .with_context(|| {
                format!("le plugin '{}' doit exporter 'alloc(len: i32) -> i32'", self.name)
            })?;

        let input = serde_json::to_vec(ctx).context("echec de serialisation du contexte JSON")?;
        let ptr = alloc.call(&mut store, input.len() as i32)?;
        memory
            .write(&mut store, ptr as usize, &input)
            .map_err(|e| anyhow::anyhow!("echec d'ecriture memoire plugin: {e:?}"))?;

        let packed = hook.call(&mut store, (ptr, input.len() as i32))?;
        let out_ptr = ((packed as u64) >> 32) as usize;
        let out_len = ((packed as u64) & 0xFFFF_FFFF) as usize;

        if out_len == 0 || out_len > 1_000_000 {
            bail!(
                "le plugin '{}' a retourne une longueur de sortie suspecte ({out_len} octets) pour '{hook_name}'",
                self.name
            );
        }

        let mut out_bytes = vec![0u8; out_len];
        memory
            .read(&store, out_ptr, &mut out_bytes)
            .map_err(|e| anyhow::anyhow!("echec de lecture memoire plugin: {e:?}"))?;

        let decision = serde_json::from_slice(&out_bytes).with_context(|| {
            format!(
                "le plugin '{}' a retourne un JSON invalide pour '{hook_name}': {}",
                self.name,
                String::from_utf8_lossy(&out_bytes)
            )
        })?;

        Ok(Some(decision))
    }
}

fn get_memory(instance: &Instance, store: &mut Store<()>, plugin_name: &str) -> Result<wasmi::Memory> {
    match instance.get_export(&mut *store, "memory") {
        Some(Extern::Memory(mem)) => Ok(mem),
        _ => bail!("le plugin '{plugin_name}' doit exporter sa memoire sous le nom 'memory'"),
    }
}

/// Charge tous les plugins declares dans la config. Un plugin qui echoue au
/// chargement fait echouer le demarrage entier du proxy : mieux vaut
/// planter tot et bruyamment qu'ignorer silencieusement un plugin casse.
pub fn load_plugins(paths: &[String]) -> Result<Vec<Plugin>> {
    paths.iter().map(|p| Plugin::load(p)).collect::<Result<Vec<_>>>()
}
