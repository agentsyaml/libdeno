use libdeno::{run, LibdenoOptions};
use std::fs;

#[test]
fn buffer_and_shared_array_buffer_transfers_work_in_fresh_run_local_stores() {
    let dir = std::env::temp_dir().join(format!("libdeno-buffer-transfer-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("worker.js"),
        r#"self.onmessage = ({ data }) => {
  const nested = new Worker(new URL("./nested.js", import.meta.url), { type: "module" });
  nested.onmessage = event => { postMessage(event.data, [event.data]); nested.terminate(); };
  nested.onerror = event => postMessage(`nested worker failed: ${event.message}`);
  nested.postMessage(data, [data.buffer]);
};"#,
    )
    .unwrap();
    fs::write(
        dir.join("nested.js"),
        r#"self.onmessage = ({ data }) => {
  new Uint8Array(data.buffer)[0] = 42;
  Atomics.store(new Int32Array(data.shared), 0, 84);
  postMessage(data.buffer, [data.buffer]);
};"#,
    )
    .unwrap();
    fs::write(
        dir.join("wasm-worker.js"),
        r#"self.onmessage = async ({ data }) => {
  const instance = await WebAssembly.instantiate(data);
  postMessage(instance instanceof WebAssembly.Instance);
};"#,
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        r#"
const original = new ArrayBuffer(4);
const clone = structuredClone(original, { transfer: [original] });
if (original.byteLength !== 0 || clone.byteLength !== 4) throw Error("structuredClone did not detach the transfer source");

const wasm = new WebAssembly.Module(new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]));
const wasmClone = structuredClone(wasm);
if (!(wasmClone instanceof WebAssembly.Module)) throw Error("structuredClone lost the WebAssembly module");
const wasmWorker = new Worker(new URL("./wasm-worker.js", import.meta.url), { type: "module" });
const wasmReply = new Promise((resolve, reject) => {
  wasmWorker.onmessage = event => resolve(event.data);
  wasmWorker.onerror = event => reject(new Error(event.message));
});
wasmWorker.postMessage(wasmClone);
if (await wasmReply !== true) throw Error("worker could not instantiate the cloned WebAssembly module");
wasmWorker.terminate();

const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
const reply = new Promise((resolve, reject) => {
  worker.onmessage = event => resolve(event.data);
  worker.onerror = event => reject(new Error(event.message));
});
const buffer = new ArrayBuffer(4);
const shared = new SharedArrayBuffer(4);
worker.postMessage({ buffer, shared }, [buffer]);
if (buffer.byteLength !== 0) throw Error("postMessage did not detach the transfer source");
const returned = await reply;
if (returned.byteLength !== 4 || new Uint8Array(returned)[0] !== 42) throw Error("worker transfer returned wrong bytes");
if (Atomics.load(new Int32Array(shared), 0) !== 84) throw Error("SharedArrayBuffer changes were not shared");
worker.terminate();

const { Worker: NodeWorker } = await import("node:worker_threads");
const nodeWorker = new NodeWorker(`const {parentPort}=require('node:worker_threads'); parentPort.on('message', ({buffer,shared})=>{new Uint8Array(buffer)[0]=21; Atomics.store(new Int32Array(shared),0,42); parentPort.postMessage(buffer,[buffer]);});`, { eval: true });
const nodeReply = new Promise((resolve, reject) => { nodeWorker.once("message", resolve); nodeWorker.once("error", reject); });
const nodeBuffer = new ArrayBuffer(4);
const nodeShared = new SharedArrayBuffer(4);
nodeWorker.postMessage({ buffer: nodeBuffer, shared: nodeShared }, [nodeBuffer]);
if (nodeBuffer.byteLength !== 0) throw Error("Node worker postMessage did not detach the transfer source");
const nodeReturned = await nodeReply;
if (nodeReturned.byteLength !== 4 || new Uint8Array(nodeReturned)[0] !== 21) throw Error("Node worker transfer returned wrong bytes");
if (Atomics.load(new Int32Array(nodeShared), 0) !== 42) throw Error("Node worker SharedArrayBuffer changes were not shared");
await nodeWorker.terminate();
Deno.exit(0);
"#,
    )
    .unwrap();

    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run(&entry, &options).unwrap(), 0);
    assert_eq!(run(&entry, &options).unwrap(), 0);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| run(&entry, &options));
        let second = scope.spawn(|| run(&entry, &options));
        assert_eq!(first.join().unwrap().unwrap(), 0);
        assert_eq!(second.join().unwrap().unwrap(), 0);
    });
    let _ = fs::remove_dir_all(dir);
}
