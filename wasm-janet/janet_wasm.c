/* Minimal Janet VM entry points for the dirge wasm bundle.
 *
 * Compiled with Emscripten against the amalgamated Janet core
 * (build/c/janet.c). This is deliberately pared down: it exposes just
 * init + eval (no dirge FFI, no plugin tool bridge, no threads). The
 * full dirge plugin layer stays native; this lets the demo and the
 * Node tests run plain Janet code inside the bundle.
 *
 * Rebuild with ../build.sh (see README.md).
 */
#include <string.h>
#include "janet.h"

static JanetTable *g_env;

void janet_wasm_init(void) {
    janet_init();
    g_env = janet_core_env(NULL);
}

/* Evaluate `code` and return the result as a malloc'd C string. Values
 * are pretty-printed like the Janet REPL (strings quoted, collections in
 * @[...] form). Errors are prefixed with "ERROR: " so callers can tell a
 * real error apart from a value that merely looks like one. The caller
 * frees the returned buffer (Module._free in JS). */
char *janet_wasm_eval(const char *code) {
    Janet out;
    JanetSignal sig = janet_dostring(g_env, code, "eval", &out);

    if (sig == JANET_SIGNAL_OK) {
        JanetBuffer buf;
        janet_buffer_init(&buf, 0);
        janet_pretty(&buf, 0, JANET_PRETTY_ONELINE | JANET_PRETTY_NOTRUNC, out);
        const uint8_t *s = janet_string(buf.data, buf.count);
        char *r = strdup((const char *)s);
        janet_buffer_deinit(&buf);
        return r;
    }

    const char *s = (const char *)janet_to_string(out);
    size_t n = strlen(s);
    char *r = (char *)malloc(n + 8);
    memcpy(r, "ERROR: ", 7);
    memcpy(r + 7, s, n + 1);
    return r;
}
