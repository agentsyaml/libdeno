// Permission parsing: turns `--allow-*` capability strings into a
// `PermissionsContainer`, mirroring the Deno CLI's flag semantics.

use std::path::Path;
use std::sync::Arc;

use deno_runtime::deno_permissions::PermissionsContainer;
use sys_traits::impls::RealSys;

use crate::LibdenoError;

/// Builds the permission container from `--allow-*` capability strings.
///
/// An empty list allows everything (the default, matching the CLI's `-A`).
/// Passing any `--allow-*` flag restricts the runtime to the declared
/// capabilities, e.g. `--allow-read=.` only allows reads under `.`. A flag
/// without a value allows that capability globally (`--allow-read` == read
/// anywhere); with a comma-separated value only the listed descriptors are
/// allowed (`--allow-read=./src,./public`).
///
/// Relative path values (`--allow-read=./src`) resolve against `cwd` (the
/// `LibdenoOptions.cwd` working directory), not the host process's current
/// directory, so the declared scope is stable regardless of where the
/// embedder runs from.
pub fn build_permissions(
    permission_args: &[String],
    parser: Arc<deno_runtime::deno_permissions::RuntimePermissionDescriptorParser<RealSys>>,
    cwd: &Path,
) -> Result<PermissionsContainer, LibdenoError> {
    use deno_runtime::deno_permissions::Permissions;
    use deno_runtime::deno_permissions::PermissionsOptions;

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
        prompt: false,
    };
    let mut has_allow = false;
    for arg in permission_args {
        let (flag, value) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (arg.as_str(), None),
        };
        if flag == "-A" || flag == "--allow-all" {
            // Explicit allow-all: keep the default.
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
        return Ok(PermissionsContainer::allow_all(parser));
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

    fn perms(args: &[&str]) -> PermissionsContainer {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        build_permissions(&args, parser(), std::env::current_dir().unwrap().as_path()).unwrap()
    }

    #[test]
    fn empty_permissions_allow_everything() {
        let p = perms(&[]);
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
    fn allow_all_flag_grants_everything() {
        let p = perms(&["-A"]);
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
    fn explicit_flags_restrict_read_to_paths() {
        let p = perms(&["--allow-read=./src,./public"]);
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
        let p = perms(&["--allow-net"]);
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
        let p = perms(&["--allow-net=example.com:8080"]);
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
        let p = perms(&["--allow-env=HOME,PATH"]);
        assert_eq!(p.query_env(Some("HOME")), PermissionState::Granted);
        assert_eq!(p.query_env(Some("PATH")), PermissionState::Granted);
        assert_eq!(p.query_env(Some("SECRET")), PermissionState::Prompt);
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let args = vec!["--allow-read=.".to_string(), "--bogus-flag".to_string()];
        let err = build_permissions(&args, parser(), std::env::current_dir().unwrap().as_path())
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
        assert!(
            build_permissions(&args, parser(), std::env::current_dir().unwrap().as_path()).is_err()
        );
    }

    #[test]
    fn empty_flag_value_is_rejected() {
        let args = vec!["--allow-read=".to_string()];
        assert!(
            build_permissions(&args, parser(), std::env::current_dir().unwrap().as_path()).is_err()
        );
    }

    #[test]
    fn repeated_flags_accumulate() {
        let p = perms(&["--allow-read=./src", "--allow-read=./public"]);
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
