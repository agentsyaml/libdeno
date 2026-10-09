use std::fs;
use std::path::PathBuf;

use libdeno::{run_source, LibdenoOptions, SourceLang};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-blob-modules-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(code: &str) {
    const HELPERS: &str = r#"
const waitMessage = (worker) => new Promise((resolve, reject) => {
  const finish = (fn) => { clearTimeout(timer); fn(); };
  const timer = setTimeout(() => finish(() => reject(new Error('worker timeout'))), 10000);
  worker.onmessage = event => finish(() => resolve(event.data));
  worker.onerror = event => { event.preventDefault(); finish(() => reject(new Error(`worker error: ${event.message}`))); };
});
const waitMissingError = (worker, url) => new Promise((resolve, reject) => {
  const finish = (fn) => { clearTimeout(timer); fn(); };
  const timer = setTimeout(() => finish(() => reject(new Error('worker error timeout'))), 10000);
  worker.onmessage = event => finish(() => reject(new Error(`unexpected worker message: ${event.data}`)));
  worker.onerror = event => { event.preventDefault(); finish(() => {
    const message = String(event.message);
    if (!message.includes('Module not found') || !message.includes(url)) reject(new Error(`unexpected worker error: ${message}`));
    else resolve(message);
  }); };
});
const isMissingModule = (error, url) => String(error).includes('Module not found') && String(error).includes(url);
"#;
    let fixture = Fixture::new();
    let code = format!("{HELPERS}\n{code}");
    assert_eq!(
        run_source(
            &code,
            SourceLang::JavaScript,
            &fixture.0,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
}

#[test]
fn dynamic_blob_module_imports() {
    run(
        r#"const url = URL.createObjectURL(new Blob(["export const answer = 42"], { type: "application/javascript" })); const module = await import(url); if (module.answer !== 42) throw new Error('Blob source/media type was not loaded');"#,
    );
}

#[test]
fn blob_worker_posts_reply() {
    run(
        r#"const url = URL.createObjectURL(new Blob(["postMessage('blob-worker-reply')"], { type: "application/javascript" })); const worker = new Worker(url, { type: "module" }); try { if (await waitMessage(worker) !== 'blob-worker-reply') throw new Error('unexpected Blob worker reply'); } finally { worker.terminate(); }"#,
    );
}

#[test]
fn blob_worker_root_survives_immediate_revoke() {
    run(
        r#"const url = URL.createObjectURL(new Blob(["postMessage('captured-root-ran')"], { type: "application/javascript" })); const worker = new Worker(url, { type: "module" }); URL.revokeObjectURL(url); try { if (await waitMessage(worker) !== 'captured-root-ran') throw new Error('captured root did not execute'); } finally { worker.terminate(); }"#,
    );
}

#[test]
fn revoked_blob_dependency_is_not_loaded_in_worker() {
    run(
        r#"const dependency = URL.createObjectURL(new Blob(["globalThis.dependencyRan = true; export const value = 9"], { type: "application/javascript" })); const source = `postMessage('root-started'); try { await import(${JSON.stringify(dependency)}); postMessage('dependency-ran'); } catch (error) { postMessage(String(error)); }`; const root = URL.createObjectURL(new Blob([source], { type: "application/javascript" })); URL.revokeObjectURL(dependency); const worker = new Worker(root, { type: "module" }); try { if (await waitMessage(worker) !== 'root-started') throw new Error('worker root did not execute'); const error = await waitMessage(worker); if (!isMissingModule(error, dependency)) throw new Error(`dependency failure was not a missing-Blob-module error: ${error}`); } finally { worker.terminate(); }"#,
    );
}

#[test]
fn revoked_blob_root_is_only_available_to_capturing_worker() {
    run(
        r#"const source = `onmessage = () => postMessage('captured-root-survived'); postMessage('started')`; const url = URL.createObjectURL(new Blob([source], { type: "application/javascript" })); const first = new Worker(url, { type: "module" }); let second; try { if (await waitMessage(first) !== 'started') throw new Error('capturing worker did not start'); URL.revokeObjectURL(url); let mainError; try { await import(url); } catch (error) { mainError = error; } if (!mainError || !isMissingModule(mainError, url)) throw new Error(`main import was not rejected as missing Blob module: ${mainError}`); second = new Worker(url, { type: "module" }); await waitMissingError(second, url); first.postMessage('continue'); if (await waitMessage(first) !== 'captured-root-survived') throw new Error('capturing worker lost its root Blob'); } finally { second?.terminate(); first.terminate(); }"#,
    );
}
