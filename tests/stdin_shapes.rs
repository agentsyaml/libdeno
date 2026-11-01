//! stdin differs per backend, and only the child-protocol path is silent about
//! it: it consumes stdin for the run request, so the script sees EOF.
//!
//! In-process runs and the supervisor backend inherit the host's stdin, so a
//! script that reads stdin works there. A caller that moves the same script to
//! `run_in_subprocess` gets empty input with no error. These tests pin the EOF
//! so the difference stays deliberate.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

const READ_STDIN: &str = "\
const chunks = [];
for await (const c of Deno.stdin.readable) chunks.push(c);
const text = new TextDecoder().decode(new Uint8Array(chunks.flatMap((c) => [...c])));
console.log(`STDIN=[${text}]`);
";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-stdin-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(dir: &Path) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(&entry, READ_STDIN).unwrap();
    entry
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    }
}

#[test]
fn child_protocol_subprocess_gives_the_script_eof_on_stdin() {
    let dir = temp_dir("helper");
    let entry = entry(&dir);
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let output = libdeno::run_in_subprocess_with_output(&entry, &options()).unwrap();
    assert_eq!(output.exit_code, 0);
    // Not "the host's stdin was empty": the run request owns that pipe, and it
    // is closed after the payload write. A script reading stdin cannot tell
    // this apart from a caller that piped nothing.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "STDIN=[]\n",
        "the child protocol must not forward host stdin"
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(feature = "execution-control")]
#[test]
fn executor_subprocess_backend_inherits_the_child_protocol_stdin() {
    use libdeno::{ExecutionBackend, ExecutionRequest, Executor};

    let dir = temp_dir("executor");
    let entry = entry(&dir);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = runtime
        .block_on(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .build(),
        )
        .unwrap();
    let result = executor
        .execute(ExecutionRequest::new(&entry, options()))
        .expect("the script must run");
    assert_eq!(result.exit_code(), 0);
    assert_eq!(
        String::from_utf8_lossy(result.output().stdout()),
        "STDIN=[]\n",
        "execute on the subprocess backend uses the child protocol, so stdin is EOF"
    );
    let _ = fs::remove_dir_all(dir);
}
