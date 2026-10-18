import { core } from "ext:core/mod.js";

const ops = core.ops;
const tcp = ops["op_libdeno_net_listen_tcp_alias"];
const tls = ops["op_libdeno_net_listen_tls_alias"];
const install = ops["op_libdeno_install_net_delegates"];
const requireAliases = [
  ops["op_libdeno_require_stat_alias"],
  ops["op_libdeno_require_real_path_alias"],
  ops["op_libdeno_require_package_scope_alias"],
  ops["op_libdeno_require_package_imports_alias"],
];
const installRequire = ops["op_libdeno_install_require_delegates"];
if ([tcp, tls, install, installRequire, ...requireAliases].some((op) => typeof op !== "function")) {
  throw new Error("runtime guard op delegates were not registered");
}
const testRouteMask = install(tcp, tls);
installRequire(...requireAliases);
delete ops["op_libdeno_net_listen_tcp_alias"];
delete ops["op_libdeno_net_listen_tls_alias"];
delete ops["op_libdeno_install_net_delegates"];
delete ops["op_libdeno_require_stat_alias"];
delete ops["op_libdeno_require_real_path_alias"];
delete ops["op_libdeno_require_package_scope_alias"];
delete ops["op_libdeno_require_package_imports_alias"];
delete ops["op_libdeno_install_require_delegates"];
if ([
  "op_libdeno_net_listen_tcp_alias",
  "op_libdeno_net_listen_tls_alias",
  "op_libdeno_install_net_delegates",
  "op_libdeno_require_stat_alias",
  "op_libdeno_require_real_path_alias",
  "op_libdeno_require_package_scope_alias",
  "op_libdeno_require_package_imports_alias",
  "op_libdeno_install_require_delegates",
].some((name) => ops[name] !== undefined)) {
  throw new Error("runtime guard delegate ops were not hidden after installation");
}
if (testRouteMask !== 0) globalThis.__libdeno_guard_probe_mask__ = testRouteMask;
