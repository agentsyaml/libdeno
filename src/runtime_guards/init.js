import { core } from "ext:core/mod.js";

const ops = core.ops;
const tcp = ops["op_libdeno_net_listen_tcp_alias"];
const tls = ops["op_libdeno_net_listen_tls_alias"];
const install = ops["op_libdeno_install_net_delegates"];
if ([tcp, tls, install].some((op) => typeof op !== "function")) {
  throw new Error("runtime guard op delegates were not registered");
}
const testRouteMask = install(tcp, tls);
delete ops["op_libdeno_net_listen_tcp_alias"];
delete ops["op_libdeno_net_listen_tls_alias"];
delete ops["op_libdeno_install_net_delegates"];
if (ops["op_libdeno_net_listen_tcp_alias"] !== undefined || ops["op_libdeno_net_listen_tls_alias"] !== undefined || ops["op_libdeno_install_net_delegates"] !== undefined) {
  throw new Error("runtime guard delegate ops were not hidden after installation");
}
if (testRouteMask !== 0) globalThis.__libdeno_guard_probe_mask__ = testRouteMask;
