//! `LibdenoOptions::args` lands where Deno puts it, on both backends.
//!
//! `docs/api.md` says the arguments are "exposed to the script via
//! `process.argv` (after argv[0])", which reads as if they start at index 1.
//! They do not: `process.argv[1]` is the entry module, exactly like the Deno
//! CLI, and the arguments start at index 2. `Deno.args` is the list without the
//! executable and the entry.
//!
//! Only `process.argv[2]` was ever asserted (in `tests/smoke.rs`, subprocess
//! only), so the entry's position and `Deno.args` on the in-process backend
//! were unpinned. A host that forwards its own command line and then indexes
//! argv itself depends on that layout.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-args-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The script reports its own view of argv: `argv.json` in the directory it is
/// told about, and its exit code says whether the write happened.
fn setup(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "Deno.writeTextFileSync(\n\
           Deno.args[0] + '/argv.json',\n\
           JSON.stringify({ argv: process.argv, args: Deno.args }),\n\
         );",
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    (dir, entry)
}

/// `args[0]` is where the report goes, then two arguments the assertions look
/// for. Script-visible arguments are therefore the directory plus a marker.
fn options(dir: &std::path::Path) -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        args: vec![
            dir.display().to_string(),
            "--second".to_string(),
            "third".to_string(),
        ],
        ..Default::default()
    }
}

#[test]
fn process_argv_puts_the_entry_first_and_the_arguments_after_it() {
    let (dir, entry) = setup("layout");
    for backend in ["in-process", "subprocess"] {
        let options = options(&dir);
        match backend {
            "in-process" => {
                let code = libdeno::run(&entry, &options).expect("in-process run must succeed");
                assert_eq!(code, 0, "{backend}");
            }
            _ => {
                let output = libdeno::run_in_subprocess_with_output(&entry, &options)
                    .expect("subprocess run must succeed");
                assert_eq!(output.exit_code, 0, "{backend}");
            }
        }
        let captured = fs::read_to_string(dir.join("argv.json"))
            .unwrap_or_else(|e| panic!("{backend}: the script wrote no report: {e}"));
        fs::remove_file(dir.join("argv.json")).unwrap();
        assert!(
            captured.contains("\"argv\""),
            "{backend}: unexpected payload: {captured}"
        );
        // argv[0] is the embedding executable and argv[1] the entry, so the
        // caller's arguments start at index 2.
        assert!(
            captured.contains(&format!("\"{}\"", entry.display())),
            "{backend}: process.argv[1] must be the entry: {captured}"
        );
        assert!(
            captured.contains("\"--second\",\"third\""),
            "{backend}: arguments must follow the entry in order: {captured}"
        );
        // Deno.args drops the executable and the entry, keeping argv[0] of the
        // caller's list as its first element.
        assert!(
            captured.contains(&format!(
                "\"args\":[\"{}\",\"--second\",\"third\"]",
                dir.display()
            )),
            "{backend}: Deno.args must be the caller's list alone: {captured}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}
