// error_masker.c — Plugin ABI v2, hook filter_response uniquement.
//
// N'implémente PAS filter_request : c'est un plugin "response-only",
// démontrant que les deux hooks sont indépendants.
//
// Règle : si le backend renvoie un statut >= 500, on le remplace par un 502
// générique avec un message neutre (évite de divulguer des détails internes
// du backend — stack traces, versions de framework, etc. — dans la réponse
// finale envoyée au client).

typedef unsigned int u32;
typedef unsigned long long u64;

static char in_buf[2048];
static char out_buf[256];

static int str_len(const char *s) {
    int n = 0;
    while (s[n] != '\0') n++;
    return n;
}

static void mem_copy(char *dst, const char *src, int n) {
    for (int i = 0; i < n; i++) dst[i] = src[i];
}

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

// Extrait un entier positionné juste après "key":  (ex: "status":503)
// Suppose que la valeur est un nombre non-négatif sans espace après ':'.
static int extract_json_int(const char *buf, int len, const char *key) {
    int key_pos = find(buf, len, key);
    if (key_pos < 0) return -1;
    int i = key_pos + str_len(key);
    int value = 0;
    int any_digit = 0;
    while (i < len && buf[i] >= '0' && buf[i] <= '9') {
        value = value * 10 + (buf[i] - '0');
        any_digit = 1;
        i++;
    }
    return any_digit ? value : -1;
}

__attribute__((export_name("alloc")))
int alloc(int len) {
    (void)len;
    return (int)(long)in_buf;
}

__attribute__((export_name("filter_response")))
u64 filter_response(int ptr, int len) {
    const char *resp = (const char *)(long)ptr;

    int status = extract_json_int(resp, len, "\"status\":");

    const char *out;
    if (status >= 500) {
        out = "{\"action\":\"block\",\"status\":502,\"body\":\"upstream error\"}";
    } else {
        out = "{\"action\":\"continue\"}";
    }

    int out_len = str_len(out);
    mem_copy(out_buf, out, out_len);

    u64 out_ptr = (u64)(long)out_buf;
    return (out_ptr << 32) | (u32)out_len;
}
