//! A script with `--allow-env` must not be able to rewrite libdeno's own
//! control variables.
//!
//! `LIBDENO_HOST_EXE` selects the binary `run_in_subprocess` executes, so
//! letting a script set it would turn `--allow-env` into an arbitrary-code
//! grant: the next subprocess run would execute whatever the script named,
//! outside the permission model and with no `--allow-run` at all. The same
//! applies to the child/supervisor handshake variables, which authenticate a
//! run request.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// In-process capture holds a process-global lease, so these tests must not
/// overlap with each other (or with any other capturing run in this binary).
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn capture_lock() -> std::sync::MutexGuard<'static, ()> {
    CAPTURE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-ctrlenv-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &Path) -> LibdenoOptions {
    LibdenoOptions {
        // --allow-env is the only capability here on purpose.
        permissions: vec![
            "--allow-env".into(),
            format!("--allow-read={}", dir.display()),
        ],
        capture_stdout: true,
        ..Default::default()
    }
}

#[test]
fn a_script_cannot_set_the_host_executable_override() {
    let _capture = capture_lock();
    let dir = temp_dir("host-exe");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "let outcome = 'allowed';\n\
         try { Deno.env.set('LIBDENO_HOST_EXE', '/bin/echo'); }\n\
         catch (e) { outcome = e.name; }\n\
         console.log(`set=${outcome}`);",
    )
    .unwrap();
    let output = libdeno::run_with_output(&entry, &options(&dir)).unwrap();
    // In-process capture redirects the process-global fd 1, so the harness's
    // own output can land in `stdout` too. Assert on the script's line, not on
    // the exact byte string.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("set=NotCapable"),
        "the control variable must be denied even with --allow-env, got: {stdout}"
    );
    assert!(
        std::env::var_os("LIBDENO_HOST_EXE").is_none(),
        "the host process environment must be untouched"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_denial_covers_every_control_variable() {
    let _capture = capture_lock();
    let dir = temp_dir("all-vars");
    let entry = dir.join("main.js");
    // Every name the library reads to make a runtime decision.
    let names = [
        "LIBDENO_HOST_EXE",
        "LIBDENO_CHILD_MODE",
        "LIBDENO_CHILD_TOKEN",
        "LIBDENO_SUPERVISOR_MODE",
        "LIBDENO_SUPERVISOR_ENDPOINT",
        "LIBDENO_SUPERVISOR_TOKEN",
        "LIBDENO_ANALYSIS_CACHE_ENTRIES",
        "LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES",
        "DENO_DIR",
        "DENO_NO_PACKAGE_JSON",
        "JSR_URL",
        "NPM_CONFIG_REGISTRY",
        "npm_config_registry",
        "NPM_CONFIG_REPLACE_REGISTRY_HOST",
        "npm_config_replace_registry_host",
    ];
    let script = names
        .iter()
        .map(|name| {
            format!(
                "try {{ Deno.env.set('{name}', 'x'); console.log('{name}=allowed'); }}\n\
                 catch (e) {{ console.log('{name}=' + e.name); }}\n"
            )
        })
        .collect::<String>();
    fs::write(&entry, script).unwrap();
    let output = libdeno::run_with_output(&entry, &options(&dir)).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for name in names {
        assert!(
            stdout.contains(&format!("{name}=NotCapable")),
            "{name} must be denied, got: {stdout}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ordinary_environment_variables_are_still_writable() {
    let _capture = capture_lock();
    let dir = temp_dir("ordinary");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "Deno.env.set('LIBDENO_ORDINARY_PROBE', 'yes');\n\
         console.log('value=' + Deno.env.get('LIBDENO_ORDINARY_PROBE'));",
    )
    .unwrap();
    let output = libdeno::run_with_output(&entry, &options(&dir)).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("value=yes"),
        "the control-variable deny list must not block unrelated names, got: {stdout}"
    );
    std::env::remove_var("LIBDENO_ORDINARY_PROBE");
    let _ = fs::remove_dir_all(dir);
}

/// `-A` / `allow_all_permissions` bypasses the deny list, exactly as it
/// bypasses every other permission.
///
/// `docs/api.md` and `SECURITY.md` both state this. It is the reason the deny
/// list is described as a guard for scoped runs, not a guarantee under `-A`:
/// a host that hands a plugin `-A` has already granted arbitrary code, so
/// blocking one variable would be theatre.
#[test]
fn allow_all_permissions_bypasses_the_deny_list() {
    let _capture = capture_lock();
    let dir = temp_dir("allow-all");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "let outcome = 'denied';\n\
         try { Deno.env.set('LIBDENO_HOST_EXE', '/bin/echo'); outcome = 'allowed'; }\n\
         catch (e) { outcome = e.name; }\n\
         console.log(`set=${outcome}`);",
    )
    .unwrap();
    let output = libdeno::run_with_output(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = fs::remove_dir_all(&dir);
    // The variable is process-global; clear whatever the script set.
    std::env::remove_var("LIBDENO_HOST_EXE");

    assert!(
        stdout.contains("set=allowed"),
        "-A must bypass the deny list, matching every other permission: {stdout}"
    );
}

/// `allow_all_permissions` wins over a non-empty `permissions` list.
///
/// The two fields are adjacent and each can be filled by a different part of a
/// host (a policy flag and a per-request capability list). The documented rule
/// is that the list is *discarded*, not intersected, so the result is the wider
/// grant. Without this test, a change to "permissions narrows the allow-all
/// fallback" would look like a hardening fix and silently reverse the
/// documented semantics for every host that sets both.
#[test]
fn allow_all_permissions_discards_a_non_empty_permission_list() {
    let _capture = capture_lock();
    let dir = temp_dir("allow-all-wins");
    let entry = dir.join("main.js");
    // The grant below covers `dir` only. If the list were honored, reading
    // `/etc/hosts` would be denied.
    fs::write(
        &entry,
        "let outcome = 'denied';\n\
         try { Deno.readTextFileSync('/etc/hosts'); outcome = 'read'; }\n\
         catch (e) { outcome = e.name; }\n\
         console.log(`read=${outcome}`);",
    )
    .unwrap();
    let output = libdeno::run_with_output(
        &entry,
        &LibdenoOptions {
            permissions: vec![format!("--allow-read={}", dir.display())],
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .expect("the run must succeed");
    let _ = fs::remove_dir_all(dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("read=read"),
        "allow_all_permissions must discard the list, got: {stdout}"
    );
}

/// The `Permission` prefix is the variant's only text-level signal.
///
/// `Permission` and `Configuration` are both `String` payloads, so a host
/// cannot tell them apart by type alone once the error is flattened to text.
/// `docs/api.md` documents the prefix; this pins it so a refactor to
/// `#[error("{0}")]` — which would read like a cleanup — shows up here.
#[test]
fn invalid_permission_flags_keep_their_documented_prefix() {
    let _capture = capture_lock();
    let dir = temp_dir("permission-prefix");
    let entry = dir.join("main.js");
    fs::write(&entry, "1;").unwrap();
    let error = libdeno::run(
        &entry,
        &LibdenoOptions {
            permissions: vec!["--not-a-permission".into()],
            ..Default::default()
        },
    )
    .unwrap_err();
    let _ = fs::remove_dir_all(dir);
    assert!(
        matches!(error, libdeno::LibdenoError::Permission(_)),
        "expected Permission, got: {error:?}"
    );
    let text = error.to_string();
    assert!(
        text.starts_with("invalid permission flags: "),
        "expected the documented prefix, got: {text}"
    );
    assert!(
        text.contains("unknown permission flag: --not-a-permission"),
        "the inner parser message must survive, got: {text}"
    );
}

/// The deny list covers `get` and `delete`, and it outranks an explicit grant.
///
/// `docs/api.md` says `Deno.env.set`/`get`/`delete` all raise `NotCapable`, and
/// that naming one of these variables in `--allow-env` does not lift it. Only
/// `set` under a bare `--allow-env` was covered. The read path matters most: a
/// script that reads `DENO_DIR` or `LIBDENO_HOST_EXE` learns where the host
/// caches modules and which binary the next subprocess run will execute, even
/// though it cannot change them.
#[test]
fn reading_and_deleting_a_control_variable_is_denied_even_when_named_in_the_grant() {
    let _capture = capture_lock();
    let dir = temp_dir("explicit-grant");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "const out = [];\n\
         for (const op of ['get', 'delete']) {\n\
         \x20 try {\n\
         \x20   if (op === 'get') Deno.env.get('DENO_DIR');\n\
         \x20   else Deno.env.delete('DENO_DIR');\n\
         \x20   out.push(op + '=allowed');\n\
         \x20 } catch (e) { out.push(op + '=' + e.name); }\n\
         }\n\
         console.log(out.join(','));",
    )
    .unwrap();
    // `--allow-env` names the variable explicitly and also matches it with a
    // prefix pattern; neither may defeat the deny list.
    let mut options = options(&dir);
    options.permissions = vec![
        "--allow-env=DENO_DIR,LIBDENO_*".into(),
        format!("--allow-read={}", dir.display()),
    ];
    let output = libdeno::run_with_output(&entry, &options).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let _ = fs::remove_dir_all(dir);
    assert!(
        stdout.contains("get=NotCapable,delete=NotCapable"),
        "an explicit --allow-env must not lift the control-variable denial, got: {stdout}"
    );
}
