//! Operating-system path helpers.
//!
//! These helpers manipulate path values without accessing the filesystem. Use
//! [`normalize_relative`] for a lexical relative path, then still pass paths
//! through a capability-scoped filesystem handle for actual access.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::{error::Error, fmt};

/// A lexical path operation rejected an absolute path or root escape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    /// The input contains a root or platform prefix.
    AbsolutePath,
    /// A `..` component would escape the relative path's starting directory.
    EscapesRoot,
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AbsolutePath => formatter.write_str("expected a relative path"),
            Self::EscapesRoot => formatter.write_str("path escapes its relative root"),
        }
    }
}

impl Error for PathError {}

/// Joins two paths using the host operating system's path rules.
///
/// This is lexical manipulation only; it does not authorize filesystem access.
pub fn join(base: &Path, child: &Path) -> PathBuf {
    base.join(child)
}

/// Returns the parent component, if the path has one.
pub fn parent(path: &Path) -> Option<&Path> {
    path.parent()
}

/// Returns the final path component, if present.
pub fn file_name(path: &Path) -> Option<&OsStr> {
    path.file_name()
}

/// Returns the final extension, if present.
pub fn extension(path: &Path) -> Option<&OsStr> {
    path.extension()
}

/// Reports whether the path begins at a filesystem root.
pub fn is_absolute(path: &Path) -> bool {
    path.is_absolute()
}

/// Lexically normalizes a relative path and rejects attempts to escape its root.
///
/// Repeated separators and `.` are removed. A `name/..` pair cancels out;
/// leading `..`, absolute roots, and platform prefixes are rejected. This does
/// not resolve symlinks and must not be used in place of capability-scoped I/O.
/// An empty path or `.` normalizes to `.`.
pub fn normalize_relative(path: &Path) -> Result<PathBuf, PathError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => return Err(PathError::AbsolutePath),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(PathError::EscapesRoot);
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Ok(normalized)
}
