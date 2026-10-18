import {
  op_net_listen_udp as udp,
  op_net_listen_unixpacket as unixpacket,
  op_net_listen_vsock as vsockListen,
  op_net_connect_vsock as vsockConnect,
  op_kv_database_open as kvOpen,
  op_cron_create as cronCreate,
} from "ext:core/ops";

globalThis.__libdeno_guard_probe__ = async () => {
  const mask = globalThis.__libdeno_guard_probe_mask__;
  const denied = (name, call, feature) => {
    try { call(); throw new Error(`${name} guard returned`); }
    catch (error) { if (!error.message.includes(feature)) throw error; }
  };
  const deniedAsync = async (name, promise, feature) => {
    try { await promise; throw new Error(`${name} guard returned`); }
    catch (error) { if (!error.message.includes(feature)) throw error; }
  };
  denied("UDP", () => udp({ hostname: "127.0.0.1", port: 0 }, false, false), "net feature");
  if (mask & 1) denied("Unix packet", () => unixpacket("/tmp/libdeno-guard.sock"), "net feature");
  if (mask & 2) {
    denied("vsock listen", () => vsockListen(2, 1024), "vsock feature");
    await deniedAsync("vsock connect", vsockConnect(2, 1024), "vsock feature");
  }
  denied("cron", () => cronCreate("guard", "* * * * *", undefined), "cron feature");
  await deniedAsync("KV open", kvOpen(undefined), "kv feature");
};
