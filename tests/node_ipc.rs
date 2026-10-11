use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CHILD_MODE: &str = "LIBDENO_NODE_IPC_TEST_MODE";

fn run_bounded(mut command: Command) {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            terminate_test_process_tree(&mut child);
            assert!(status.success(), "isolated IPC process exited {status}");
            return;
        }
        if Instant::now() >= deadline {
            terminate_test_process_tree(&mut child);
            panic!("isolated IPC process exceeded 35s; its process group was terminated");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn terminate_test_process_tree(child: &mut Child) {
    let group = child.id() as i32;
    unsafe {
        libc::kill(-group, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let _ = child.try_wait();
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn terminate_test_process_tree(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn isolated_command(mode: &str) -> Command {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "isolated_node_ipc_process", "--nocapture"])
        .env(CHILD_MODE, mode)
        .env_remove("NODE_CHANNEL_FD")
        .env_remove("LIBDENO_SPAWNED_IPC")
        .env_remove("NODE_CHANNEL_SERIALIZATION_MODE")
        .env_remove("LIBDENO_TEST_IPC_PROBE")
        .env_remove("LIBDENO_TEST_IPC_FD")
        .env_remove("LIBDENO_TEST_IPC_DEV")
        .env_remove("LIBDENO_TEST_IPC_INO")
        .env_remove("LIBDENO_TEST_IPC_MODE");
    #[cfg(unix)]
    command.process_group(0);
    command
}

fn run_isolated(mode: &str, entry: &std::path::Path) {
    if std::env::var(CHILD_MODE).as_deref() == Ok(mode) {
        let before = std::env::vars_os().collect::<Vec<_>>();
        if mode == "ordinary" {
            assert!(libdeno::run(
                entry,
                &libdeno::LibdenoOptions {
                    allow_all_permissions: true,
                    execution_deadline: Some(Duration::from_secs(10)),
                    ..Default::default()
                }
            )
            .is_ok());
            assert_eq!(before, std::env::vars_os().collect::<Vec<_>>());
        } else {
            // SAFETY: this isolated process starts without a raw channel.
            unsafe { libdeno::initialize_node_ipc_from_env() }.unwrap();
            assert!(unsafe { libdeno::initialize_node_ipc_from_env() }.is_err());
            std::env::set_var("NODE_CHANNEL_FD", "not-a-handle");
            assert!(libdeno::run(
                entry,
                &libdeno::LibdenoOptions {
                    allow_all_permissions: true,
                    execution_deadline: Some(Duration::from_secs(10)),
                    ..Default::default()
                }
            )
            .is_ok());
            assert_eq!(
                before
                    .into_iter()
                    .filter(|(key, _)| key != "NODE_CHANNEL_FD")
                    .collect::<Vec<_>>(),
                std::env::vars_os()
                    .filter(|(key, _)| key != "NODE_CHANNEL_FD")
                    .collect::<Vec<_>>()
            );
        }
        return;
    }

    let mut command = isolated_command(mode);
    if mode == "ordinary" {
        command.env("NODE_CHANNEL_FD", "999999");
    } else {
        command.env_remove("NODE_CHANNEL_FD");
    }
    command.env("LIBDENO_NODE_IPC_ENTRY", entry);
    run_bounded(command);
}

#[test]
fn isolated_node_ipc_process() {
    match std::env::var(CHILD_MODE).as_deref() {
        Ok("legacy-boundary") => {
            #[cfg(unix)]
            run_legacy_child_boundary();
        }
        Ok("fork-chain") => run_fork_chain(),
        Ok("ordinary") | Ok("once") => {
            let entry =
                std::path::PathBuf::from(std::env::var_os("LIBDENO_NODE_IPC_ENTRY").unwrap());
            run_isolated(std::env::var(CHILD_MODE).unwrap().as_str(), &entry);
        }
        _ => {
            let root = temp_root();
            std::fs::create_dir_all(&root).unwrap();
            let noop = root.join("noop.js");
            std::fs::write(&noop, "globalThis.ok = true;").unwrap();
            run_isolated("ordinary", &noop);
            run_isolated("once", &noop);
            let _ = std::fs::remove_dir_all(&root);

            run_bounded(isolated_command("fork-chain"));
        }
    }
}

#[cfg(unix)]
#[test]
fn node_ipc_legacy_fork_to_rust_child_boundary() {
    run_bounded(isolated_command("legacy-boundary"));
}

fn temp_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("libdeno-node-ipc-{}", std::process::id()))
}

#[cfg(unix)]
fn run_legacy_child_boundary() {
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    let child_entry = root.join("child-check.cjs");
    let b_entry = root.join("fork-b.cjs");
    let a_entry = root.join("fork-a.cjs");
    let host = env!("CARGO_BIN_EXE_child_host");
    std::fs::write(
        &child_entry,
        r#"
if (process.env.NODE_CHANNEL_FD !== undefined) throw new Error('NODE_CHANNEL_FD leaked to ordinary child');
if (process.env.NODE_CHANNEL_SERIALIZATION_MODE !== undefined) throw new Error('Node serialization mode leaked to ordinary child');
if (process.env.LIBDENO_SPAWNED_IPC !== undefined) throw new Error('legacy IPC marker leaked to ordinary child');
if (typeof process.send === 'function') throw new Error('ordinary child received process.send');
console.log('IPC_ENV_CLEAN');"#,
    )
    .unwrap();
    std::fs::write(
        &b_entry,
        r#"const trace = (name) => process.stderr.write(`IPC_TRACE pid=${process.pid} mono_ms=${performance.now().toFixed(3)} ${name}\n`);
if (process.argv.includes('--libdeno-test-ipc-second-run')) {
  if (typeof process.send === 'function') throw new Error('consumed startup channel was handed off again');
} else {
  process.on('message', message => {
    if (message !== 'ping') process.exitCode = 1;
    trace('BJS_PING_RECV');
    process.send('pong', error => {
      if (error) { console.error(error); process.exitCode = 1; return; }
      trace('BJS_PONG_CALLBACK');
      process.disconnect();
    });
  });
  trace('BJS_LISTENER_READY');
  process.send('ready', error => { if (error) { console.error(error); process.exitCode = 1; } });
}"#,
    )
    .unwrap();
    std::fs::write(
        &a_entry,
        format!(
            "const {{ fork }} = require('node:child_process');\nconst trace = name => process.stderr.write(`IPC_TRACE pid=${{process.pid}} mono_ms=${{performance.now().toFixed(3)}} ${{name}}\\n`);\nconst child = fork({b_entry:?}, ['--libdeno-test-ipc-spawn-c', {child_entry:?}], {{ execPath: {host:?}, env: {{ PATH: process.env.PATH, LIBDENO_SPAWNED_IPC: '1' }} }});\nlet ready = false; let gotPong = false;\nchild.on('message', message => {{ if (message === 'ready') {{ ready = true; trace('A_READY'); child.send('ping', error => {{ if (error) {{ console.error(error); process.exitCode = 1; }} }}); }} else if (message === 'pong') {{ gotPong = true; trace('A_PONG'); }} else {{ process.exitCode = 1; }} }});\nchild.on('error', error => {{ console.error(error); process.exitCode = 1; }});\nchild.on('exit', code => {{ trace('A_EXIT'); if (code !== 0 || !ready || !gotPong) {{ console.error(`IPC_TRACE pid=${{process.pid}} mono_ms=${{performance.now().toFixed(3)}} A_MISSING_READY_OR_PONG code=${{code}} ready=${{ready}} gotPong=${{gotPong}}`); process.exitCode = 1; }} }});"
            ,
            b_entry = b_entry,
            child_entry = child_entry,
            host = host
        ),
    )
    .unwrap();
    // This runner B is launched by the trusted Node fork launcher and receives
    // a connected IPC endpoint. Its Rust test mode runs ordinary C first.
    unsafe { libdeno::initialize_node_ipc_from_env() }.unwrap();
    let result = libdeno::run(
        &a_entry,
        &libdeno::LibdenoOptions {
            allow_all_permissions: true,
            execution_deadline: Some(Duration::from_secs(25)),
            ..Default::default()
        },
    );
    let _ = std::fs::remove_dir_all(root);
    assert_eq!(result.unwrap(), 0);
}

fn run_fork_chain() {
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    let child = root.join("fork-child.cjs");
    let parent = root.join("fork-parent.cjs");
    let child_host = env!("CARGO_BIN_EXE_child_host");
    std::fs::write(
        &child,
        format!(r#"const {{ fork }} = require('node:child_process');
const trace = name => process.stderr.write(`IPC_TRACE pid=${{process.pid}} mono_ms=${{performance.now().toFixed(3)}} ${{name}}\n`);
if (process.argv.includes('grandchild')) {{
  process.on('message', msg => {{
    if (msg !== 'ping') process.exitCode = 1;
    process.send('pong', error => {{ if (error) process.exitCode = 1; process.disconnect(); }});
  }});
  trace('C_LISTENER_READY');
  process.send('ready');
}} else {{
  process.on('message', msg => {{
    if (msg !== 'ping') {{ process.exitCode = 1; return; }}
    const grandchild = fork({child:?}, ['grandchild'], {{ execPath: {child_host:?}, env: {{ PATH: process.env.PATH }} }});
    grandchild.on('message', reply => {{
      if (reply === 'ready') {{ grandchild.send('ping'); return; }}
      if (reply !== 'pong') {{ process.exitCode = 1; return; }}
      process.send('pong', error => {{ if (error) process.exitCode = 1; process.disconnect(); }});
    }});
    grandchild.on('exit', code => {{ if (code !== 0) process.exitCode = 1; }});
    grandchild.on('error', error => {{ console.error(error); process.exitCode = 1; }});
  }});
  trace('B_LISTENER_READY');
  process.send('ready');
}}"#, child = child, child_host = child_host),
    )
    .unwrap();
    std::fs::write(
        &parent,
        format!(
            "const {{ fork }} = require('node:child_process');\nconst trace = name => process.stderr.write(`IPC_TRACE pid=${{process.pid}} mono_ms=${{performance.now().toFixed(3)}} ${{name}}\\n`);\nconst child = fork({child:?}, [], {{ execPath: {child_host:?}, env: {{ PATH: process.env.PATH }} }});\nlet ready = false; let gotPong = false;\nchild.on('message', msg => {{ if (msg === 'ready') {{ ready = true; trace('A_READY'); child.send('ping'); }} else if (msg === 'pong') {{ gotPong = true; trace('A_PONG'); }} else {{ process.exitCode = 1; }} }});\nchild.on('error', e => {{ console.error(e); process.exitCode = 1; }});\nchild.on('exit', code => {{ trace('A_EXIT'); if (code !== 0 || !ready || !gotPong) process.exitCode = 1; }});"
        ),
    )
    .unwrap();
    unsafe { libdeno::initialize_node_ipc_from_env() }.unwrap();
    let result = libdeno::run(
        &parent,
        &libdeno::LibdenoOptions {
            allow_all_permissions: true,
            execution_deadline: Some(Duration::from_secs(20)),
            ..Default::default()
        },
    );
    let _ = std::fs::remove_dir_all(root);
    assert_eq!(result.unwrap(), 0);
}
