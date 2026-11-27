//! Manifest, lockfile, resolver, and local registry support for Star projects.

use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST_FILE: &str = "star.toml";
pub const LOCK_FILE: &str = "star.lock";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub package: PackageMetadata,
    #[serde(default)]
    pub build: BuildConfig,
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    #[serde(default = "default_edition")]
    pub edition: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BuildConfig {
    pub entry: PathBuf,
    pub target: String,
    pub optimization: u8,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            entry: PathBuf::from("src/main.star"),
            target: "bytecode".into(),
            optimization: 0,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Permissions {
    pub filesystem: bool,
    pub network: bool,
    pub process: bool,
    pub ffi: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Dependency {
    Version(String),
    Source(DependencySource),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DependencySource {
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lockfile {
    pub lock_version: u32,
    pub packages: Vec<LockedPackage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: String,
    pub checksum: String,
}

#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    pub root: PathBuf,
    pub locked: LockedPackage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleInitialization {
    Ready,
    Start,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModuleState {
    Initializing,
    Initialized,
    Failed,
}

/// Per-execution-context module initialization tracking, including cycle detection.
#[derive(Default)]
pub struct ModuleExecutionContext {
    states: HashMap<PathBuf, ModuleState>,
}

impl ModuleExecutionContext {
    pub fn begin(&mut self, path: &Path) -> Result<ModuleInitialization, PackageError> {
        let key = path.canonicalize()?;
        match self.states.get(&key) {
            Some(ModuleState::Initialized) => Ok(ModuleInitialization::Ready),
            Some(ModuleState::Initializing) => Err(PackageError::Invalid(format!(
                "circular module access before initialization: {}",
                key.display()
            ))),
            Some(ModuleState::Failed) => Err(PackageError::Invalid(format!(
                "module initialization previously failed: {}",
                key.display()
            ))),
            None => {
                self.states.insert(key, ModuleState::Initializing);
                Ok(ModuleInitialization::Start)
            }
        }
    }

    pub fn finish(&mut self, path: &Path) -> Result<(), PackageError> {
        let key = path.canonicalize()?;
        if self.states.get(&key) != Some(&ModuleState::Initializing) {
            return Err(PackageError::Invalid(
                "module initialization state mismatch".into(),
            ));
        }
        self.states.insert(key, ModuleState::Initialized);
        Ok(())
    }

    pub fn fail(&mut self, path: &Path) -> Result<(), PackageError> {
        let key = path.canonicalize()?;
        self.states.insert(key, ModuleState::Failed);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ModuleSearchPaths {
    pub current_source: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub installed_dependencies: Option<PathBuf>,
    pub standard_library: Option<PathBuf>,
    pub star_path: Vec<PathBuf>,
    pub filesystem_read: bool,
    pub environment_read: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedModule {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub via_star_path: bool,
}

/// Resolve a module in the QLM-defined order and reject traversal/symlink escapes.
pub fn resolve_module(
    name: &str,
    search: &ModuleSearchPaths,
) -> Result<ResolvedModule, PackageError> {
    let relative = module_relative_path(name)?;
    let mut roots = Vec::new();
    if let Some(source) = &search.current_source {
        if let Some(parent) = source.parent() {
            roots.push((parent.to_path_buf(), false));
        }
    }
    if let Some(root) = &search.project_root {
        roots.push((root.join("src"), false));
    }
    if let Some(root) = &search.installed_dependencies {
        roots.push((root.clone(), false));
    }
    if let Some(root) = &search.standard_library {
        roots.push((root.clone(), false));
    }
    let extensions = match relative
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(_) => vec![relative.clone()],
        None => vec![
            relative.with_extension("star"),
            relative.with_extension("starc"),
        ],
    };
    let mut denied = false;
    for (root, via_star_path) in roots {
        if !search.filesystem_read {
            denied = true;
            continue;
        }
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        for relative_path in &extensions {
            let candidate = canonical_root.join(relative_path);
            let Ok(path) = candidate.canonicalize() else {
                continue;
            };
            if !path.starts_with(&canonical_root) || !path.is_file() {
                continue;
            }
            return Ok(ResolvedModule {
                path,
                relative_path: relative_path.clone(),
                via_star_path,
            });
        }
    }
    if !search.star_path.is_empty() {
        if !search.environment_read {
            return Err(PackageError::Invalid(
                "env.read permission is required to consult STAR_PATH".into(),
            ));
        }
        for root in &search.star_path {
            if !search.filesystem_read {
                denied = true;
                continue;
            }
            let Ok(canonical_root) = root.canonicalize() else {
                continue;
            };
            for relative_path in &extensions {
                let candidate = canonical_root.join(relative_path);
                let Ok(path) = candidate.canonicalize() else {
                    continue;
                };
                if path.starts_with(&canonical_root) && path.is_file() {
                    return Ok(ResolvedModule {
                        path,
                        relative_path: relative_path.clone(),
                        via_star_path: true,
                    });
                }
            }
        }
    }
    if denied {
        return Err(PackageError::Invalid(
            "fs.read permission is required to resolve a file module".into(),
        ));
    }
    Err(PackageError::Invalid(format!(
        "module `{name}` was not found"
    )))
}

fn module_relative_path(name: &str) -> Result<PathBuf, PackageError> {
    let normalized = if name.contains('/')
        || name.ends_with(".star")
        || name.ends_with(".starc")
        || name.starts_with('.')
    {
        name.to_owned()
    } else {
        name.replace('.', "/")
    };
    let path = PathBuf::from(normalized);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(PackageError::Invalid(format!(
            "invalid module path `{name}`"
        )));
    }
    Ok(path)
}

#[derive(Debug)]
pub enum PackageError {
    Io(io::Error),
    Invalid(String),
    Toml(String),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Invalid(message) | Self::Toml(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PackageError {}
impl From<io::Error> for PackageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn schema_version() -> u32 {
    1
}
fn default_edition() -> String {
    "1".into()
}

impl Manifest {
    pub fn read(project: &Path) -> Result<Self, PackageError> {
        let path = locate_manifest(project)?;
        let source = fs::read_to_string(&path)?;
        let manifest: Self =
            toml::from_str(&source).map_err(|error| PackageError::Toml(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn write(&self, project: &Path) -> Result<(), PackageError> {
        self.validate()?;
        let text =
            toml::to_string_pretty(self).map_err(|error| PackageError::Toml(error.to_string()))?;
        fs::write(project.join(MANIFEST_FILE), text)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), PackageError> {
        validate_name(&self.package.name)?;
        Version::parse(&self.package.version).map_err(|_| {
            PackageError::Invalid("package version must be semantic versioning".into())
        })?;
        if self.schema_version != 1 {
            return Err(PackageError::Invalid(
                "unsupported star.toml schema_version".into(),
            ));
        }
        if self.package.edition != "1" {
            return Err(PackageError::Invalid(
                "only Star edition 1 is supported".into(),
            ));
        }
        if self.build.optimization > 3 {
            return Err(PackageError::Invalid(
                "optimization must be between 0 and 3".into(),
            ));
        }
        if !matches!(self.build.target.as_str(), "source" | "bytecode") {
            return Err(PackageError::Invalid(
                "build.target must be `source` or `bytecode`".into(),
            ));
        }
        validate_relative_path(&self.build.entry)?;
        for (name, dependency) in &self.dependencies {
            validate_name(name)?;
            let source = dependency.source();
            let choices = usize::from(source.version.is_some())
                + usize::from(source.path.is_some())
                + usize::from(source.git.is_some());
            if matches!(dependency, Dependency::Version(_)) || choices == 1 {
                if let Some(version) = source.version.as_deref() {
                    VersionReq::parse(version).map_err(|_| {
                        PackageError::Invalid(format!("invalid version requirement for `{name}`"))
                    })?;
                }
                if let Some(path) = source.path.as_deref() {
                    validate_relative_path(path)?;
                }
                if source.git.is_some() && !source.rev.as_deref().is_some_and(valid_git_revision) {
                    return Err(PackageError::Invalid(format!(
                        "git dependency `{name}` must pin a full 40- or 64-character commit ID"
                    )));
                }
            } else {
                return Err(PackageError::Invalid(format!(
                    "dependency `{name}` must specify exactly one of version, path, or git"
                )));
            }
        }
        Ok(())
    }
}

impl Dependency {
    pub fn source(&self) -> DependencySource {
        match self {
            Self::Version(version) => DependencySource {
                version: Some(version.clone()),
                ..Default::default()
            },
            Self::Source(source) => source.clone(),
        }
    }
}

fn validate_name(name: &str) -> Result<(), PackageError> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(PackageError::Invalid(format!(
            "invalid package name `{name}`"
        )));
    }
    Ok(())
}

fn valid_git_revision(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_relative_path(path: &Path) -> Result<(), PackageError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(PackageError::Invalid(format!(
            "package path `{}` must be project-relative and cannot traverse parents",
            path.display()
        )));
    }
    Ok(())
}

pub fn locate_manifest(project: &Path) -> Result<PathBuf, PackageError> {
    for name in [MANIFEST_FILE, "Star.toml"] {
        let path = project.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(PackageError::Invalid(format!(
        "no `{MANIFEST_FILE}` found in {}",
        project.display()
    )))
}

pub fn read_lock(project: &Path) -> Result<Option<Lockfile>, PackageError> {
    let path = project.join(LOCK_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let source = fs::read_to_string(path)?;
    let lock: Lockfile =
        toml::from_str(&source).map_err(|error| PackageError::Toml(error.to_string()))?;
    if lock.lock_version != 1 {
        return Err(PackageError::Invalid(
            "unsupported star.lock version".into(),
        ));
    }
    Ok(Some(lock))
}

pub fn write_lock(project: &Path, lock: &Lockfile) -> Result<(), PackageError> {
    let mut packages = lock.packages.clone();
    packages
        .sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    let text = toml::to_string_pretty(&Lockfile {
        lock_version: 1,
        packages,
    })
    .map_err(|error| PackageError::Toml(error.to_string()))?;
    fs::write(project.join(LOCK_FILE), text)?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct LocalRegistry {
    root: PathBuf,
}

impl LocalRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn search(&self, query: &str) -> Result<Vec<(String, String, String)>, PackageError> {
        let mut matches = Vec::new();
        if !self.root.exists() {
            return Ok(matches);
        }
        for package_dir in fs::read_dir(&self.root)? {
            let package_dir = package_dir?;
            if !package_dir.file_type()?.is_dir() {
                continue;
            }
            let name = package_dir.file_name().to_string_lossy().into_owned();
            if !name.contains(query) {
                continue;
            }
            for version_dir in fs::read_dir(package_dir.path())? {
                let version_dir = version_dir?;
                if !version_dir.file_type()?.is_dir() {
                    continue;
                }
                let Ok(manifest) = Manifest::read(&version_dir.path()) else {
                    continue;
                };
                matches.push((
                    name.clone(),
                    manifest.package.version,
                    manifest.package.description,
                ));
            }
        }
        matches.sort();
        Ok(matches)
    }

    pub fn resolve(
        &self,
        name: &str,
        requirement: &str,
    ) -> Result<(PathBuf, Manifest, String), PackageError> {
        validate_name(name)?;
        let requirement = VersionReq::parse(requirement).map_err(|_| {
            PackageError::Invalid(format!("invalid version requirement `{requirement}`"))
        })?;
        let base = self.root.join(name);
        let mut candidates = Vec::new();
        if base.exists() {
            for entry in fs::read_dir(&base)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let Ok(manifest) = Manifest::read(&entry.path()) else {
                    continue;
                };
                let Ok(version) = Version::parse(&manifest.package.version) else {
                    continue;
                };
                if requirement.matches(&version) {
                    candidates.push((version, entry.path(), manifest));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let Some((_version, path, manifest)) = candidates.into_iter().next() else {
            return Err(PackageError::Invalid(format!(
                "no local registry package `{name}` matches `{requirement}`"
            )));
        };
        let checksum = source_checksum(&path)?;
        Ok((path, manifest, checksum))
    }

    pub fn publish(&self, project: &Path) -> Result<(PathBuf, String), PackageError> {
        let manifest = Manifest::read(project)?;
        let version = Version::parse(&manifest.package.version)
            .map_err(|_| PackageError::Invalid("invalid package version".into()))?;
        let target = self
            .root
            .join(&manifest.package.name)
            .join(version.to_string());
        if target.exists() {
            return Err(PackageError::Invalid(
                "registry versions are immutable".into(),
            ));
        }
        fs::create_dir_all(target.parent().expect("registry package parent"))?;
        copy_package_tree(project, &target)?;
        let checksum = source_checksum(&target)?;
        Ok((target, checksum))
    }
}

pub fn source_checksum(root: &Path) -> Result<String, PackageError> {
    let root = root.canonicalize()?;
    let files = package_files(&root)?;
    let mut hash = Sha256::new();
    for path in files {
        let relative = path
            .strip_prefix(&root)
            .expect("collected package path")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(&path)?;
        hash.update((relative.len() as u64).to_be_bytes());
        hash.update(relative.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    Ok(hex(&hash.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn package_files(root: &Path) -> Result<Vec<PathBuf>, PackageError> {
    let root = root.canonicalize()?;
    let mut pending = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(directory)? {
            let item = item?;
            let file_type = item.file_type()?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), ".git" | ".star" | "target" | "deps") {
                continue;
            }
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(item.path());
            } else if file_type.is_file() {
                files.push(item.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn copy_package_tree(source: &Path, destination: &Path) -> Result<(), PackageError> {
    let source = source.canonicalize()?;
    fs::create_dir_all(destination)?;
    for file in package_files(&source)? {
        let relative = file
            .strip_prefix(&source)
            .map_err(|_| PackageError::Invalid("package file escaped project root".into()))?;
        let target = destination.join(relative);
        fs::create_dir_all(target.parent().expect("package file parent"))?;
        fs::copy(file, target)?;
    }
    Ok(())
}

pub fn resolve_dependencies(
    project: &Path,
    registry: &LocalRegistry,
) -> Result<(Lockfile, Vec<ResolvedPackage>), PackageError> {
    let root = project.canonicalize()?;
    let manifest = Manifest::read(&root)?;
    let mut output = Vec::new();
    let mut seen = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    for (name, dependency) in &manifest.dependencies {
        resolve_one(
            &root,
            name,
            dependency,
            registry,
            &mut seen,
            &mut visiting,
            &mut output,
        )?;
    }
    output.sort_by(|a, b| {
        a.locked
            .name
            .cmp(&b.locked.name)
            .then(a.locked.version.cmp(&b.locked.version))
    });
    let packages = output
        .iter()
        .map(|resolved| resolved.locked.clone())
        .collect();
    Ok((
        Lockfile {
            lock_version: 1,
            packages,
        },
        output,
    ))
}

fn resolve_one(
    parent: &Path,
    name: &str,
    dependency: &Dependency,
    registry: &LocalRegistry,
    seen: &mut BTreeSet<String>,
    visiting: &mut BTreeSet<String>,
    output: &mut Vec<ResolvedPackage>,
) -> Result<(), PackageError> {
    let source = dependency.source();
    let (root, manifest, checksum, source_id) = if let Some(relative) = source.path {
        validate_relative_path(&relative)?;
        let root = parent.join(&relative).canonicalize()?;
        if !root.starts_with(parent) {
            return Err(PackageError::Invalid(format!(
                "path dependency `{name}` escapes its parent package"
            )));
        }
        let manifest = Manifest::read(&root)?;
        if manifest.package.name != name {
            return Err(PackageError::Invalid(format!(
                "path dependency `{name}` has package name `{}`",
                manifest.package.name
            )));
        }
        let checksum = source_checksum(&root)?;
        (
            root,
            manifest,
            checksum,
            format!("path+{}", relative.to_string_lossy().replace('\\', "/")),
        )
    } else if let Some(url) = source.git {
        let revision = source.rev.as_deref().ok_or_else(|| {
            PackageError::Invalid(format!("git dependency `{name}` has no pinned revision"))
        })?;
        if !valid_git_revision(revision) {
            return Err(PackageError::Invalid(format!(
                "git dependency `{name}` must pin a full commit ID"
            )));
        }
        let cache = parent
            .join(".star")
            .join("git")
            .join(format!("{name}-{revision}"));
        if !cache.exists() {
            fs::create_dir_all(cache.parent().expect("git cache parent"))?;
            let clone = Command::new("git")
                .arg("clone")
                .arg("--no-checkout")
                .arg("--filter=blob:none")
                .arg(&url)
                .arg(&cache)
                .output()?;
            if !clone.status.success() {
                return Err(PackageError::Invalid(format!(
                    "git clone for `{name}` failed: {}",
                    String::from_utf8_lossy(&clone.stderr).trim()
                )));
            }
            let checkout = Command::new("git")
                .arg("-C")
                .arg(&cache)
                .arg("checkout")
                .arg("--detach")
                .arg(revision)
                .output()?;
            if !checkout.status.success() {
                return Err(PackageError::Invalid(format!(
                    "git revision for `{name}` is unavailable: {}",
                    String::from_utf8_lossy(&checkout.stderr).trim()
                )));
            }
        }
        let remote = Command::new("git")
            .arg("-C")
            .arg(&cache)
            .args(["remote", "get-url", "origin"])
            .output()?;
        if !remote.status.success() || String::from_utf8_lossy(&remote.stdout).trim() != url {
            return Err(PackageError::Invalid(format!(
                "cached git source for `{name}` does not match its manifest"
            )));
        }
        let head = Command::new("git")
            .arg("-C")
            .arg(&cache)
            .args(["rev-parse", "HEAD"])
            .output()?;
        if !head.status.success()
            || !String::from_utf8_lossy(&head.stdout)
                .trim()
                .eq_ignore_ascii_case(revision)
        {
            return Err(PackageError::Invalid(format!(
                "cached git source for `{name}` is not at the pinned revision"
            )));
        }
        let status = Command::new("git")
            .arg("-C")
            .arg(&cache)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()?;
        if !status.status.success() || !status.stdout.is_empty() {
            return Err(PackageError::Invalid(format!(
                "cached git source for `{name}` has local changes"
            )));
        }
        let root = cache.canonicalize()?;
        let manifest = Manifest::read(&root)?;
        if manifest.package.name != name {
            return Err(PackageError::Invalid(format!(
                "git dependency `{name}` has package name `{}`",
                manifest.package.name
            )));
        }
        let checksum = source_checksum(&root)?;
        (root, manifest, checksum, format!("git+{url}#{revision}"))
    } else {
        let requirement = source.version.as_deref().unwrap_or("*");
        let (root, manifest, checksum) = registry.resolve(name, requirement)?;
        (root, manifest, checksum, "registry".to_owned())
    };
    if let Some(requirement) = source.version.as_deref() {
        let requirement = VersionReq::parse(requirement).map_err(|_| {
            PackageError::Invalid(format!("invalid version requirement for `{name}`"))
        })?;
        let version = Version::parse(&manifest.package.version).map_err(|_| {
            PackageError::Invalid(format!("invalid version for dependency `{name}`"))
        })?;
        if !requirement.matches(&version) {
            return Err(PackageError::Invalid(format!(
                "dependency `{name}` does not match its version requirement"
            )));
        }
    }
    let package_key = format!("{}@{}", manifest.package.name, manifest.package.version);
    if visiting.contains(&package_key) {
        return Err(PackageError::Invalid(format!(
            "dependency cycle detected at `{package_key}`"
        )));
    }
    if seen.contains(&package_key) {
        return Ok(());
    }
    visiting.insert(package_key.clone());
    for (child_name, child) in &manifest.dependencies {
        resolve_one(&root, child_name, child, registry, seen, visiting, output)?;
    }
    visiting.remove(&package_key);
    seen.insert(package_key);
    output.push(ResolvedPackage {
        root,
        locked: LockedPackage {
            name: manifest.package.name,
            version: manifest.package.version,
            source: source_id,
            checksum,
        },
    });
    Ok(())
}

pub fn install_tree(source: &Path, destination: &Path) -> Result<(), PackageError> {
    copy_package_tree(source, destination)
}

pub fn write_deterministic_package(
    project: &Path,
    destination: &Path,
) -> Result<String, PackageError> {
    let project = project.canonicalize()?;
    Manifest::read(&project)?;
    let files = package_files(&project)?;
    let mut output = fs::File::create(destination)?;
    output.write_all(b"STARPKG1")?;
    let file_count = u32::try_from(files.len())
        .map_err(|_| PackageError::Invalid("package contains too many files".into()))?;
    output.write_all(&file_count.to_be_bytes())?;
    for file in files {
        let relative = file
            .strip_prefix(&project)
            .map_err(|_| PackageError::Invalid("package file escaped project root".into()))?
            .to_string_lossy()
            .replace('\\', "/");
        if relative.len() > u32::MAX as usize {
            return Err(PackageError::Invalid("package path is too long".into()));
        }
        let bytes = fs::read(&file)?;
        output.write_all(&(relative.len() as u32).to_be_bytes())?;
        output.write_all(&(bytes.len() as u64).to_be_bytes())?;
        output.write_all(relative.as_bytes())?;
        output.write_all(&bytes)?;
    }
    output.flush()?;
    let checksum = source_checksum(&project)?;
    Ok(checksum)
}
