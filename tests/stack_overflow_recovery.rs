//! A script that exhausts the V8 stack is an ordinary failed run.
//!
//! Static `import` chains are evaluated recursively, so a deep enough module
//! graph overflows the stack the same way an infinitely recursive function
//! does. The important property is that the host process survives it: the next
//! run in the same process still works, because the isolate is torn down per
//! run.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-overflow-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        capture_stderr: true,
        ..Default::default()
    }
}

#[test]
fn a_stack_overflow_does_not_poison_later_runs() {
    let dir = temp_dir("recursion");
    let bad = dir.join("bad.js");
    let good = dir.join("good.js");
    fs::write(&bad, "function f() { f() }\nf();\n").unwrap();
    fs::write(&good, "console.log('GOOD');\n").unwrap();

    let error =
        libdeno::run_with_output(&bad, &options()).expect_err("infinite recursion must fail");
    assert!(
        error
            .to_string()
            .contains("Maximum call stack size exceeded"),
        "unexpected error: {error}"
    );

    let after = libdeno::run_with_output(&good, &options()).expect("a later run must still work");
    assert_eq!(String::from_utf8_lossy(&after.stdout).trim(), "GOOD");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_deep_static_import_chain_fails_cleanly_in_a_subprocess() {
    let dir = temp_dir("chain");
    let chain = dir.join("chain");
    fs::create_dir_all(&chain).unwrap();
    // The exact depth where a debug build overflows depends on frame size, so
    // this uses a depth that is far past any build's limit.
    let depth = 6000;
    for i in 0..depth {
        let body = if i + 1 < depth {
            format!("import './m{}.js';\n", i + 1)
        } else {
            "console.log('END');\n".to_string()
        };
        fs::write(chain.join(format!("m{i}.js")), body).unwrap();
    }

    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let output = libdeno::run_in_subprocess_with_output(chain.join("m0.js"), &options())
        .expect("a stack overflow is a run failure, not an API failure");

    assert_ne!(output.exit_code, 0);
    assert!(
        output.stdout.is_empty(),
        "no script output before the failure"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Maximum call stack size exceeded"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = fs::remove_dir_all(dir);
}
