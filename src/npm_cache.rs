// Process-level npm resolution snapshot cache.
//
// A managed npm project without a deno.lock re-resolves its dependency graph
// from scratch on every run. Keep the last resolved snapshot in-process, but
// key it only with resolver-owned, credential-free input identity captured
// during the same construction attempt.
//
// Projects with a deno.lock are never cached here: they already reuse the
// on-disk lockfile via ResolveFromLockfile.

use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(feature = "npm", test))]
use std::sync::Arc;
#[cfg(any(feature = "npm", test))]
use std::sync::Mutex;
#[cfg(any(feature = "npm", test))]
use std::sync::OnceLock;

#[cfg(any(feature = "npm", test))]
use deno_npm::resolution::ValidSerializedNpmResolutionSnapshot;
use deno_npmrc::ReplaceRegistryHost;
use deno_resolver::factory::ResolverFactory;
use deno_resolver::factory::WorkspaceFactory;
use sys_traits::impls::RealSys;
use sys_traits::EnvHomeDir;
use url::Url;

/// Serializes the tests that mutate process-global resolver inputs (`HOME`, and
/// the `$HOME/.npmrc` it selects). `SharedServices::new` re-reads those inputs
/// to detect a manifest that changed mid-construction, so a concurrent mutation
/// makes an unrelated build look unstable. Shared with the `stable_builder_*`
/// tests in `services`, which construct a real resolver stack.
#[cfg(test)]
static RESOLVER_ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn resolver_env_test_lock() -> &'static Mutex<()> {
    RESOLVER_ENV_TEST_LOCK.get_or_init(|| Mutex::new(()))
}

const DENO_CONFIG_FILE_NAMES: [&str; 2] = ["deno.json", "deno.jsonc"];
const MANIFEST_CANDIDATE_FILE_NAMES: [&str; 3] = [
    DENO_CONFIG_FILE_NAMES[0],
    DENO_CONFIG_FILE_NAMES[1],
    "package.json",
];

/// The parsed resolver inputs and a comparable filesystem probe captured for
/// one construction attempt. The key uses only `identity`; `probe` is kept
/// separately so a file that changes between resolver construction and key
/// publication causes a retry instead of producing a key for a different
/// file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolverInputManifest {
    identity: ResolverInputIdentity,
    probe: ResolverInputProbe,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ResolverInputIdentity {
    initial_cwd: PathBuf,
    workspace_root: PathBuf,
    discovery_dir: PathBuf,
    members: Vec<PathBuf>,
    links: Vec<PathBuf>,
    configs: Vec<SemanticFingerprint>,
    external_configs: Vec<SemanticFingerprint>,
    packages: Vec<SemanticFingerprint>,
    npmrc: NpmConfigFingerprint,
    lockfile: Option<PathBuf>,
    lockfile_present: bool,
    byonm: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolverInputProbe {
    files: Vec<FileProbe>,
    byonm_node_modules: Vec<DirectoryProbe>,
    managed_root_node_modules: Option<RootPathProbe>,
    npmrc_has_auth: bool,
    environment: EnvironmentProbe,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SemanticFingerprint {
    path: PathBuf,
    hash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NpmConfigFingerprint {
    default_registry: String,
    scoped_registries: Vec<(String, String)>,
    registry_configs: Vec<String>,
    replace_registry_host: String,
    min_release_age_days: Option<u64>,
    trust_policy_no_downgrade: bool,
    trust_policy_ignore_after_minutes: Option<u64>,
    trust_policy_exclude: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileProbe {
    path: PathBuf,
    kind: FileProbeKind,
    fingerprint: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FileProbeKind {
    /// Config/package bytes are probed only for construction stability. The
    /// cache key uses the parsed semantic value instead; lockfiles are probed
    /// by content because they are not part of the snapshot key.
    Content,
    /// Authenticated npmrc contents are never hashed because npmrc may contain
    /// credentials. Auth-free npmrc files and lockfiles use Content so
    /// same-size edits cannot be missed.
    Metadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryProbe {
    path: PathBuf,
    fingerprint: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootPathProbe {
    path: PathBuf,
    identity: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentProbe {
    deno_dir: Option<std::ffi::OsString>,
    no_package_json: bool,
    jsr_url: Option<String>,
    jsr_url_has_auth: bool,
    registry: Option<String>,
    registry_has_auth: bool,
    replace_registry_host: Option<String>,
    replace_registry_host_has_auth: bool,
    min_release_age: Option<String>,
    global_npmrc: Option<PathBuf>,
    referenced_npmrc_vars: Vec<EnvironmentValueProbe>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EnvironmentValueProbe {
    name: String,
    fingerprint: Option<(u64, u64)>,
}

/// Immutable identity captured from the resolver that owns a managed npm
/// resolution. It contains no npmrc source, token, auth flag, certificate
/// path, or private URL userinfo.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg(any(feature = "npm", test))]
pub(crate) struct ManagedNpmSnapshotKey {
    identity: ResolverInputIdentity,
}

pub(crate) const RESOLVER_INPUTS_CHANGED: &str = "resolver inputs changed during construction";

#[cfg(test)]
type SemanticProbeTestHook = Box<dyn Fn() + Send>;

#[cfg(test)]
static SEMANTIC_PROBE_TEST_HOOK: OnceLock<
    Mutex<Option<(std::thread::ThreadId, SemanticProbeTestHook)>>,
> = OnceLock::new();

#[cfg(test)]
static SEMANTIC_PROBE_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn semantic_probe_test_lock() -> &'static Mutex<()> {
    SEMANTIC_PROBE_TEST_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn set_semantic_probe_test_hook(hook: impl Fn() + Send + 'static) {
    let hooks = SEMANTIC_PROBE_TEST_HOOK.get_or_init(|| Mutex::new(None));
    *hooks.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((std::thread::current().id(), Box::new(hook)));
}

#[cfg(test)]
pub(crate) fn clear_semantic_probe_test_hook() {
    let hooks = SEMANTIC_PROBE_TEST_HOOK.get_or_init(|| Mutex::new(None));
    *hooks.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
fn invoke_semantic_probe_test_hook() {
    let hooks = SEMANTIC_PROBE_TEST_HOOK.get_or_init(|| Mutex::new(None));
    let current_thread = std::thread::current().id();
    if let Some((owner, hook)) = hooks.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if owner == &current_thread {
            hook();
        }
    }
}

/// Builds a manifest from the actual memoized workspace and resolver factory.
pub(crate) fn resolver_input_manifest(
    initial_cwd: PathBuf,
    config_start_paths: Vec<PathBuf>,
    workspace_factory: &WorkspaceFactory<RealSys>,
    resolver_factory: &ResolverFactory<RealSys>,
) -> deno_core::anyhow::Result<ResolverInputManifest> {
    let workspace_directory = workspace_factory.workspace_directory()?;
    let (npmrc, npmrc_path) = workspace_factory.npmrc_with_path()?;
    let (byonm, node_modules_path) =
        crate::deno_resolver_adapter::resolver_byonm_and_root_node_modules_path(resolver_factory)?;
    let managed_root_path = crate::deno_resolver_adapter::managed_root_node_modules_path(
        workspace_factory,
        resolver_factory,
        &initial_cwd,
    )?;
    let manifest = manifest_from_workspace(
        initial_cwd.clone(),
        config_start_paths.clone(),
        workspace_directory,
        npmrc,
        WorkspaceResolverInputs {
            npmrc_path: npmrc_path.as_deref(),
            byonm,
            node_modules_path: node_modules_path.as_deref(),
            managed_root_path: managed_root_path.as_deref(),
        },
    )?;

    // The semantic values above came from memoized discovery. Reparse through
    // a fresh authoritative factory before accepting the probe/key pair; this
    // closes the semantic-extraction -> raw-probe window.
    let authoritative_factory = crate::deno_resolver_adapter::new_authoritative_workspace_factory(
        initial_cwd.clone(),
        config_start_paths.clone(),
        workspace_factory,
    )?;
    let authoritative_workspace = authoritative_factory.workspace_directory()?;
    let (authoritative_npmrc, authoritative_npmrc_path) =
        authoritative_factory.npmrc_with_path()?;
    let authoritative_resolver = crate::deno_resolver_adapter::new_resolver_factory(
        authoritative_factory.clone(),
        crate::analysis_cache::node_analysis_cache(),
    );
    let (authoritative_byonm, authoritative_node_modules_path) =
        crate::deno_resolver_adapter::resolver_byonm_and_root_node_modules_path(
            &authoritative_resolver,
        )?;
    let authoritative_managed_root_path =
        crate::deno_resolver_adapter::managed_root_node_modules_path(
            &authoritative_factory,
            &authoritative_resolver,
            &initial_cwd,
        )?;
    let authoritative = manifest_from_workspace(
        initial_cwd,
        config_start_paths,
        authoritative_workspace,
        authoritative_npmrc,
        WorkspaceResolverInputs {
            npmrc_path: authoritative_npmrc_path.as_deref(),
            byonm: authoritative_byonm,
            node_modules_path: authoritative_node_modules_path.as_deref(),
            managed_root_path: authoritative_managed_root_path.as_deref(),
        },
    )?;
    if manifest.identity != authoritative.identity
        || !npm_sensitive_state_equal(npmrc, authoritative_npmrc)
        || !root_node_modules_path_equal(
            node_modules_path.as_deref(),
            authoritative_node_modules_path.as_deref(),
        )
        || managed_root_path != authoritative_managed_root_path
    {
        return Err(deno_core::anyhow::anyhow!(RESOLVER_INPUTS_CHANGED));
    }
    Ok(manifest)
}

struct WorkspaceResolverInputs<'a> {
    npmrc_path: Option<&'a Path>,
    byonm: bool,
    node_modules_path: Option<&'a Path>,
    managed_root_path: Option<&'a Path>,
}

fn manifest_from_workspace(
    initial_cwd: PathBuf,
    config_start_paths: Vec<PathBuf>,
    workspace_directory: &deno_config::workspace::WorkspaceDirectoryRc,
    npmrc: &deno_npmrc::ResolvedNpmRc,
    resolver_inputs: WorkspaceResolverInputs<'_>,
) -> deno_core::anyhow::Result<ResolverInputManifest> {
    let WorkspaceResolverInputs {
        npmrc_path,
        byonm,
        node_modules_path,
        managed_root_path,
    } = resolver_inputs;
    let workspace = &workspace_directory.workspace;
    let (npmrc_identity, npmrc_has_auth) = npm_config_fingerprint(npmrc);

    let mut configs = Vec::new();
    for config in workspace.resolver_deno_jsons() {
        if let Ok(path) = config.specifier.to_file_path() {
            configs.push(SemanticFingerprint {
                path: canonical_path(&path),
                hash: semantic_hash(&config.json)?,
            });
        }
    }
    normalize_semantic_fingerprints(&mut configs);

    let mut external_configs = Vec::new();
    for config in workspace.resolver_deno_jsons() {
        if let Some(path) = config.to_import_map_path()? {
            let path = canonical_path(&path);
            let Some((_, value)) = config.to_import_map_value(&RealSys)? else {
                continue;
            };
            external_configs.push(SemanticFingerprint {
                path,
                hash: semantic_hash(&value)?,
            });
        }
    }
    normalize_semantic_fingerprints(&mut external_configs);

    let mut packages = Vec::new();
    for package in workspace.package_jsons().chain(workspace.link_pkg_jsons()) {
        packages.push(SemanticFingerprint {
            path: canonical_path(&package.path),
            hash: semantic_hash(package.as_ref())?,
        });
    }
    normalize_semantic_fingerprints(&mut packages);

    let mut members = workspace
        .config_folders()
        .keys()
        .filter_map(|url| url.to_file_path().ok())
        .map(|path| canonical_path(&path))
        .collect::<Vec<_>>();
    let mut links = workspace
        .link_folders()
        .keys()
        .filter_map(|url| url.to_file_path().ok())
        .map(|path| canonical_path(&path))
        .collect::<Vec<_>>();
    normalize_paths(&mut members);
    normalize_paths(&mut links);

    let lockfile = workspace
        .resolve_lockfile_path()?
        .map(|path| canonical_path(&path));
    let lockfile_present = lockfile.as_ref().is_some_and(|path| path.exists());
    let mut scope_dirs = vec![workspace_directory.dir_path(), workspace.root_dir_path()];
    scope_dirs.extend(discovery_candidate_dirs(&initial_cwd, &config_start_paths));
    scope_dirs.extend(members.iter().cloned());
    scope_dirs.extend(links.iter().cloned());
    scope_dirs.extend(
        configs
            .iter()
            .chain(external_configs.iter())
            .chain(packages.iter())
            .filter_map(|entry| entry.path.parent().map(Path::to_path_buf)),
    );
    normalize_paths(&mut scope_dirs);

    #[cfg(test)]
    invoke_semantic_probe_test_hook();

    let mut files = Vec::new();
    for path in configs
        .iter()
        .chain(external_configs.iter())
        .chain(packages.iter())
        .map(|entry| &entry.path)
    {
        files.push(file_probe(path.clone(), FileProbeKind::Content));
    }
    let npmrc_probe_kind = if npmrc_has_auth {
        FileProbeKind::Metadata
    } else {
        FileProbeKind::Content
    };
    for dir in &scope_dirs {
        for name in MANIFEST_CANDIDATE_FILE_NAMES {
            files.push(file_probe(dir.join(name), FileProbeKind::Content));
        }
        files.push(file_probe(dir.join("package.json"), FileProbeKind::Content));
        files.push(file_probe(dir.join(".npmrc"), npmrc_probe_kind));
    }
    if let Some(path) = npmrc_path {
        files.push(file_probe(path.to_path_buf(), npmrc_probe_kind));
    }
    let global_npmrc = global_npmrc_path();
    if let Some(path) = &global_npmrc {
        files.push(file_probe(path.clone(), npmrc_probe_kind));
    }
    if let Some(path) = &lockfile {
        files.push(file_probe(path.clone(), FileProbeKind::Content));
    }
    normalize_file_probes(&mut files);
    let environment = environment_probe(&files, npmrc_has_auth, global_npmrc);

    // Canonical path identity is stable when a missing root becomes a regular
    // directory, but changes when the path is replaced by a link or its target
    // changes. It deliberately ignores directory metadata and children.
    let managed_root_node_modules = managed_root_path.map(|path| {
        let path = absolute_path(path);
        let identity = canonical_path(&path);
        RootPathProbe { path, identity }
    });

    let mut byonm_paths = if byonm {
        collect_byonm_node_modules_paths(&scope_dirs)
    } else {
        Vec::new()
    };
    if byonm {
        if let Some(path) = node_modules_path {
            byonm_paths.push(absolute_path(path));
        }
    }
    normalize_paths(&mut byonm_paths);
    let byonm_node_modules = byonm_paths
        .into_iter()
        .map(|path| DirectoryProbe {
            fingerprint: current_directory_fingerprint(&path),
            path,
        })
        .collect::<Vec<_>>();

    let identity = ResolverInputIdentity {
        initial_cwd: canonical_path(&initial_cwd),
        workspace_root: canonical_path(&workspace.root_dir_path()),
        discovery_dir: canonical_path(&workspace_directory.dir_path()),
        members,
        links,
        configs,
        external_configs,
        packages,
        npmrc: npmrc_identity,
        lockfile,
        lockfile_present,
        byonm,
    };

    Ok(ResolverInputManifest {
        identity,
        probe: ResolverInputProbe {
            files,
            byonm_node_modules,
            managed_root_node_modules,
            npmrc_has_auth,
            environment,
        },
    })
}

impl ResolverInputManifest {
    pub(crate) fn is_current(&self) -> deno_core::anyhow::Result<bool> {
        // Probe only the fixed candidates captured during construction. Do not
        // rediscover the workspace here: that path clears resolver caches and
        // reparses every config/package file on every reusable run.
        if self
            .probe
            .files
            .iter()
            .any(|probe| current_file_fingerprint(&probe.path, probe.kind) != probe.fingerprint)
        {
            return Ok(false);
        }
        if self
            .probe
            .managed_root_node_modules
            .as_ref()
            .is_some_and(|probe| canonical_path(&probe.path) != probe.identity)
        {
            return Ok(false);
        }
        if self
            .probe
            .byonm_node_modules
            .iter()
            .any(|probe| current_directory_fingerprint(&probe.path) != probe.fingerprint)
        {
            return Ok(false);
        }
        Ok(current_environment_probe(&self.probe.environment) == self.probe.environment)
    }

    /// Credentials may come from environment expansion that cannot be safely
    /// retained or fingerprinted. Rebuild authenticated resolver state for
    /// every run so a rotated credential is never hidden by manifest reuse.
    pub(crate) fn is_reusable(&self) -> deno_core::anyhow::Result<bool> {
        if self.probe.npmrc_has_auth || self.probe.environment.jsr_url_has_auth {
            return Ok(false);
        }
        self.is_current()
    }
}

/// Builds the only key used by the production snapshot cache. It is derived
/// from the accepted manifest's parsed baseline; no paths are re-read here.
#[cfg(any(feature = "npm", test))]
pub(crate) fn managed_snapshot_key(
    manifest: &ResolverInputManifest,
) -> Option<ManagedNpmSnapshotKey> {
    if manifest.identity.byonm
        || manifest.identity.lockfile_present
        || manifest.probe.npmrc_has_auth
    {
        return None;
    }
    Some(ManagedNpmSnapshotKey {
        identity: manifest.identity.clone(),
    })
}

fn semantic_hash<T: serde::Serialize>(value: &T) -> deno_core::anyhow::Result<u64> {
    let value = deno_core::serde_json::to_value(value)?;
    let bytes = deno_core::serde_json::to_vec(&canonical_json(value))?;
    Ok(hash_bytes(&bytes))
}

fn canonical_json(value: deno_core::serde_json::Value) -> deno_core::serde_json::Value {
    use deno_core::serde_json::Value;

    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_json).collect()),
        Value::Object(object) => {
            let mut entries = object.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut canonical = deno_core::serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonical_json(value));
            }
            Value::Object(canonical)
        }
        value => value,
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Compares sensitive resolver state only while both parsed npmrc values are
/// alive. None of this state is copied into the manifest, key, or diagnostics.
fn npm_sensitive_state_equal(
    left: &deno_npmrc::ResolvedNpmRc,
    right: &deno_npmrc::ResolvedNpmRc,
) -> bool {
    npm_auth_presence(left) == npm_auth_presence(right)
        && left.default_config == right.default_config
        && left.scopes == right.scopes
        && left.registry_configs == right.registry_configs
        && left.replace_registry_host == right.replace_registry_host
}

fn root_node_modules_path_equal(left: Option<&Path>, right: Option<&Path>) -> bool {
    left.map(canonical_path) == right.map(canonical_path)
}

fn npm_auth_presence(npmrc: &deno_npmrc::ResolvedNpmRc) -> bool {
    let mut has_auth = registry_config_has_sensitive(&npmrc.default_config.config)
        || url_has_userinfo(&npmrc.default_config.registry_url);
    has_auth |= npmrc.scopes.values().any(|config| {
        registry_config_has_sensitive(&config.config) || url_has_userinfo(&config.registry_url)
    });
    has_auth |= npmrc.registry_configs.iter().any(|(key, config)| {
        registry_config_has_sensitive(config) || registry_config_key_has_userinfo(key)
    });
    if let ReplaceRegistryHost::Url(url) = &npmrc.replace_registry_host {
        has_auth |= url_has_userinfo(url);
    }
    has_auth
}

fn npm_config_fingerprint(npmrc: &deno_npmrc::ResolvedNpmRc) -> (NpmConfigFingerprint, bool) {
    let mut has_auth = registry_config_has_sensitive(&npmrc.default_config.config)
        || url_has_userinfo(&npmrc.default_config.registry_url);
    let mut scoped_registries = npmrc
        .scopes
        .iter()
        .map(|(scope, config)| {
            has_auth |= registry_config_has_sensitive(&config.config)
                || url_has_userinfo(&config.registry_url);
            (scope.clone(), public_registry_url(&config.registry_url))
        })
        .collect::<Vec<_>>();
    scoped_registries.sort();

    let mut registry_configs = npmrc
        .registry_configs
        .iter()
        .map(|(key, config)| {
            has_auth |=
                registry_config_has_sensitive(config) || registry_config_key_has_userinfo(key);
            public_registry_config_key(key)
        })
        .collect::<Vec<_>>();
    registry_configs.sort();

    if let ReplaceRegistryHost::Url(url) = &npmrc.replace_registry_host {
        has_auth |= url_has_userinfo(url);
    }

    let mut trust_policy_exclude = npmrc.trust_policy_exclude.clone();
    trust_policy_exclude.sort();
    trust_policy_exclude.dedup();

    (
        NpmConfigFingerprint {
            default_registry: public_registry_url(&npmrc.default_config.registry_url),
            scoped_registries,
            registry_configs,
            replace_registry_host: replace_registry_host_identity(&npmrc.replace_registry_host),
            min_release_age_days: npmrc.min_release_age_days,
            trust_policy_no_downgrade: matches!(
                npmrc.trust_policy,
                deno_npmrc::TrustPolicyConfig::NoDowngrade
            ),
            trust_policy_ignore_after_minutes: npmrc.trust_policy_ignore_after_minutes,
            trust_policy_exclude,
        },
        has_auth,
    )
}

fn registry_config_has_sensitive(config: &deno_npmrc::RegistryConfig) -> bool {
    config.has_auth() || config.certfile.is_some() || config.keyfile.is_some()
}

fn url_has_userinfo(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn registry_config_key_has_userinfo(key: &str) -> bool {
    let candidate = if key.starts_with("//") {
        format!("https:{key}")
    } else {
        key.to_string()
    };
    Url::parse(&candidate)
        .map(|url| url_has_userinfo(&url))
        .unwrap_or_else(|_| {
            candidate
                .split_once("://")
                .and_then(|(_, rest)| rest.split('/').next())
                .is_some_and(|authority| authority.contains('@'))
        })
}

/// Registry URLs can legally contain userinfo. Keep only routing identity in
/// the manifest; credentials are never retained in a key or its Debug output.
fn public_registry_url(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

fn public_registry_config_key(key: &str) -> String {
    let candidate = if key.starts_with("//") {
        format!("https:{key}")
    } else {
        key.to_string()
    };
    Url::parse(&candidate)
        .map(|url| public_registry_url(&url))
        .unwrap_or_else(|_| {
            if candidate.contains('@') {
                "<redacted-registry>".to_string()
            } else {
                candidate
            }
        })
}

fn replace_registry_host_identity(value: &ReplaceRegistryHost) -> String {
    match value {
        ReplaceRegistryHost::NpmJs => "npmjs".to_string(),
        ReplaceRegistryHost::Never => "never".to_string(),
        ReplaceRegistryHost::Always => "always".to_string(),
        ReplaceRegistryHost::Hostname(hostname) => format!("hostname:{hostname}"),
        ReplaceRegistryHost::Url(url) => format!("url:{}", public_registry_url(url)),
    }
}

fn environment_probe(
    files: &[FileProbe],
    npmrc_has_auth: bool,
    global_npmrc: Option<PathBuf>,
) -> EnvironmentProbe {
    let mut referenced_npmrc_vars = if npmrc_has_auth {
        Vec::new()
    } else {
        let mut names = files
            .iter()
            .filter(|probe| {
                probe.kind == FileProbeKind::Content
                    && probe.path.file_name().is_some_and(|name| name == ".npmrc")
            })
            .flat_map(|probe| referenced_env_names(&probe.path))
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| EnvironmentValueProbe {
                fingerprint: env_value_fingerprint(&name),
                name,
            })
            .collect::<Vec<_>>()
    };
    referenced_npmrc_vars.sort_by(|left, right| left.name.cmp(&right.name));
    EnvironmentProbe {
        deno_dir: std::env::var_os("DENO_DIR"),
        no_package_json: std::env::var_os("DENO_NO_PACKAGE_JSON").as_deref()
            == Some(std::ffi::OsStr::new("1")),
        jsr_url: env_url_identity(&["JSR_URL"]),
        jsr_url_has_auth: env_url_has_userinfo(&["JSR_URL"]),
        registry: env_url_identity(&["NPM_CONFIG_REGISTRY", "npm_config_registry"]),
        registry_has_auth: env_url_has_userinfo(&["NPM_CONFIG_REGISTRY", "npm_config_registry"]),
        replace_registry_host: env_replace_registry_host_identity(),
        replace_registry_host_has_auth: env_replace_registry_host_has_auth(),
        min_release_age: env_scalar_identity(&[
            "NPM_CONFIG_MIN_RELEASE_AGE",
            "npm_config_min_release_age",
        ]),
        global_npmrc,
        referenced_npmrc_vars,
    }
}

fn current_environment_probe(baseline: &EnvironmentProbe) -> EnvironmentProbe {
    EnvironmentProbe {
        deno_dir: std::env::var_os("DENO_DIR"),
        no_package_json: std::env::var_os("DENO_NO_PACKAGE_JSON").as_deref()
            == Some(std::ffi::OsStr::new("1")),
        jsr_url: env_url_identity(&["JSR_URL"]),
        jsr_url_has_auth: env_url_has_userinfo(&["JSR_URL"]),
        registry: env_url_identity(&["NPM_CONFIG_REGISTRY", "npm_config_registry"]),
        registry_has_auth: env_url_has_userinfo(&["NPM_CONFIG_REGISTRY", "npm_config_registry"]),
        replace_registry_host: env_replace_registry_host_identity(),
        replace_registry_host_has_auth: env_replace_registry_host_has_auth(),
        min_release_age: env_scalar_identity(&[
            "NPM_CONFIG_MIN_RELEASE_AGE",
            "npm_config_min_release_age",
        ]),
        global_npmrc: global_npmrc_path(),
        referenced_npmrc_vars: baseline
            .referenced_npmrc_vars
            .iter()
            .map(|probe| EnvironmentValueProbe {
                name: probe.name.clone(),
                fingerprint: env_value_fingerprint(&probe.name),
            })
            .collect(),
    }
}

fn referenced_env_names(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find("${") {
        let start = offset + start + 2;
        let Some(end) = text[start..].find('}') else {
            break;
        };
        let end = start + end;
        let name = &text[start..end];
        if !name.is_empty() && !name.contains(['$', '{', '\\']) {
            names.push(name.to_string());
        }
        offset = end + 1;
    }
    names
}

fn env_value_fingerprint(name: &str) -> Option<(u64, u64)> {
    let value = std::env::var(name).ok()?;
    Some((hash_bytes(value.as_bytes()), value.len() as u64))
}

fn env_url_identity(names: &[&str]) -> Option<String> {
    for name in names {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        return Some(
            Url::parse(&value)
                .map(|url| public_registry_url(&url))
                .unwrap_or_else(|_| "<invalid>".to_string()),
        );
    }
    None
}

fn env_url_has_userinfo(names: &[&str]) -> bool {
    for name in names {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        return Url::parse(&value)
            .map(|url| url_has_userinfo(&url))
            .unwrap_or(false);
    }
    false
}

fn env_replace_registry_host_identity() -> Option<String> {
    for name in [
        "NPM_CONFIG_REPLACE_REGISTRY_HOST",
        "npm_config_replace_registry_host",
    ] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        return Some(match value.trim() {
            "" | "npmjs" | "never" | "always" => value.trim().to_string(),
            value => Url::parse(value)
                .ok()
                .filter(|url| url.host_str().is_some())
                .map(|url| format!("url:{}", public_registry_url(&url)))
                .unwrap_or_else(|| {
                    format!("hostname:{}", env_replace_registry_hostname_identity(value))
                }),
        });
    }
    None
}

fn env_replace_registry_hostname_identity(value: &str) -> String {
    Url::parse(&format!("https://{value}"))
        .ok()
        .filter(|url| url.host_str().is_some())
        .map(|url| public_registry_url(&url))
        .unwrap_or_else(|| {
            value
                .rsplit_once('@')
                .map_or(value, |(_, hostname)| hostname)
                .to_string()
        })
}

fn env_replace_registry_host_has_auth() -> bool {
    for name in [
        "NPM_CONFIG_REPLACE_REGISTRY_HOST",
        "npm_config_replace_registry_host",
    ] {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        return Url::parse(value.trim())
            .ok()
            .filter(|url| url.host_str().is_some())
            .is_some_and(|url| url_has_userinfo(&url));
    }
    false
}

fn env_scalar_identity(names: &[&str]) -> Option<String> {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            return Some(value);
        }
    }
    None
}

fn collect_byonm_node_modules_paths(scope_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for dir in scope_dirs {
        for ancestor in dir.ancestors() {
            paths.push(absolute_path(&ancestor.join("node_modules")));
        }
    }
    paths
}

fn discovery_candidate_dirs(initial_cwd: &Path, start_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.extend(
        canonical_path(initial_cwd)
            .ancestors()
            .map(Path::to_path_buf),
    );
    for start_path in start_paths {
        dirs.extend(
            canonical_path(start_path)
                .ancestors()
                .map(Path::to_path_buf),
        );
    }
    dirs
}

fn normalize_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn normalize_semantic_fingerprints(paths: &mut Vec<SemanticFingerprint>) {
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    paths.dedup_by(|a, b| a.path == b.path);
}

fn normalize_file_probes(probes: &mut Vec<FileProbe>) {
    probes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
    probes.dedup_by(|a, b| a.path == b.path && a.kind == b.kind);
}

fn file_probe(path: PathBuf, kind: FileProbeKind) -> FileProbe {
    let path = absolute_path(&path);
    let fingerprint = current_file_fingerprint(&path, kind);
    FileProbe {
        path,
        kind,
        fingerprint,
    }
}

fn current_file_fingerprint(path: &Path, kind: FileProbeKind) -> Option<(u64, u64)> {
    let target_fingerprint = match kind {
        FileProbeKind::Content => content_fingerprint(path),
        FileProbeKind::Metadata => metadata_fingerprint(path),
    };
    path_probe_fingerprint(path, target_fingerprint)
}

fn current_directory_fingerprint(path: &Path) -> Option<(u64, u64)> {
    path_probe_fingerprint(path, metadata_fingerprint(path))
}

fn path_probe_fingerprint(
    path: &Path,
    target_fingerprint: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    let link_metadata = std::fs::symlink_metadata(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    link_metadata.file_type().is_symlink().hash(&mut hasher);
    link_metadata.len().hash(&mut hasher);
    meta_fingerprint(&link_metadata).hash(&mut hasher);
    std::fs::read_link(path).ok().hash(&mut hasher);
    std::fs::canonicalize(path).ok().hash(&mut hasher);
    target_fingerprint.hash(&mut hasher);
    Some((
        hasher.finish(),
        target_fingerprint.map_or(0, |(_, length)| length),
    ))
}

fn content_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let bytes = std::fs::read(path).ok()?;
    Some((hash_bytes(&bytes), bytes.len() as u64))
}

fn metadata_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((meta_fingerprint(&metadata)?, metadata.len()))
}

fn global_npmrc_path() -> Option<PathBuf> {
    RealSys
        .env_home_dir()
        .map(|home| absolute_path(&home.join(".npmrc")))
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Canonicalizes an existing path and preserves a distinct absolute identity
/// for a missing path by canonicalizing its nearest existing parent.
fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let absolute = absolute_path(path);
    let Some(file_name) = absolute.file_name() else {
        return absolute;
    };
    absolute
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .map(|parent| parent.join(file_name))
        .unwrap_or(absolute)
}

fn meta_fingerprint(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos() as u64)
}

/// FIFO cache of recent snapshots; bounded so a long-lived process serving
/// many projects does not grow without limit. Key construction is bounded but
/// deliberately has no public latency guarantee.
#[cfg(any(feature = "npm", test))]
type SnapshotCache = Vec<(
    Arc<ManagedNpmSnapshotKey>,
    ValidSerializedNpmResolutionSnapshot,
)>;

#[cfg(any(feature = "npm", test))]
static CACHE: OnceLock<Mutex<SnapshotCache>> = OnceLock::new();
#[cfg(any(feature = "npm", test))]
const MAX_ENTRIES: usize = 8;

#[cfg(any(feature = "npm", test))]
fn cache() -> &'static Mutex<SnapshotCache> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Returns a clone of the snapshot cached for `key`, if any.
#[cfg(any(feature = "npm", test))]
pub(crate) fn get(key: &ManagedNpmSnapshotKey) -> Option<ValidSerializedNpmResolutionSnapshot> {
    let entries = cache().lock().unwrap_or_else(|e| e.into_inner());
    entries
        .iter()
        .find(|(candidate, _)| candidate.as_ref() == key)
        .map(|(_, snapshot)| snapshot.clone())
}

/// Caches `snapshot` under `key`, replacing an existing entry with the same
/// key. On overflow the oldest entry is dropped (FIFO).
#[cfg(any(feature = "npm", test))]
pub(crate) fn insert(
    key: Arc<ManagedNpmSnapshotKey>,
    snapshot: ValidSerializedNpmResolutionSnapshot,
) {
    let mut entries = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = entries
        .iter_mut()
        .find(|(candidate, _)| candidate.as_ref() == key.as_ref())
    {
        entry.1 = snapshot;
        return;
    }
    entries.push((key, snapshot));
    if entries.len() > MAX_ENTRIES {
        entries.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deno_resolver_adapter::new_resolver_factory;
    use crate::deno_resolver_adapter::new_workspace_factory;
    use deno_npmrc::NpmRc;
    use deno_npmrc::NpmRegistryUrl;

    use super::RESOLVER_ENV_TEST_LOCK as ENV_LOCK;

    struct TestEnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl TestEnvGuard {
        fn new(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|&name| (name, std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn test_key(n: u64) -> ManagedNpmSnapshotKey {
        ManagedNpmSnapshotKey {
            identity: ResolverInputIdentity {
                initial_cwd: PathBuf::from(format!("/proj/{n}")),
                workspace_root: PathBuf::from("/proj"),
                discovery_dir: PathBuf::from(format!("/proj/{n}")),
                members: vec![],
                links: vec![],
                configs: vec![],
                external_configs: vec![],
                packages: vec![],
                npmrc: NpmConfigFingerprint {
                    default_registry: String::new(),
                    scoped_registries: vec![],
                    registry_configs: vec![],
                    replace_registry_host: String::new(),
                    min_release_age_days: None,
                    trust_policy_no_downgrade: false,
                    trust_policy_ignore_after_minutes: None,
                    trust_policy_exclude: vec![],
                },
                lockfile: None,
                lockfile_present: false,
                byonm: false,
            },
        }
    }

    fn test_manifest(n: u64) -> ResolverInputManifest {
        let key = test_key(n);
        ResolverInputManifest {
            identity: key.identity,
            probe: ResolverInputProbe {
                files: vec![],
                byonm_node_modules: vec![],
                managed_root_node_modules: None,
                npmrc_has_auth: false,
                environment: EnvironmentProbe {
                    deno_dir: None,
                    no_package_json: false,
                    jsr_url: None,
                    jsr_url_has_auth: false,
                    registry: None,
                    registry_has_auth: false,
                    replace_registry_host: None,
                    replace_registry_host_has_auth: false,
                    min_release_age: None,
                    global_npmrc: None,
                    referenced_npmrc_vars: vec![],
                },
            },
        }
    }

    #[cfg(feature = "npm")]
    struct ColdInstallRegistry {
        shutdown: std::sync::mpsc::Sender<()>,
        thread: std::thread::JoinHandle<()>,
    }

    #[cfg(feature = "npm")]
    impl ColdInstallRegistry {
        fn new() -> (Self, String) {
            use std::io::{Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let base_url = format!("http://127.0.0.1:{}/", address.port());
            let packument = format!(
                r#"{{"name":"cold-pkg","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"cold-pkg","version":"1.0.0","main":"index.js","dist":{{"tarball":"{base_url}cold-pkg-1.0.0.tgz"}}}}}}}}"#
            );
            let archive = test_gzip(&test_tar_archive(&[
                (
                    "package/package.json",
                    br#"{"name":"cold-pkg","version":"1.0.0","main":"index.js"}"#,
                ),
                ("package/index.js", b"module.exports = 'cold-install-ok';"),
            ]));
            let responses = [
                (
                    "/cold-pkg".to_string(),
                    ("application/json".to_string(), packument.into_bytes()),
                ),
                (
                    "/cold-pkg-1.0.0.tgz".to_string(),
                    ("application/octet-stream".to_string(), archive),
                ),
            ]
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
            let (shutdown, receiver) = std::sync::mpsc::channel();
            listener.set_nonblocking(true).unwrap();
            let thread = std::thread::spawn(move || loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let mut request = Vec::new();
                        let mut buffer = [0; 1024];
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(count) => {
                                    request.extend_from_slice(&buffer[..count]);
                                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let path = request
                            .split(|byte| *byte == b' ')
                            .nth(1)
                            .map(|path| String::from_utf8_lossy(path).into_owned())
                            .unwrap_or_default();
                        let response = responses
                            .get(&path)
                            .map(|(content_type, body)| test_http_response(200, content_type, body))
                            .unwrap_or_else(|| test_http_response(404, "text/plain", b"missing"));
                        let _ = stream.write_all(&response);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if receiver.try_recv().is_ok() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            });
            (Self { shutdown, thread }, base_url)
        }

        fn shutdown(self) {
            let _ = self.shutdown.send(());
            let _ = self.thread.join();
        }
    }

    #[cfg(feature = "npm")]
    fn test_http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[cfg(feature = "npm")]
    fn test_tar_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (name, contents) in entries {
            let mut header = [0u8; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", contents.len()).as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].copy_from_slice(b"        ");
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum: u32 = header.iter().map(|byte| *byte as u32).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            archive.extend_from_slice(&header);
            archive.extend_from_slice(contents);
            archive.extend(std::iter::repeat_n(0, (512 - contents.len() % 512) % 512));
        }
        archive.extend([0u8; 1024]);
        archive
    }

    #[cfg(feature = "npm")]
    fn test_gzip(data: &[u8]) -> Vec<u8> {
        let mut compressed = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 0x03];
        let mut offset = 0;
        loop {
            let count = (data.len() - offset).min(65535);
            let final_block = offset + count == data.len();
            let length = count as u16;
            compressed.push(final_block as u8);
            compressed.extend_from_slice(&length.to_le_bytes());
            compressed.extend_from_slice(&(!length).to_le_bytes());
            compressed.extend_from_slice(&data[offset..offset + count]);
            offset += count;
            if final_block {
                break;
            }
        }
        let crc = test_crc32(data);
        compressed.extend_from_slice(&crc.to_le_bytes());
        compressed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        compressed
    }

    #[cfg(feature = "npm")]
    fn test_crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffff;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    0xedb8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        crc ^ 0xffff_ffff
    }

    #[test]
    fn fifo_eviction_and_replace() {
        for n in 0..9 {
            insert(
                Arc::new(test_key(n)),
                ValidSerializedNpmResolutionSnapshot::default(),
            );
        }
        assert!(
            get(&test_key(0)).is_none(),
            "oldest entry should be evicted"
        );
        assert!(
            get(&test_key(8)).is_some(),
            "newest entry should be present"
        );
        insert(
            Arc::new(test_key(8)),
            ValidSerializedNpmResolutionSnapshot::default(),
        );
        assert!(get(&test_key(8)).is_some());
        insert(
            Arc::new(test_key(9)),
            ValidSerializedNpmResolutionSnapshot::default(),
        );
        assert!(get(&test_key(1)).is_none(), "next oldest should be evicted");
        assert!(get(&test_key(9)).is_some());
    }

    #[test]
    fn lockfile_byonm_and_auth_manifests_are_not_cacheable() {
        let mut lockfile = test_manifest(20);
        lockfile.identity.lockfile = Some(PathBuf::from("/proj/deno.lock"));
        lockfile.identity.lockfile_present = true;
        assert!(managed_snapshot_key(&lockfile).is_none());

        let mut byonm = test_manifest(21);
        byonm.identity.byonm = true;
        assert!(managed_snapshot_key(&byonm).is_none());

        let mut auth = test_manifest(22);
        auth.probe.npmrc_has_auth = true;
        assert!(managed_snapshot_key(&auth).is_none());
    }

    #[test]
    fn actual_byonm_manifest_probes_external_node_modules_and_disables_key() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "libdeno-resolver-byonm-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let node_modules = dir.join("node_modules");
        std::fs::create_dir_all(node_modules.join("external-pkg")).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"byonm-project","dependencies":{}}"#,
        )
        .unwrap();

        std::fs::write(dir.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        assert!(resolver.use_byonm().unwrap());
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();

        assert!(manifest.identity.byonm);
        assert!(manifest
            .probe
            .byonm_node_modules
            .iter()
            .any(|probe| probe.path == absolute_path(&node_modules)));
        assert!(managed_snapshot_key(&manifest).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn managed_manifest_tracks_effective_root_identity_not_its_contents() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "libdeno-managed-node-modules-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let target_a = dir.join("target-a");
        let target_b = dir.join("target-b");
        std::fs::create_dir_all(&target_a).unwrap();
        std::fs::create_dir_all(&target_b).unwrap();
        let node_modules = dir.join("node_modules");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"managed-link-project","dependencies":{}}"#,
        )
        .unwrap();

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        assert!(!resolver.use_byonm().unwrap());
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(manifest.is_current().unwrap());
        std::fs::create_dir(&node_modules).unwrap();
        std::fs::create_dir(node_modules.join("installed-package")).unwrap();
        assert!(manifest.is_current().unwrap());

        std::fs::remove_dir(node_modules.join("installed-package")).unwrap();
        std::fs::remove_dir(&node_modules).unwrap();
        std::os::unix::fs::symlink(&target_a, &node_modules).unwrap();
        assert!(!manifest.is_current().unwrap());

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(!manifest.identity.byonm);
        assert!(manifest.is_current().unwrap());
        std::fs::remove_file(&node_modules).unwrap();
        std::os::unix::fs::symlink(&target_b, &node_modules).unwrap();
        assert!(!manifest.is_current().unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn managed_root_path_falls_back_to_initial_cwd_without_discovery_config() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "libdeno-managed-root-selection-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let initial_cwd = dir.join("project");
        let discovery_start = dir.join("discovery");
        std::fs::create_dir_all(&initial_cwd).unwrap();
        std::fs::create_dir_all(&discovery_start).unwrap();
        let factory =
            new_workspace_factory(initial_cwd.clone(), vec![discovery_start.clone()]).unwrap();
        let root_folder = factory
            .workspace_directory()
            .unwrap()
            .workspace
            .root_folder_configs();
        assert!(root_folder.deno_json.is_none());
        assert!(root_folder.pkg_json.is_none());
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        assert!(!resolver.use_byonm().unwrap());
        let managed_root = crate::deno_resolver_adapter::managed_root_node_modules_path(
            &factory,
            &resolver,
            &initial_cwd,
        )
        .unwrap()
        .unwrap();
        assert_eq!(managed_root, initial_cwd.join("node_modules"));
        assert_ne!(managed_root, discovery_start.join("node_modules"));

        let manifest = resolver_input_manifest(
            initial_cwd.clone(),
            vec![discovery_start],
            &factory,
            &resolver,
        )
        .unwrap();
        assert_eq!(
            manifest
                .probe
                .managed_root_node_modules
                .as_ref()
                .unwrap()
                .path,
            initial_cwd.join("node_modules")
        );
        std::fs::create_dir_all(dir.join("discovery/node_modules/unrelated")).unwrap();
        assert!(manifest.is_current().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn managed_root_probe_is_absent_when_node_modules_mode_is_none() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("libdeno-managed-root-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deno.json"), r#"{"nodeModulesDir":"none"}"#).unwrap();

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        assert!(!resolver.use_byonm().unwrap());
        assert!(
            crate::deno_resolver_adapter::resolver_byonm_and_root_node_modules_path(&resolver)
                .unwrap()
                .1
                .is_none()
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(manifest.probe.managed_root_node_modules.is_none());

        std::fs::create_dir_all(dir.join("node_modules/unrelated")).unwrap();
        assert!(manifest.is_current().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "npm")]
    #[test]
    fn successful_cold_install_saves_snapshot_when_root_appears() {
        let _semantic_lock = semantic_probe_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "libdeno-cold-snapshot-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = dir.join("home");
        let deno_dir = dir.join("deno-dir");
        let project = dir.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&deno_dir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let _env = TestEnvGuard::new(&["HOME", "NPM_CONFIG_REGISTRY", "DENO_DIR"]);
        let (registry, registry_url) = ColdInstallRegistry::new();
        std::env::set_var("HOME", &home);
        std::env::set_var("DENO_DIR", &deno_dir);
        std::env::set_var("NPM_CONFIG_REGISTRY", registry_url);
        std::fs::write(
            project.join("package.json"),
            r#"{"name":"cold-snapshot-project","dependencies":{"cold-pkg":"1.0.0"}}"#,
        )
        .unwrap();
        let entry = project.join("main.js");
        std::fs::write(
            &entry,
            "import value from 'npm:cold-pkg'; if (value !== 'cold-install-ok') throw new Error('bad package');",
        )
        .unwrap();

        let factory = new_workspace_factory(project.clone(), vec![project.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(project.clone(), vec![project.clone()], &factory, &resolver)
                .unwrap();
        let key = managed_snapshot_key(&manifest).expect("managed project should be cacheable");
        assert!(get(&key).is_none(), "snapshot must be cold before the run");
        assert!(!project.join("node_modules").exists());

        assert_eq!(
            crate::run(
                &entry,
                &crate::LibdenoOptions {
                    cwd: Some(project.clone()),
                    allow_all_permissions: true,
                    ..Default::default()
                }
            )
            .unwrap(),
            0
        );
        registry.shutdown();

        assert!(project.join("node_modules").is_dir());
        assert!(
            get(&key).is_some(),
            "successful first install did not save its snapshot"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn semantic_hash_ignores_json_object_order() {
        let first: deno_core::serde_json::Value =
            deno_core::serde_json::from_str(r##"{"imports":{"#a":"./a.js","#b":"./b.js"}}"##)
                .unwrap();
        let second: deno_core::serde_json::Value =
            deno_core::serde_json::from_str(r##"{"imports":{"#b":"./b.js","#a":"./a.js"}}"##)
                .unwrap();
        assert_eq!(
            semantic_hash(&first).unwrap(),
            semantic_hash(&second).unwrap()
        );
    }

    #[test]
    fn auth_anywhere_is_detected() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old_token = std::env::var_os("NPM_TOKEN");
        let old_registry = std::env::var_os("NPM_CONFIG_REGISTRY");
        let old_replace = std::env::var_os("NPM_CONFIG_REPLACE_REGISTRY_HOST");
        std::env::remove_var("NPM_CONFIG_REGISTRY");
        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");

        let cases = [
            (
                "registry=https://default-user:default-token@example.test/\n",
                None,
            ),
            (
                "@scope:registry=https://scope-user:scope-token@example.test/scope/\n",
                None,
            ),
            (
                "registry=https://registry.example/\n//registry.example/:_authToken=registry-token\n",
                None,
            ),
            (
                "registry=https://registry.example/\n//registry.example/:_authToken=${NPM_TOKEN}\n",
                Some(("NPM_TOKEN", "expanded-token")),
            ),
            (
                "registry=https://registry.example/\n//registry.example/:certfile=client-cert\n",
                None,
            ),
            (
                "replace-registry-host=https://replace-user:replace-token@example.test/\n",
                None,
            ),
        ];

        for (source, env) in cases {
            std::env::remove_var("NPM_TOKEN");
            if let Some((name, value)) = env {
                std::env::set_var(name, value);
            }
            let resolved = NpmRc::parse(&RealSys, source)
                .unwrap()
                .as_resolved(&NpmRegistryUrl::for_npm(&RealSys))
                .unwrap();
            let (_, has_auth) = npm_config_fingerprint(&resolved);
            assert!(has_auth, "auth was not detected");
        }

        match old_token {
            Some(value) => std::env::set_var("NPM_TOKEN", value),
            None => std::env::remove_var("NPM_TOKEN"),
        }
        match old_registry {
            Some(value) => std::env::set_var("NPM_CONFIG_REGISTRY", value),
            None => std::env::remove_var("NPM_CONFIG_REGISTRY"),
        }
        match old_replace {
            Some(value) => std::env::set_var("NPM_CONFIG_REPLACE_REGISTRY_HOST", value),
            None => std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST"),
        }
    }

    #[test]
    fn environment_probe_tracks_registry_url_userinfo_without_retaining_it() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&[
            "HOME",
            "NPM_CONFIG_REGISTRY",
            "npm_config_registry",
            "NPM_CONFIG_REPLACE_REGISTRY_HOST",
            "npm_config_replace_registry_host",
        ]);
        std::env::remove_var("npm_config_registry");
        std::env::remove_var("npm_config_replace_registry_host");
        std::env::set_var("NPM_CONFIG_REGISTRY", "https://registry.example/");
        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");

        let baseline = environment_probe(&[], false, global_npmrc_path());
        assert!(!baseline.registry_has_auth);
        assert!(!baseline.replace_registry_host_has_auth);

        std::env::set_var(
            "NPM_CONFIG_REGISTRY",
            "https://registry-user:registry-secret@example.test/",
        );
        let with_registry_auth = current_environment_probe(&baseline);
        assert!(with_registry_auth.registry_has_auth);
        assert_ne!(with_registry_auth, baseline);

        std::env::set_var("NPM_CONFIG_REGISTRY", "https://registry.example/");
        let without_registry_auth = current_environment_probe(&baseline);
        assert!(!without_registry_auth.registry_has_auth);
        assert_eq!(without_registry_auth, baseline);

        std::env::set_var(
            "NPM_CONFIG_REPLACE_REGISTRY_HOST",
            "https://replace-user:replace-secret@example.test/",
        );
        let with_replace_auth = current_environment_probe(&baseline);
        assert!(with_replace_auth.replace_registry_host_has_auth);
        assert_ne!(with_replace_auth, baseline);

        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");
        let without_replace_auth = current_environment_probe(&baseline);
        assert!(!without_replace_auth.replace_registry_host_has_auth);
        assert_eq!(without_replace_auth, baseline);

        let debug = format!("{with_registry_auth:?}{with_replace_auth:?}");
        assert!(!debug.contains("registry-secret"));
        assert!(!debug.contains("replace-secret"));
    }

    #[test]
    fn environment_probe_invalidates_on_deno_dir_and_no_package_json_changes() {
        let _semantic_lock = semantic_probe_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&["DENO_DIR", "DENO_NO_PACKAGE_JSON"]);
        std::env::set_var("DENO_DIR", "/tmp/deno-cache-a");
        std::env::remove_var("DENO_NO_PACKAGE_JSON");

        let baseline = environment_probe(&[], false, global_npmrc_path());
        assert_eq!(current_environment_probe(&baseline), baseline);

        std::env::set_var("DENO_DIR", "/tmp/deno-cache-b");
        assert_ne!(current_environment_probe(&baseline), baseline);
        std::env::set_var("DENO_DIR", "/tmp/deno-cache-a");
        assert_eq!(current_environment_probe(&baseline), baseline);

        std::env::set_var("DENO_NO_PACKAGE_JSON", "true");
        assert_eq!(current_environment_probe(&baseline), baseline);
        std::env::set_var("DENO_NO_PACKAGE_JSON", "1");
        assert_ne!(current_environment_probe(&baseline), baseline);
    }

    #[test]
    fn env_replace_registry_host_identity_distinguishes_non_url_hostnames() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&["NPM_CONFIG_REPLACE_REGISTRY_HOST"]);

        std::env::set_var("NPM_CONFIG_REPLACE_REGISTRY_HOST", "registry-a.example");
        let first = env_replace_registry_host_identity().unwrap();
        std::env::set_var("NPM_CONFIG_REPLACE_REGISTRY_HOST", "registry-b.example");
        let second = env_replace_registry_host_identity().unwrap();

        assert_ne!(first, second);
        assert!(first.contains("registry-a.example"));
        assert!(second.contains("registry-b.example"));

        std::env::set_var(
            "NPM_CONFIG_REPLACE_REGISTRY_HOST",
            "https://replace-user:replace-secret@example.test/",
        );
        let url_identity = env_replace_registry_host_identity().unwrap();
        assert!(!url_identity.contains("replace-secret"));
    }

    #[test]
    fn resolver_manifest_probe_catches_member_edit_before_key_publish() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("libdeno-resolver-manifest-{}", std::process::id()));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        let member = dir.join("member");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        std::fs::write(dir.join("deno.json"), r#"{"workspace":["./member"]}"#).unwrap();
        let package = member.join("package.json");
        let member_config = member.join("deno.json");
        let config_a = r##"{"imports":{"#mod":"./a.js"}}"##;
        let config_b = r##"{"imports":{"#mod":"./b.js"}}"##;
        assert_eq!(config_a.len(), config_b.len());
        std::fs::write(&member_config, config_a).unwrap();
        let first = r#"{"name":"member-a","version":"1.0.0"}"#;
        let second = r#"{"name":"member-b","version":"1.0.0"}"#;
        assert_eq!(first.len(), second.len());
        std::fs::write(&package, first).unwrap();

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        let key = managed_snapshot_key(&manifest).expect("manifest should be cacheable");
        assert!(manifest.is_current().unwrap());
        std::fs::write(&package, second).unwrap();
        assert!(!manifest.is_current().unwrap());
        assert_eq!(managed_snapshot_key(&manifest), Some(key));

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        std::fs::write(&member_config, config_b).unwrap();
        assert!(!manifest.is_current().unwrap());

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    fn semantic_probe_npmrc_transition_is_rejected(first: &str, second: &str) {
        let _semantic_lock = semantic_probe_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&[
            "HOME",
            "NPM_CONFIG_REGISTRY",
            "NPM_CONFIG_REPLACE_REGISTRY_HOST",
        ]);
        let dir = std::env::temp_dir().join(format!(
            "libdeno-sensitive-npmrc-transition-{}",
            std::process::id()
        ));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"sensitive-transition","dependencies":{}}"#,
        )
        .unwrap();
        let npmrc = dir.join(".npmrc");
        std::fs::write(&npmrc, first).unwrap();
        std::env::set_var("HOME", &home);
        std::env::remove_var("NPM_CONFIG_REGISTRY");
        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutations_for_hook = mutations.clone();
        let npmrc_for_hook = npmrc.clone();
        let second_for_hook = second.to_string();
        set_semantic_probe_test_hook(move || {
            if mutations_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::write(&npmrc_for_hook, &second_for_hook).unwrap();
            }
        });
        let result = resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver);
        clear_semantic_probe_test_hook();

        let error = match result {
            Ok(_) => panic!("sensitive npm transition was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), RESOLVER_INPUTS_CHANGED);
        assert!(
            mutations.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the hook must run in the semantic-to-probe window for both parses"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn semantic_probe_rejects_no_auth_to_auth_transition() {
        semantic_probe_npmrc_transition_is_rejected(
            "registry=https://registry.example/\n",
            "registry=https://registry.example/\n//registry.example/:_authToken=fixture-a\n",
        );
    }

    #[test]
    fn semantic_probe_rejects_auth_rotation_with_same_public_routing() {
        semantic_probe_npmrc_transition_is_rejected(
            "registry=https://registry.example/\n//registry.example/:_authToken=fixture-a\n",
            "registry=https://registry.example/\n//registry.example/:_authToken=fixture-b\n",
        );
    }

    #[test]
    fn root_node_modules_path_comparison_normalizes_paths() {
        assert_eq!(DENO_CONFIG_FILE_NAMES, ["deno.json", "deno.jsonc"]);
        assert_eq!(
            MANIFEST_CANDIDATE_FILE_NAMES,
            ["deno.json", "deno.jsonc", "package.json"]
        );
        assert!(!DENO_CONFIG_FILE_NAMES.contains(&"package.json"));

        let root = std::env::temp_dir().join("libdeno-root-node-modules");
        let equivalent = std::env::temp_dir()
            .join(".")
            .join("libdeno-root-node-modules");
        let other = std::env::temp_dir().join("libdeno-other-node-modules");

        assert!(root_node_modules_path_equal(Some(&root), Some(&equivalent)));
        assert!(root_node_modules_path_equal(None, None));
        assert!(!root_node_modules_path_equal(None, Some(&root)));
        assert!(!root_node_modules_path_equal(Some(&root), Some(&other)));
    }

    #[cfg(unix)]
    #[test]
    fn semantic_probe_rejects_node_modules_root_path_transition() {
        let _semantic_lock = semantic_probe_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&["HOME"]);
        let dir = std::env::temp_dir().join(format!(
            "libdeno-node-modules-mode-transition-{}",
            std::process::id()
        ));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        let node_modules = dir.join("node_modules");
        let node_modules_a = dir.join("node_modules-a");
        let node_modules_b = dir.join("node_modules-b");
        std::fs::create_dir_all(&node_modules_a).unwrap();
        std::fs::create_dir_all(&node_modules_b).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"node-modules-transition","dependencies":{}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
        std::os::unix::fs::symlink(&node_modules_a, &node_modules).unwrap();
        std::env::set_var("HOME", &home);

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        assert!(resolver.use_byonm().unwrap());
        let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutations_for_hook = mutations.clone();
        let node_modules_for_hook = node_modules.clone();
        let node_modules_b_for_hook = node_modules_b.clone();
        set_semantic_probe_test_hook(move || {
            if mutations_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::remove_file(&node_modules_for_hook).unwrap();
                std::os::unix::fs::symlink(&node_modules_b_for_hook, &node_modules_for_hook)
                    .unwrap();
            }
        });
        let result = resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver);
        clear_semantic_probe_test_hook();

        let error = match result {
            Ok(_) => panic!("node_modules mode transition was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), RESOLVER_INPUTS_CHANGED);
        assert!(mutations.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_probe_detects_symlink_retarget_to_existing_target() {
        let dir =
            std::env::temp_dir().join(format!("libdeno-symlink-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target_a = dir.join("target-a");
        let target_b = dir.join("target-b");
        let link = dir.join("link");
        std::fs::write(&target_a, b"same target bytes").unwrap();
        std::fs::write(&target_b, b"same target bytes").unwrap();
        std::os::unix::fs::symlink(&target_a, &link).unwrap();

        let baseline = file_probe(link.clone(), FileProbeKind::Content);
        assert_eq!(baseline.path, absolute_path(&link));
        assert_eq!(
            current_file_fingerprint(&baseline.path, baseline.kind),
            baseline.fingerprint
        );

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&target_b, &link).unwrap();
        assert_ne!(
            current_file_fingerprint(&baseline.path, baseline.kind),
            baseline.fingerprint,
            "retargeting an existing symlink target must invalidate the probe"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cheap_probe_detects_same_size_npmrc_edit_without_discovery() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("libdeno-cheap-npmrc-probe-{}", std::process::id()));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"cheap-probe-project","dependencies":{}}"#,
        )
        .unwrap();
        let npmrc = dir.join(".npmrc");
        let first = "registry=https://registry-a.example/\n";
        let second = "registry=https://registry-b.example/\n";
        assert_eq!(first.len(), second.len());
        std::fs::write(&npmrc, first).unwrap();

        let old_home = std::env::var_os("HOME");
        let old_registry = std::env::var_os("NPM_CONFIG_REGISTRY");
        let old_replace = std::env::var_os("NPM_CONFIG_REPLACE_REGISTRY_HOST");
        std::env::set_var("HOME", &home);
        std::env::remove_var("NPM_CONFIG_REGISTRY");
        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(manifest.is_current().unwrap());
        assert!(manifest.probe.files.iter().any(|probe| {
            probe.path == absolute_path(&npmrc) && probe.kind == FileProbeKind::Content
        }));

        let modified = std::fs::metadata(&npmrc).unwrap().modified().unwrap();
        std::fs::write(&npmrc, second).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&npmrc)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(first.len() as u64, std::fs::metadata(&npmrc).unwrap().len());
        assert_eq!(
            modified,
            std::fs::metadata(&npmrc).unwrap().modified().unwrap()
        );
        assert!(!manifest.is_current().unwrap());

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match old_registry {
            Some(value) => std::env::set_var("NPM_CONFIG_REGISTRY", value),
            None => std::env::remove_var("NPM_CONFIG_REGISTRY"),
        }
        match old_replace {
            Some(value) => std::env::set_var("NPM_CONFIG_REPLACE_REGISTRY_HOST", value),
            None => std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cheap_probe_tracks_npmrc_environment_expansion() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("libdeno-cheap-npmrc-env-{}", std::process::id()));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"cheap-env-project","dependencies":{}}"#,
        )
        .unwrap();
        std::fs::write(dir.join(".npmrc"), "registry=${LIBDENO_TEST_REGISTRY}\n").unwrap();

        let old_home = std::env::var_os("HOME");
        let old_registry = std::env::var_os("LIBDENO_TEST_REGISTRY");
        std::env::set_var("HOME", &home);
        std::env::set_var("LIBDENO_TEST_REGISTRY", "https://registry-a.example/");

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(manifest.is_current().unwrap());

        std::env::set_var("LIBDENO_TEST_REGISTRY", "https://registry-b.example/");
        assert!(!manifest.is_current().unwrap());

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match old_registry {
            Some(value) => std::env::set_var("LIBDENO_TEST_REGISTRY", value),
            None => std::env::remove_var("LIBDENO_TEST_REGISTRY"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jsr_url_environment_change_invalidates_manifest() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&["HOME", "JSR_URL"]);
        let dir =
            std::env::temp_dir().join(format!("libdeno-jsr-url-probe-{}", std::process::id()));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"jsr-url-probe","dependencies":{}}"#,
        )
        .unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("JSR_URL", "https://jsr-a.example/");

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(manifest.is_current().unwrap());

        std::env::set_var("JSR_URL", "https://jsr-b.example/");
        assert!(!manifest.is_current().unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn lockfile_content_probe_detects_same_size_edit_with_unchanged_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "libdeno-lockfile-content-probe-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lockfile = dir.join("deno.lock");
        let first = b"{\"version\":4,\"packages\":{}}";
        let second = b"{\"version\":4,\"packages\":[]}";
        assert_eq!(first.len(), second.len());
        std::fs::write(&lockfile, first).unwrap();
        let baseline = file_probe(lockfile.clone(), FileProbeKind::Content);
        let modified = std::fs::metadata(&lockfile).unwrap().modified().unwrap();

        std::fs::write(&lockfile, second).unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&lockfile)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();

        assert_ne!(
            current_file_fingerprint(&baseline.path, baseline.kind),
            baseline.fingerprint
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn global_npmrc_location_change_invalidates_and_rebuilds_auth_state() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = TestEnvGuard::new(&[
            "HOME",
            "USERPROFILE",
            "NPM_CONFIG_REGISTRY",
            "NPM_CONFIG_REPLACE_REGISTRY_HOST",
        ]);
        let dir = std::env::temp_dir().join(format!(
            "libdeno-global-npmrc-location-{}",
            std::process::id()
        ));
        let home_a = dir.join("home-a");
        let home_b = dir.join("home-b");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home_a).unwrap();
        std::fs::create_dir_all(&home_b).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"global-npmrc-location","dependencies":{}}"#,
        )
        .unwrap();
        // Deliberately do not inspect or retain this credential; the rebuilt
        // manifest is checked only through its non-reusable/cache-disabled state.
        std::fs::write(
            home_b.join(".npmrc"),
            "//registry.example/:_authToken=fixture-only\n",
        )
        .unwrap();
        std::env::remove_var("NPM_CONFIG_REGISTRY");
        std::env::remove_var("NPM_CONFIG_REPLACE_REGISTRY_HOST");
        std::env::set_var("HOME", &home_a);
        std::env::set_var("USERPROFILE", &home_a);

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let old_manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(old_manifest.is_current().unwrap());
        assert!(old_manifest.is_reusable().unwrap());

        std::env::set_var("HOME", &home_b);
        std::env::set_var("USERPROFILE", &home_b);
        assert!(!old_manifest.is_current().unwrap());
        assert!(!old_manifest.is_reusable().unwrap());

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        factory.workspace_directory().unwrap();
        factory.npmrc_with_path().unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let rebuilt_manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(!rebuilt_manifest.is_reusable().unwrap());
        assert!(managed_snapshot_key(&rebuilt_manifest).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resolved_auth_manifest_disables_lookup_and_save_key() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "libdeno-resolver-auth-manifest-{}",
            std::process::id()
        ));
        let home = dir.join("home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"auth-project","dependencies":{}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".npmrc"),
            "registry=https://registry.example/\n//registry.example/:_authToken=secret-token\n",
        )
        .unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let factory = new_workspace_factory(dir.clone(), vec![dir.clone()]).unwrap();
        let resolver = new_resolver_factory(
            factory.clone(),
            crate::analysis_cache::node_analysis_cache(),
        );
        let manifest =
            resolver_input_manifest(dir.clone(), vec![dir.clone()], &factory, &resolver).unwrap();
        assert!(managed_snapshot_key(&manifest).is_none());
        assert!(!manifest.is_reusable().unwrap());

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
