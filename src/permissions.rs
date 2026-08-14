// Permission parsing: turns `--allow-*` capability strings into a
// `PermissionsContainer`, mirroring the Deno CLI's flag semantics.

use std::path::Path;
use std::sync::Arc;

use deno_runtime::deno_permissions::PermissionsContainer;
use sys_traits::impls::RealSys;

use crate::LibdenoError;

/// Builds the permission container from `--allow-*` capability strings.
///
/// The default stance is opt-in: an empty `permission_args` list is a
/// construction error (`LibdenoError::Permission`) unless `allow_all` is set
/// (equivalent to the CLI's `-A`), which grants every capability. The
/// `-A`/`--allow-all` strings in `permission_args` remain valid and are
/// equivalent to setting `allow_all`. Passing any other `--allow-*` flag
/// restricts the runtime to the declared capabilities, e.g. `--allow-read=.`
/// only allows reads under `.`. A flag without a value allows that capability
/// globally (`--allow-read` == read anywhere); with a comma-separated value
/// only the listed descriptors are allowed (`--allow-read=./src,./public`).
///
/// `prompt` mirrors `deno run`'s default interactive mode. The three
/// combinations:
///
/// - `prompt: false` + empty args: construction error (the v0.2.0 default).
/// - `prompt: true` + empty args: no error — every capability starts in the
///   Prompt state, so each access is asked interactively (`deno run` with no
///   `--allow-*` flags).
/// - `prompt: true` + flags: the flags grant, everything else is asked.
///
/// With `prompt: false`, anything not granted is denied.
///
/// Relative path values (`--allow-read=./src`) resolve against `cwd` (the
/// `LibdenoOptions.cwd` working directory), not the host process's current
/// directory, so the declared scope is stable regardless of where the
/// embedder runs from.
pub fn build_permissions(
    permission_args: &[String],
    allow_all: bool,
    prompt: bool,
    parser: Arc<deno_runtime::deno_permissions::RuntimePermissionDescriptorParser<RealSys>>,
    cwd: &Path,
) -> Result<PermissionsContainer, LibdenoError> {
    use deno_runtime::deno_permissions::Permissions;
    use deno_runtime::deno_permissions::PermissionsOptions;

    if allow_all {
        return Ok(PermissionsContainer::allow_all(parser));
    }

    let mut opts = PermissionsOptions {
        allow_env: None,
        deny_env: None,
        ignore_env: None,
        allow_net: None,
        deny_net: None,
        allow_ffi: None,
        deny_ffi: None,
        allow_read: None,
        deny_read: None,
        ignore_read: None,
        allow_run: None,
        deny_run: None,
        allow_sys: None,
        deny_sys: None,
        allow_write: None,
        deny_write: None,
        allow_import: None,
        deny_import: None,
        prompt,
    };
    let mut has_allow = false;
    for arg in permission_args {
        let (flag, value) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (arg.as_str(), None),
        };
        if flag == "-A" || flag == "--allow-all" {
            // Explicit allow-all: keep the default. A value is rejected — it
            // would otherwise silently grant everything.
            if value.is_some() {
                return Err(LibdenoError::Permission(format!(
                    "flag `{flag}` does not take a value"
                )));
            }
            return Ok(PermissionsContainer::allow_all(parser));
        }
        // `--allow-read` (no value) means global; `--allow-read=` (empty value)
        // is rejected — it would otherwise silently parse as a global grant.
        let specs: Vec<String> = match value {
            Some(v) => {
                let specs: Vec<String> = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if specs.is_empty() {
                    return Err(LibdenoError::Permission(format!(
                        "flag `{flag}=` requires at least one value"
                    )));
                }
                specs
            }
            None => vec![],
        };
        // read/write/ffi take filesystem paths; resolve relative ones against
        // `cwd` so the permission scope tracks LibdenoOptions.cwd, not process cwd.
        let paths = |specs: Vec<String>| -> Vec<String> {
            specs
                .into_iter()
                .map(|s| {
                    let p = Path::new(&s);
                    if p.is_absolute() {
                        s
                    } else {
                        cwd.join(p).to_string_lossy().into_owned()
                    }
                })
                .collect()
        };
        // Repeated flags accumulate (union), like the CLI: `--allow-read=./a
        // --allow-read=./b` grants both, not just the last.
        let extend = |slot: &mut Option<Vec<String>>, specs: Vec<String>| {
            slot.get_or_insert_with(Vec::new).extend(specs);
        };
        match flag {
            "--allow-read" => {
                has_allow = true;
                extend(&mut opts.allow_read, paths(specs));
            }
            "--allow-write" => {
                has_allow = true;
                extend(&mut opts.allow_write, paths(specs));
            }
            "--allow-env" => {
                has_allow = true;
                extend(&mut opts.allow_env, specs);
            }
            "--allow-net" => {
                has_allow = true;
                extend(&mut opts.allow_net, specs);
            }
            "--allow-run" => {
                has_allow = true;
                extend(&mut opts.allow_run, specs);
            }
            "--allow-ffi" => {
                has_allow = true;
                extend(&mut opts.allow_ffi, paths(specs));
            }
            "--allow-sys" => {
                has_allow = true;
                extend(&mut opts.allow_sys, specs);
            }
            "--allow-import" => {
                has_allow = true;
                // Values are import descriptor strings (hosts or host:port,
                // mirroring --allow-net; full URLs like `https://…` are
                // rejected by the upstream parser, same as the CLI). Without a
                // value: import access is granted globally.
                extend(&mut opts.allow_import, specs);
            }
            _ => {
                return Err(LibdenoError::Permission(format!(
                    "unknown permission flag: {flag}"
                )))
            }
        }
    }
    if !has_allow {
        if prompt {
            // Empty list + prompt: every capability starts in Prompt state,
            // exactly like `deno run` with no --allow flags — each access is
            // asked interactively instead of erroring or granting.
            let perms = Permissions::from_options(&*parser, &opts)
                .map_err(|e| LibdenoError::Permission(e.to_string()))?;
            return Ok(PermissionsContainer::new(parser, perms));
        }
        return Err(LibdenoError::Configuration(
            "no permission flags provided; since v0.2.0 an empty list grants \
             nothing (it was allow-all before v0.2.0). Pass --allow-* capability \
             flags, set LibdenoOptions.prompt = true for interactive prompting, \
             or set LibdenoOptions.allow_all_permissions = true to grant all \
             capabilities"
                .to_string(),
        ));
    }
    let perms = Permissions::from_options(&*parser, &opts)
        .map_err(|e| LibdenoError::Permission(e.to_string()))?;
    Ok(PermissionsContainer::new(parser, perms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_runtime::deno_permissions::PermissionState;

    fn parser() -> Arc<deno_runtime::deno_permissions::RuntimePermissionDescriptorParser<RealSys>> {
        Arc::new(deno_runtime::deno_permissions::RuntimePermissionDescriptorParser::new(RealSys))
    }

    fn perms(args: &[&str], allow_all: bool, prompt: bool) -> PermissionsContainer {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        build_permissions(
            &args,
            allow_all,
            prompt,
            parser(),
            std::env::current_dir().unwrap().as_path(),
        )
        .unwrap()
    }

    #[test]
    fn empty_permissions_without_opt_in_are_rejected() {
        let args: Vec<String> = vec![];
        let err = build_permissions(
            &args,
            false,
            false,
            parser(),
            std::env::current_dir().unwrap().as_path(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LibdenoError::Configuration(_)),
            "expected configuration error, got {err:?}"
        );
    }

    #[test]
    fn empty_permissions_with_prompt_are_all_prompt_state() {
        // Empty list + prompt: no construction error — every capability starts
        // in the Prompt state, so each access is asked interactively (the
        // v0.2.0 empty-list error does not apply with prompt: true).
        let p = perms(&[], false, true);
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Prompt
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn prompt_keeps_flag_grants_and_prompts_the_rest() {
        // Flags still grant with prompt: true; everything outside them is left
        // in the Prompt state for interactive asking instead of being denied.
        let p = perms(&["--allow-read=./src"], false, true);
        assert_eq!(
            p.query_read(Some("./src/lib.rs")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("./target/x")).unwrap(),
            PermissionState::Prompt
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn allow_all_opt_in_grants_everything() {
        let p = perms(&[], true, false);
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(p.query_env(Some("HOME")), PermissionState::Granted);
    }

    #[test]
    fn allow_all_opt_in_wins_over_flags() {
        // The opt-in is checked before flag parsing: with allow_all set, any
        // flags are ignored and everything is granted (documented precedence,
        // so embedders cannot accidentally combine opt-in with restrictions).
        let p = perms(&["--allow-read=./src"], true, false);
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Granted
        );
    }

    #[test]
    fn allow_all_flag_grants_everything() {
        let p = perms(&["-A"], false, false);
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Granted
        );
    }

    #[test]
    fn allow_all_with_value_is_rejected() {
        for bad in ["-A=anything", "--allow-all=x"] {
            let args = vec![bad.to_string()];
            assert!(
                build_permissions(
                    &args,
                    false,
                    false,
                    parser(),
                    std::env::current_dir().unwrap().as_path()
                )
                .is_err(),
                "`{bad}` must be rejected, not grant allow-all"
            );
        }
    }

    #[test]
    fn allow_import_flag_restricts_to_hosts() {
        // The flag restores remote-module import gating: values are host-style
        // import descriptors (like --allow-net), not full URLs — the upstream
        // parser rejects URL schemes, matching the deno CLI.
        let p = perms(&["--allow-import=jsr.io"], false, false);
        assert_eq!(
            p.query_import(Some("jsr.io")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_import(Some("deno.land")).unwrap(),
            PermissionState::Prompt
        );
        // Remote module loading has NO --allow-net fallback (upstream checks
        // only the import permission), so a bare --allow-net grant leaves
        // import in the Prompt/deny state.
        let p = perms(&["--allow-net"], false, false);
        assert_eq!(
            p.query_import(Some("deno.land")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn allow_import_without_value_grants_globally() {
        let p = perms(&["--allow-import"], false, false);
        assert_eq!(
            p.query_import(Some("any.host.example")).unwrap(),
            PermissionState::Granted
        );
        // Other capabilities stay restricted: allow-import is not allow-all.
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn explicit_flags_restrict_read_to_paths() {
        let p = perms(&["--allow-read=./src,./public"], false, false);
        assert_eq!(
            p.query_read(Some("./src/lib.rs")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("./public/index.html")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("./target/x")).unwrap(),
            PermissionState::Prompt
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn flag_without_value_allows_capability_globally() {
        let p = perms(&["--allow-net"], false, false);
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("/etc/passwd")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn net_flag_restricts_to_hosts() {
        let p = perms(&["--allow-net=example.com:8080"], false, false);
        assert_eq!(
            p.query_net(Some("example.com:8080")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_net(Some("example.com")).unwrap(),
            PermissionState::Prompt
        );
        assert_eq!(
            p.query_net(Some("other.com")).unwrap(),
            PermissionState::Prompt
        );
    }

    #[test]
    fn env_flag_restricts_to_names() {
        let p = perms(&["--allow-env=HOME,PATH"], false, false);
        assert_eq!(p.query_env(Some("HOME")), PermissionState::Granted);
        assert_eq!(p.query_env(Some("PATH")), PermissionState::Granted);
        assert_eq!(p.query_env(Some("SECRET")), PermissionState::Prompt);
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let args = vec!["--allow-read=.".to_string(), "--bogus-flag".to_string()];
        let err = build_permissions(
            &args,
            false,
            false,
            parser(),
            std::env::current_dir().unwrap().as_path(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LibdenoError::Permission(_)),
            "expected permission error, got {err:?}"
        );
    }

    #[test]
    fn unknown_flag_alone_does_not_allow_everything() {
        // A typo like `--allow-raed=/etc` must error, not silently fall back to
        // allow-all (the previous behavior).
        let args = vec!["--allow-raed=/etc".to_string()];
        assert!(build_permissions(
            &args,
            false,
            false,
            parser(),
            std::env::current_dir().unwrap().as_path()
        )
        .is_err());
    }

    #[test]
    fn empty_flag_value_is_rejected() {
        let args = vec!["--allow-read=".to_string()];
        assert!(build_permissions(
            &args,
            false,
            false,
            parser(),
            std::env::current_dir().unwrap().as_path()
        )
        .is_err());
    }

    #[test]
    fn repeated_flags_accumulate() {
        let p = perms(
            &["--allow-read=./src", "--allow-read=./public"],
            false,
            false,
        );
        assert_eq!(
            p.query_read(Some("./src/lib.rs")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("./public/index.html")).unwrap(),
            PermissionState::Granted
        );
        assert_eq!(
            p.query_read(Some("./other/x")).unwrap(),
            PermissionState::Prompt
        );
    }
}
