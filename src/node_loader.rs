// Node compat require loader and the CJS analysis source provider.
//
// CJS detection delegates to the official CJS tracker (package.json "type"
// aware). Reads are permission-checked: fully granted read permission bypasses
// the check, npm-managed `node_modules` files are trusted, and everything
// else must satisfy the declared `--allow-read` restrictions (mirroring
// deno_lib's npm permission checker).

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
/// like `require()` itself: only fully-granted read permission skips the
/// check, and npm-managed `node_modules` files are always readable.
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
        let read_path = if self.permissions.query_read_all() {
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
            if in_npm_package {
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
/// tracker (package.json "type" aware). Reads are permission-checked: fully
/// granted read permission bypasses the check, npm-managed `node_modules`
/// files are trusted, and everything else must satisfy the declared
/// `--allow-read` restrictions (mirroring deno_lib's npm permission checker).
pub struct SimpleNodeRequireLoader {
    cjs_tracker: deno_resolver::cjs::CjsTrackerRc<
        deno_resolver::npm::DenoInNpmPackageChecker,
        sys_traits::impls::RealSys,
    >,
    in_npm_pkg_checker: DenoInNpmPackageChecker,
}

impl SimpleNodeRequireLoader {
    pub fn new(
        cjs_tracker: deno_resolver::cjs::CjsTrackerRc<
            deno_resolver::npm::DenoInNpmPackageChecker,
            sys_traits::impls::RealSys,
        >,
        in_npm_pkg_checker: DenoInNpmPackageChecker,
    ) -> Self {
        Self {
            cjs_tracker,
            in_npm_pkg_checker,
        }
    }
}

impl deno_runtime::deno_node::NodeRequireLoader for SimpleNodeRequireLoader {
    fn ensure_read_permission<'a>(
        &self,
        permissions: &mut deno_runtime::deno_permissions::PermissionsContainer,
        path: Cow<'a, Path>,
    ) -> Result<Cow<'a, Path>, JsErrorBox> {
        // Fully granted read permission needs no check, like deno_lib.
        if permissions.query_read_all() {
            return Ok(path);
        }
        // npm-managed files under the real node_modules root are always
        // readable. Canonicalize FIRST: in_npm_package is a URL-prefix test
        // against the canonicalized root, so an un-resolved path with `..`
        // components would pass it and escape the root.
        let canonical =
            std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf());
        let in_npm_package = ModuleSpecifier::from_file_path(&canonical)
            .ok()
            .map(|url| self.in_npm_pkg_checker.in_npm_package(&url))
            .unwrap_or(false);
        if in_npm_package {
            return Ok(Cow::Owned(canonical));
        }
        permissions
            .check_open(Cow::Owned(canonical), OpenAccessKind::Read, Some("require"))
            .map(|checked| checked.into_path())
            .map_err(JsErrorBox::from_err)
    }

    fn load_text_file_lossy(&self, path: &Path) -> Result<FastString, JsErrorBox> {
        std::fs::read_to_string(path)
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
}
