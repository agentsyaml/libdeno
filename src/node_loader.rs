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
use node_resolver::errors::PackageJsonLoadError;

use deno_runtime::deno_permissions::OpenAccessKind;
use deno_runtime::deno_permissions::PermissionsContainer;

/// Source provider for recursive CommonJS analysis (require() chains inside
/// CJS modules that reference further CJS files). Reads are permission-gated
/// like `require()` itself: only fully-granted read permission skips the
/// check, and npm-managed `node_modules` files are always readable.
pub struct FsCjsAnalysisSourceProvider {
    permissions: PermissionsContainer,
}

impl FsCjsAnalysisSourceProvider {
    pub fn new(permissions: PermissionsContainer) -> Self {
        Self { permissions }
    }
}

impl node_resolver::analyze::CjsAnalysisSourceProvider for FsCjsAnalysisSourceProvider {
    fn load_source<'a>(&'a self, specifier: &ModuleSpecifier) -> Option<Cow<'a, str>> {
        let path = specifier.to_file_path().ok()?;
        if !self.permissions.query_read_all()
            && !path.components().any(|c| c.as_os_str() == "node_modules")
            && self
                .permissions
                .check_open(
                    Cow::Borrowed(&path),
                    OpenAccessKind::Read,
                    Some("CJS analysis"),
                )
                .is_err()
        {
            return None;
        }
        std::fs::read_to_string(path).ok().map(Cow::Owned)
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
}

impl SimpleNodeRequireLoader {
    pub fn new(
        cjs_tracker: deno_resolver::cjs::CjsTrackerRc<
            deno_resolver::npm::DenoInNpmPackageChecker,
            sys_traits::impls::RealSys,
        >,
    ) -> Self {
        Self { cjs_tracker }
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
        // npm-managed files under node_modules are always readable.
        if path.components().any(|c| c.as_os_str() == "node_modules") {
            return Ok(path);
        }
        permissions
            .check_open(path, OpenAccessKind::Read, Some("require"))
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
