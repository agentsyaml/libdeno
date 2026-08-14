/*
 * Minimal Node-API addon used by tests/native_addon.rs.
 *
 * Exports nothing but `napi_register_module_v1`, which returns the exports
 * object unchanged. The point is to exercise op_napi_open's load path (ffi
 * permission check -> dlopen -> symbol lookup -> register call) without
 * needing node_api.h, which deno_napi does not ship. A real NAPI_MODULE
 * addon would additionally link against the host's exported napi_* symbols;
 * that is orthogonal to the loader question this test guards.
 */
#ifdef _WIN32
#define ADDON_EXPORT __declspec(dllexport)
#else
#define ADDON_EXPORT __attribute__((visibility("default")))
#endif

typedef void *napi_env;
typedef void *napi_value;

ADDON_EXPORT napi_value napi_register_module_v1(napi_env env, napi_value exports) {
  (void)env;
  return exports;
}
