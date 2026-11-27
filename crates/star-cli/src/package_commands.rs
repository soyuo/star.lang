use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use star_package::{
    install_tree, read_lock, resolve_dependencies, resolve_dependencies_from_lock, source_checksum,
    write_deterministic_package, write_lock, Dependency, DependencySource, LocalRegistry, Manifest,
    PackageError,
};

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 2;
const EXIT_IO: u8 = 3;

pub fn run(command: &str, arguments: &[String]) -> u8 {
    let project = match env::current_dir() {
        Ok(path) => path,
        Err(error) => return report(error),
    };
    let registry = local_registry();
    let result = match command {
        "add" => add(&project, arguments),
        "remove" => remove(&project, arguments),
        "install" => install(&project, &registry),
        "update" => update(&project, &registry),
        "search" => search(&registry, arguments),
        "package" => package(&project),
        "publish" => publish(&project, &registry),
        "audit" => audit(&project, &registry),
        "login" if arguments.is_empty() => {
            println!("the configured local registry requires no login; set STAR_REGISTRY_HOME to select it");
            Ok(())
        }
        "login" => Err(PackageError::Invalid("usage: star login".into())),
        _ => Err(PackageError::Invalid("unknown package command".into())),
    };
    match result {
        Ok(()) => EXIT_OK,
        Err(PackageError::Invalid(message)) if message.starts_with("usage:") => {
            eprintln!("star: {message}");
            EXIT_USAGE
        }
        Err(error) => report(error),
    }
}

pub fn local_registry() -> LocalRegistry {
    LocalRegistry::new(registry_path())
}

fn registry_path() -> PathBuf {
    if let Some(path) = env::var_os("STAR_REGISTRY_HOME") {
        return PathBuf::from(path);
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".star").join("registry")
}

fn add(project: &Path, arguments: &[String]) -> Result<(), PackageError> {
    let Some(name) = arguments.first() else {
        return Err(usage("star add <name> [--version <requirement>|--path <directory>|--git <url> --rev <revision>]"));
    };
    let mut dependency = Dependency::Version("*".into());
    let mut index = 1;
    let mut source = DependencySource::default();
    while index < arguments.len() {
        let option = arguments[index].as_str();
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| usage("star add is missing an option value"))?;
        match option {
            "--version" => source.version = Some(value.clone()),
            "--path" => {
                source.version = None;
                source.path = Some(PathBuf::from(value));
            }
            "--git" => {
                source.version = None;
                source.git = Some(value.clone());
            }
            "--rev" => source.rev = Some(value.clone()),
            _ => return Err(usage("unknown star add option")),
        }
        index += 2;
    }
    if source.rev.is_some() != source.git.is_some() {
        return Err(PackageError::Invalid(
            "--rev must be supplied exactly with --git".into(),
        ));
    }
    if source.path.is_some() || source.git.is_some() {
        dependency = Dependency::Source(source);
    } else if let Some(version) = source.version {
        dependency = Dependency::Version(version);
    }
    let mut manifest = Manifest::read(project)?;
    manifest.dependencies.insert(name.clone(), dependency);
    manifest.write(project)?;
    println!(
        "added `{name}` to {}",
        project.join(star_package::MANIFEST_FILE).display()
    );
    Ok(())
}

fn remove(project: &Path, arguments: &[String]) -> Result<(), PackageError> {
    if arguments.len() != 1 {
        return Err(usage("star remove <name>"));
    }
    let mut manifest = Manifest::read(project)?;
    if manifest.dependencies.remove(&arguments[0]).is_none() {
        return Err(PackageError::Invalid(format!(
            "dependency `{}` is not declared",
            arguments[0]
        )));
    }
    manifest.write(project)?;
    println!("removed `{}`", arguments[0]);
    Ok(())
}

fn install(project: &Path, registry: &LocalRegistry) -> Result<(), PackageError> {
    let existing = read_lock(project)?;
    let write_new_lock = existing.is_none();
    let (lock, packages) = if let Some(existing) = existing.as_ref() {
        let resolved = resolve_dependencies_from_lock(project, registry, existing)?;
        if !same_lock(existing, &resolved.0) {
            return Err(PackageError::Invalid(
                "star.lock does not match the manifest or dependency sources; run `star update`"
                    .into(),
            ));
        }
        resolved
    } else {
        resolve_dependencies(project, registry)?
    };
    install_packages(project, &lock, &packages)?;
    if write_new_lock {
        write_lock(project, &lock)?;
    }
    Ok(())
}

fn update(project: &Path, registry: &LocalRegistry) -> Result<(), PackageError> {
    let (lock, packages) = resolve_dependencies(project, registry)?;
    install_packages(project, &lock, &packages)?;
    write_lock(project, &lock)
}

fn same_lock(left: &star_package::Lockfile, right: &star_package::Lockfile) -> bool {
    let mut left = left.packages.clone();
    let mut right = right.packages.clone();
    left.sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    right.sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    left == right
}

fn install_packages(
    project: &Path,
    lock: &star_package::Lockfile,
    packages: &[star_package::ResolvedPackage],
) -> Result<(), PackageError> {
    let project_root = project.canonicalize()?;
    for package in packages {
        if package.locked.source.starts_with("path+") {
            continue;
        }
        let parent =
            ensure_project_directory(&project_root, Path::new("deps").join(&package.locked.name))?;
        let destination = parent.join(&package.locked.version);
        if fs::symlink_metadata(&destination).is_ok() {
            let canonical = destination.canonicalize().map_err(|_| {
                PackageError::Invalid("installed dependency path is not accessible".into())
            })?;
            if !canonical.starts_with(&project_root) {
                return Err(PackageError::Invalid(
                    "installed dependency path escapes the project".into(),
                ));
            }
            if source_checksum(&destination)? != package.locked.checksum {
                return Err(PackageError::Invalid(format!(
                    "installed dependency `{}` checksum does not match star.lock",
                    package.locked.name
                )));
            }
            continue;
        }
        install_tree(&package.root, &destination)?;
    }
    println!("installed {} package(s)", lock.packages.len());
    Ok(())
}

fn ensure_project_directory(
    project_root: &Path,
    relative: PathBuf,
) -> Result<PathBuf, PackageError> {
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(PackageError::Invalid("invalid install directory".into()));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() || metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(PackageError::Invalid(
                    "install path component is not a directory".into(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(PackageError::Io(error)),
        }
        current = current.canonicalize()?;
        if !current.starts_with(project_root) {
            return Err(PackageError::Invalid(
                "install path escapes the project".into(),
            ));
        }
        if !current.is_dir() {
            return Err(PackageError::Invalid(
                "install path component is not a directory".into(),
            ));
        }
    }
    Ok(current)
}

fn search(registry: &LocalRegistry, arguments: &[String]) -> Result<(), PackageError> {
    if arguments.len() != 1 {
        return Err(usage("star search <query>"));
    }
    let results = registry.search(&arguments[0])?;
    if results.is_empty() {
        println!("no local packages matched `{}`", arguments[0]);
    }
    for (name, version, description) in results {
        println!("{name} {version}\t{description}");
    }
    Ok(())
}

fn package(project: &Path) -> Result<(), PackageError> {
    let manifest = Manifest::read(project)?;
    let output_dir = project.join("target").join("packages");
    fs::create_dir_all(&output_dir)?;
    let output = output_dir.join(format!(
        "{}-{}.starpkg",
        manifest.package.name, manifest.package.version
    ));
    let checksum = write_deterministic_package(project, &output)?;
    println!("created {} (sha256 {checksum})", output.display());
    Ok(())
}

fn publish(project: &Path, registry: &LocalRegistry) -> Result<(), PackageError> {
    let (path, checksum) = registry.publish(project)?;
    println!(
        "published to local registry {} (sha256 {checksum})",
        path.display()
    );
    Ok(())
}

fn audit(project: &Path, registry: &LocalRegistry) -> Result<(), PackageError> {
    let project_root = project.canonicalize()?;
    let Some(existing) = read_lock(project)? else {
        return Err(PackageError::Invalid(
            "star.lock is missing; run `star install`".into(),
        ));
    };
    let (resolved, packages) = resolve_dependencies_from_lock(project, registry, &existing)?;
    if !same_lock(&existing, &resolved) {
        return Err(PackageError::Invalid(
            "star.lock does not match the manifest; run `star update`".into(),
        ));
    }
    let mut checked = std::collections::BTreeSet::new();
    for locked in &existing.packages {
        if !checked.insert((locked.name.clone(), locked.version.clone())) {
            return Err(PackageError::Invalid(format!(
                "star.lock contains duplicate package `{}@{}`",
                locked.name, locked.version
            )));
        }
        let Some(package) = packages.iter().find(|package| {
            package.locked.name == locked.name && package.locked.version == locked.version
        }) else {
            return Err(PackageError::Invalid(format!(
                "locked package `{}@{}` is unavailable",
                locked.name, locked.version
            )));
        };
        if package.locked.checksum != locked.checksum {
            return Err(PackageError::Invalid(format!(
                "checksum mismatch for `{}@{}`",
                locked.name, locked.version
            )));
        }
        if package.locked.source != locked.source {
            return Err(PackageError::Invalid(format!(
                "source mismatch for `{}@{}`",
                locked.name, locked.version
            )));
        }
        if !package.locked.source.starts_with("path+") {
            let installed = project_root
                .join("deps")
                .join(&locked.name)
                .join(&locked.version);
            if !installed.exists() {
                return Err(PackageError::Invalid(format!(
                    "installed package `{}@{}` is missing",
                    locked.name, locked.version
                )));
            }
            let canonical_installed = installed.canonicalize().map_err(|_| {
                PackageError::Invalid(format!(
                    "installed package `{}@{}` cannot be resolved",
                    locked.name, locked.version
                ))
            })?;
            if !canonical_installed.starts_with(&project_root) {
                return Err(PackageError::Invalid(format!(
                    "installed package `{}@{}` escapes the project",
                    locked.name, locked.version
                )));
            }
            if source_checksum(&canonical_installed)? != locked.checksum {
                return Err(PackageError::Invalid(format!(
                    "installed package `{}@{}` failed checksum verification",
                    locked.name, locked.version
                )));
            }
        }
    }
    println!(
        "audit passed: {} locked package(s)",
        existing.packages.len()
    );
    Ok(())
}

fn usage(message: &str) -> PackageError {
    PackageError::Invalid(format!("usage: {message}"))
}
fn report(error: impl std::fmt::Display) -> u8 {
    eprintln!("star: package error: {error}");
    EXIT_IO
}
