use libdeno::deno_core::{op2, OpState};
use libdeno::{run, ExtensionFactory, LibdenoOptions};
use std::fs;

#[op2]
#[string]
fn op_native_counter(state: &mut OpState) -> String {
    let counter = state.borrow_mut::<usize>();
    *counter += 1;
    counter.to_string()
}

deno_core::extension!(native_counter,
    ops = [op_native_counter],
    esm_entry_point = "ext:native_counter/init.js",
    esm = [dir "tests/native_extension", "init.js"],
    state = |state| state.put(0usize),
);

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        extensions: Some(ExtensionFactory::new(|| vec![native_counter::init()])),
        ..Default::default()
    }
}

#[test]
fn native_op_is_initialized_in_main_and_recursive_web_workers_per_run() {
    let dir = std::env::temp_dir().join(format!("libdeno-native-op-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("worker.js"),
        r#"
const own = nativeCounter();
if (new URL(import.meta.url).searchParams.has("nested")) { postMessage(own); close(); }
else {
  const child = new Worker(new URL("./worker.js?nested", import.meta.url), { type: "module" });
  child.onmessage = (event) => { postMessage(`${own}:${event.data}`); child.terminate(); close(); };
  child.onerror = () => postMessage("error");
}
"#,
    )
    .unwrap();
    fs::write(dir.join("main.js"), r#"
if (nativeCounter() !== "1") throw new Error("main op state was not fresh");
const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
const result = await new Promise((resolve, reject) => { worker.onmessage = e => resolve(e.data); worker.onerror = reject; });
if (result !== "1:1") throw new Error(`unexpected nested worker results: ${result}`);
worker.terminate();
Deno.exit(0);
"#).unwrap();

    let entry = dir.join("main.js");
    let options = options();
    assert_eq!(run(&entry, &options).unwrap(), 0);
    assert_eq!(run(&entry, &options).unwrap(), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn native_extensions_are_rejected_before_subprocess_spawn() {
    let error = libdeno::run_in_subprocess("missing.js", &options()).unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Configuration(message) if message.contains("native extension factories"))
    );
}

#[cfg(feature = "execution-control")]
#[test]
fn native_extensions_are_rejected_before_supervised_subprocess_spawn() {
    let error = libdeno::run_in_supervised_subprocess("missing.js", &options()).unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Configuration(message) if message.contains("native extension factories"))
    );
}
