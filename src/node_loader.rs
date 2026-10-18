// Node compat require loader and the CJS analysis source provider.
//
// CJS detection delegates to the official CJS tracker (package.json "type"
// aware). Reads honor an installed broker; without one, fully granted reads
// skip checks and npm-managed `node_modules` files are trusted.

use std::borrow::Cow;
use std::path::Path;

use deno_core::FastString;
use deno_core::ModuleSpecifier;
use deno_error::JsErrorBox;
use deno_media_type::MediaType;
use deno_resolver::npm::DenoInNpmPackageChecker;
use node_resolver::errors::PackageJsonLoadError;
// node_resolver's `npm` module is private; the trait is re-exported at the
// crate root.
use node_resolver::InNpmPackageChecker;

use deno_runtime::deno_permissions::OpenAccessKind;
use deno_runtime::deno_permissions::PermissionsContainer;

/// Source provider for recursive CommonJS analysis (require() chains inside
/// CJS modules that reference further CJS files). Reads are permission-gated
/// like `require()` itself: only fully-granted read permission without a broker
/// skips the check, and npm files are exempt only without a broker.
pub struct FsCjsAnalysisSourceProvider {
    permissions: PermissionsContainer,
    in_npm_pkg_checker: DenoInNpmPackageChecker,
}

impl FsCjsAnalysisSourceProvider {
    pub fn new(
        permissions: PermissionsContainer,
        in_npm_pkg_checker: DenoInNpmPackageChecker,
    ) -> Self {
        Self {
            permissions,
            in_npm_pkg_checker,
        }
    }
}

impl node_resolver::analyze::CjsAnalysisSourceProvider for FsCjsAnalysisSourceProvider {
    fn load_source<'a>(&'a self, specifier: &ModuleSpecifier) -> Option<Cow<'a, str>> {
        let path = specifier.to_file_path().ok()?;
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let broker_installed = crate::permission_broker::is_installed();
        let read_path = if !broker_installed && self.permissions.query_read_all() {
            canonical
        } else {
            // Canonicalize first: the exemption must test the *resolved* path
            // so `..`/symlink traversal can't smuggle a non-npm file past the
            // check. Read the path returned by check_open as well, rather than
            // switching back to the original path after the permission check.
            let in_npm_package = ModuleSpecifier::from_file_path(&canonical)
                .ok()
                .map(|url| self.in_npm_pkg_checker.in_npm_package(&url))
                .unwrap_or(false);
            if in_npm_package && !broker_installed {
                canonical
            } else {
                self.permissions
                    .check_open(
                        Cow::Owned(canonical),
                        OpenAccessKind::Read,
                        Some("CJS analysis"),
                    )
                    .ok()?
                    .into_owned_path()
            }
        };
        std::fs::read_to_string(read_path).ok().map(Cow::Owned)
    }
}

/// Node compat require loader. CJS detection delegates to the official CJS
/// tracker (package.json "type" aware). Reads honor an installed broker;
/// without one, fully granted reads and npm-managed `node_modules` files can
/// bypass local checks, while other paths must satisfy `--allow-read`.
pub struct SimpleNodeRequireLoader {
    cjs_tracker: deno_resolver::cjs::CjsTrackerRc<
        deno_resolver::npm::DenoInNpmPackageChecker,
        sys_traits::impls::RealSys,
    >,
    in_npm_pkg_checker: DenoInNpmPackageChecker,
    permissions: PermissionsContainer,
}

impl SimpleNodeRequireLoader {
    pub fn new(
        cjs_tracker: deno_resolver::cjs::CjsTrackerRc<
            deno_resolver::npm::DenoInNpmPackageChecker,
            sys_traits::impls::RealSys,
        >,
        in_npm_pkg_checker: DenoInNpmPackageChecker,
        permissions: PermissionsContainer,
    ) -> Self {
        Self {
            cjs_tracker,
            in_npm_pkg_checker,
            permissions,
        }
    }
}

impl deno_runtime::deno_node::NodeRequireLoader for SimpleNodeRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        permissions: &mut deno_runtime::deno_permissions::PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        // Only bypass checks when no installed broker is the permission authority.
        let broker_installed = crate::permission_broker::is_installed();
        if !broker_installed && permissions.query_read_all() {
            return Ok(path);
        }
        // Without a broker, npm-managed files under the real node_modules root
        // are exempt. Canonicalize FIRST: in_npm_package is a URL-prefix test
        // against the canonicalized root, so an un-resolved path with `..`
        // components would pass it and escape the root.
        let canonical =
            std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf());
        let in_npm_package = ModuleSpecifier::from_file_path(&canonical)
            .ok()
            .map(|url| self.in_npm_pkg_checker.in_npm_package(&url))
            .unwrap_or(false);
        if in_npm_package && !broker_installed {
            return Ok(Cow::Owned(canonical));
        }
        permissions
            .check_open(Cow::Owned(canonical), OpenAccessKind::Read, Some("require"))
            .map(|checked| checked.into_path())
            .map_err(JsErrorBox::from_err)
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        let path = if crate::permission_broker::is_installed() {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            self.permissions
                .check_open(Cow::Owned(canonical), OpenAccessKind::Read, Some("require"))
                .map_err(JsErrorBox::from_err)?
                .into_owned_path()
        } else {
            path.to_path_buf()
        };
        std::fs::read_to_string(&path)
            .map(Into::into)
            .map_err(|e| JsErrorBox::generic(format!("Failed to read {path:?}: {e}")))
    }

    fn is_maybe_cjs(&self, specifier: &url::Url) -> Result<bool, PackageJsonLoadError> {
        self.cjs_tracker
            .is_maybe_cjs(specifier, MediaType::from_specifier(specifier))
    }

    fn is_maybe_cjs_from_require(
        &self,
        specifier: &url::Url,
    ) -> Result<bool, PackageJsonLoadError> {
        self.cjs_tracker
            .is_maybe_cjs_from_require(specifier, MediaType::from_specifier(specifier))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use node_resolver::analyze::CjsAnalysisSourceProvider;
    use std::sync::Arc;

    #[cfg(unix)]
    #[test]
    fn cjs_analysis_reads_the_canonical_checked_path() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("libdeno-cjs-canonical-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let target = dir.join("target.cjs");
        let link = dir.join("link.cjs");
        std::fs::write(&target, "module.exports = 'canonical';").unwrap();
        symlink(&target, &link).unwrap();

        let parser = Arc::new(
            deno_runtime::deno_permissions::RuntimePermissionDescriptorParser::new(
                sys_traits::impls::RealSys,
            ),
        );
        let permissions = crate::permissions::build_permissions(
            &[format!("--allow-read={}", target.display())],
            false,
            false,
            parser,
            &dir,
        )
        .unwrap();
        let provider = FsCjsAnalysisSourceProvider::new(
            permissions,
            DenoInNpmPackageChecker::new(deno_resolver::npm::CreateInNpmPkgCheckerOptions::Byonm),
        );
        let specifier = ModuleSpecifier::from_file_path(&link).unwrap();

        assert_eq!(
            provider.load_source(&specifier).unwrap().as_ref(),
            "module.exports = 'canonical';"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn cjs_analysis_source_provider_checks_installed_broker_inside_npm_package() {
        const CHILD: &str = "LIBDENO_NODE_LOADER_BROKER_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run_cjs_analysis_broker_case();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "node_loader::tests::cjs_analysis_source_provider_checks_installed_broker_inside_npm_package",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated broker test failed: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated broker test exceeded 10 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn run_cjs_analysis_broker_case() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir =
            std::env::temp_dir().join(format!("libdeno-analysis-broker-{}", std::process::id()));
        let package = dir.join("node_modules/pkg");
        std::fs::create_dir_all(&package).unwrap();
        let target = package.join("index.cjs");
        std::fs::write(&target, "module.exports = 'secret';").unwrap();
        let target = std::fs::canonicalize(target).unwrap();
        let server_target = target.clone();
        let socket = dir.join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: deno_core::serde_json::Value =
                deno_core::serde_json::from_str(line.trim()).unwrap();
            assert_eq!(
                request["value"].as_str(),
                Some(server_target.to_str().unwrap())
            );
            writeln!(stream, "{{\"id\":{},\"result\":\"deny\"}}", request["id"]).unwrap();
        });

        crate::install_permission_broker(&socket).unwrap();
        let parser = Arc::new(
            deno_runtime::deno_permissions::RuntimePermissionDescriptorParser::new(
                sys_traits::impls::RealSys,
            ),
        );
        let permissions = crate::permissions::build_permissions(
            &[format!("--allow-read={}", dir.display())],
            false,
            false,
            parser,
            &dir,
        )
        .unwrap();
        assert!(!permissions.query_read_all());
        let provider = FsCjsAnalysisSourceProvider::new(
            permissions,
            DenoInNpmPackageChecker::new(deno_resolver::npm::CreateInNpmPkgCheckerOptions::Byonm),
        );
        let specifier = ModuleSpecifier::from_file_path(&target).unwrap();
        assert!(provider.in_npm_pkg_checker.in_npm_package(&specifier));
        assert!(provider.load_source(&specifier).is_none());
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
