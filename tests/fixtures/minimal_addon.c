/*
 * Minimal Node-API addon used by tests/native_addon.rs.
 *
 * Unlike an empty-shell addon, this one actually uses the Node-API: it calls
 * napi_create_function / napi_set_named_property to export an `add(a, b)`
 * function. That exercises the full load path of a real native package:
 * op_napi_open (ffi permission check -> dlopen -> symbol resolution ->
 * register call) AND the host's exported napi_* symbols, which dlopen'ed
 * addons link against (napi_sym's 214-symbol export list; unix test binaries
 * get --export-dynamic from .cargo/config.toml). A shell-only addon would
 * pass without the host exporting anything, missing the most common real-
 * world failure mode ("Failed to load native module" style packages).
 *
 * The napi_* declarations below mirror the stable node_api.h ABI; deno_napi
 * does not ship the header, so they are spelled out.
 */
#ifdef _WIN32
#define ADDON_EXPORT __declspec(dllexport)
#else
#define ADDON_EXPORT __attribute__((visibility("default")))
#endif

#include <stddef.h>

typedef void *napi_env;
typedef void *napi_value;
typedef void *napi_callback_info;
typedef napi_value (*napi_callback)(napi_env, napi_callback_info);
typedef int napi_status;
#define napi_ok 0

napi_status napi_create_function(napi_env, const char *, size_t, napi_callback,
                                 void *, napi_value *);
napi_status napi_set_named_property(napi_env, napi_value, const char *,
                                    napi_value);
napi_status napi_get_cb_info(napi_env, napi_callback_info, size_t *,
                             napi_value *, napi_value *, void **);
napi_status napi_get_value_double(napi_env, napi_value, double *);
napi_status napi_create_double(napi_env, double, napi_value *);

static napi_value add_cb(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value argv[2];
  napi_value result = NULL;
  if (napi_get_cb_info(env, info, &argc, argv, NULL, NULL) != napi_ok) {
    return NULL;
  }
  if (argc < 2) {
    return NULL;
  }
  double a = 0;
  double b = 0;
  if (napi_get_value_double(env, argv[0], &a) != napi_ok) {
    return NULL;
  }
  if (napi_get_value_double(env, argv[1], &b) != napi_ok) {
    return NULL;
  }
  if (napi_create_double(env, a + b, &result) != napi_ok) {
    return NULL;
  }
  return result;
}

ADDON_EXPORT napi_value napi_register_module_v1(napi_env env,
                                                napi_value exports) {
  napi_value add_fn = NULL;
  if (napi_create_function(env, "add", 3, add_cb, NULL, &add_fn) == napi_ok) {
    napi_set_named_property(env, exports, "add", add_fn);
  }
  return exports;
}
