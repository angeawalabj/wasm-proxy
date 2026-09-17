// admin_guard.c — Plugin ABI v2, freestanding (pas de libc).
//
// ABI v2 :
//   memory (export)
//   alloc(len: i32) -> i32
//       Réserve un espace pour que l'hôte y écrive `len` octets.
//   filter_request(ptr: i32, len: i32) -> i64
//       Lit `len` octets de JSON à `ptr` :
//         {"method":"GET","path":"/admin","headers":{"x-api-key":"..."}}
//       Retourne un i64 empaqueté : (out_ptr << 32) | out_len, pointant
//       vers du JSON de sortie écrit par le plugin lui-même :
//         {"action":"continue","add_headers":{"x-plugin-checked":"1"}}
//         {"action":"block","status":403,"body":"missing api key"}
//
// Règle métier : "/admin" nécessite le header "x-api-key: secret123".
// Sans le bon header -> bloqué (403). Sinon -> laissé passer, avec un
// header de traçabilité ajouté sur la réponse.

typedef unsigned int u32;
typedef unsigned long long u64;

// --- Buffers statiques (pas de malloc en freestanding) ---
static char in_buf[4096];
static char out_buf[512];

// --- Utilitaires bas niveau (pas de libc disponible) ---

static int str_len(const char *s) {
    int n = 0;
    while (s[n] != '\0') n++;
    return n;
}

static void mem_copy(char *dst, const char *src, int n) {
    for (int i = 0; i < n; i++) dst[i] = src[i];
}

// Cherche `needle` (C-string) dans hay[0..hay_len). Retourne l'index de
// début du match, ou -1. Recherche naïve O(n*m), largement suffisant pour
// des messages JSON courts.
static int find(const char *hay, int hay_len, const char *needle) {
    int nlen = str_len(needle);
    if (nlen == 0 || nlen > hay_len) return -1;
    for (int i = 0; i <= hay_len - nlen; i++) {
        int j = 0;
        while (j < nlen && hay[i + j] == needle[j]) j++;
        if (j == nlen) return i;
    }
    return -1;
}

// Extrait la valeur associée à une clé JSON de la forme "key":"value".
// Ecrit la valeur dans `out` (sans les guillemets), retourne sa longueur,
// ou -1 si la clé est absente. Ne gère pas l'échappement JSON : suffisant
// ici car l'hôte contrôle l'encodage et n'émet pas de guillemets internes
// pour ces champs simples (méthode, chemin, valeurs de header courantes).
static int extract_json_string(const char *buf, int len, const char *key, char *out, int out_cap) {
    int key_pos = find(buf, len, key);
    if (key_pos < 0) return -1;

    int i = key_pos + str_len(key);
    // avance jusqu'au premier guillemet ouvrant la valeur
    while (i < len && buf[i] != '"') i++;
    if (i >= len) return -1;
    i++; // saute le guillemet ouvrant

    int start = i;
    while (i < len && buf[i] != '"') i++;
    int value_len = i - start;
    if (value_len >= out_cap) value_len = out_cap - 1;

    mem_copy(out, buf + start, value_len);
    out[value_len] = '\0';
    return value_len;
}

__attribute__((export_name("alloc")))
int alloc(int len) {
    // Instance fraîche par requête (voir plugin.rs côté hôte) : un buffer
    // statique unique suffit, pas besoin d'un vrai allocateur.
    (void)len;
    return (int)(long)in_buf;
}

__attribute__((export_name("filter_request")))
u64 filter_request(int ptr, int len) {
    const char *req = (const char *)(long)ptr;

    char path[256];
    int path_len = extract_json_string(req, len, "\"path\":", path, sizeof(path));

    int is_admin = (path_len == 6
        && path[0] == '/' && path[1] == 'a' && path[2] == 'd'
        && path[3] == 'm' && path[4] == 'i' && path[5] == 'n');

    const char *out;
    int out_len;

    if (is_admin) {
        char api_key[64];
        int key_len = extract_json_string(req, len, "\"x-api-key\":", api_key, sizeof(api_key));

        int authorized = (key_len == 9
            && api_key[0] == 's' && api_key[1] == 'e' && api_key[2] == 'c'
            && api_key[3] == 'r' && api_key[4] == 'e' && api_key[5] == 't'
            && api_key[6] == '1' && api_key[7] == '2' && api_key[8] == '3');

        if (authorized) {
            out = "{\"action\":\"continue\",\"add_headers\":{\"x-plugin-checked\":\"admin_guard\"}}";
        } else {
            out = "{\"action\":\"block\",\"status\":403,\"body\":\"missing or invalid api key\"}";
        }
    } else {
        out = "{\"action\":\"continue\"}";
    }

    out_len = str_len(out);
    mem_copy(out_buf, out, out_len);

    u64 out_ptr = (u64)(long)out_buf;
    return (out_ptr << 32) | (u32)out_len;
}
