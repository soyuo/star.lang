//! Capability-gated host services for filesystem, environment, and processes.
//!
//! Filesystem roots are opened by the host with ambient authority during
//! configuration. Guest paths are always resolved relative to those directory
//! handles. Process spawning is a separate, powerful capability; it does not
//! sandbox the child process from the host operating system.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
pub use star_compiler::Capability;

use super::path::{normalize_relative, PathError};

/// Host operation failed because it lacked a capability or the host operation
/// itself failed.
#[derive(Debug)]
pub enum HostError {
    /// The host did not grant the operation's required capability.
    PermissionDenied(Capability),
    /// The host granted filesystem access but did not configure its root.
    FilesystemRootNotConfigured(Capability),
    /// The requested relative path is invalid or escapes its supplied root.
    InvalidPath(PathError),
    /// The operating-system filesystem operation failed.
    Io(io::Error),
    /// An environment variable contained non-Unicode data.
    EnvironmentValueNotUnicode,
    /// Process spawning requires an absolute executable path.
    ProcessPathMustBeAbsolute,
    /// The operating system rejected process creation.
    Process(io::Error),
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied(capability) => {
                write!(formatter, "missing {} capability", capability.name())
            }
            Self::FilesystemRootNotConfigured(capability) => write!(
                formatter,
                "{} capability has no configured directory root",
                capability.name()
            ),
            Self::InvalidPath(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "filesystem error: {error}"),
            Self::EnvironmentValueNotUnicode => {
                formatter.write_str("environment value is not valid Unicode")
            }
            Self::ProcessPathMustBeAbsolute => {
                formatter.write_str("process executable path must be absolute")
            }
            Self::Process(error) => write!(formatter, "process spawn failed: {error}"),
        }
    }
}

impl std::error::Error for HostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPath(error) => Some(error),
            Self::Io(error) | Self::Process(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PathError> for HostError {
    fn from(error: PathError) -> Self {
        Self::InvalidPath(error)
    }
}

impl From<io::Error> for HostError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A deny-by-default set of host services with explicit directory roots.
pub struct HostServices {
    capabilities: BTreeSet<Capability>,
    read_root: Option<Dir>,
    write_root: Option<Dir>,
}

impl HostServices {
    /// Creates host services with exactly the supplied capabilities.
    pub fn new(capabilities: BTreeSet<Capability>) -> Self {
        Self {
            capabilities,
            read_root: None,
            write_root: None,
        }
    }

    /// Returns the immutable capability grants used by this service set.
    pub fn capabilities(&self) -> &BTreeSet<Capability> {
        &self.capabilities
    }

    /// Opens the host-selected root for guest file reads.
    ///
    /// This configuration method uses ambient host authority. Call it only from
    /// trusted host setup code; guest file operations remain relative to the
    /// resulting directory capability.
    pub fn set_read_root(&mut self, root: impl AsRef<Path>) -> Result<(), HostError> {
        self.require(Capability::FsRead)?;
        self.read_root = Some(Dir::open_ambient_dir(root, ambient_authority())?);
        Ok(())
    }

    /// Opens the host-selected root for guest file writes.
    ///
    /// This configuration method uses ambient host authority. Call it only from
    /// trusted host setup code; guest file operations remain relative to the
    /// resulting directory capability.
    pub fn set_write_root(&mut self, root: impl AsRef<Path>) -> Result<(), HostError> {
        self.require(Capability::FsWrite)?;
        self.write_root = Some(Dir::open_ambient_dir(root, ambient_authority())?);
        Ok(())
    }

    /// Reads file bytes relative to the configured read root.
    pub fn read_file(&self, path: impl AsRef<Path>) -> Result<Vec<u8>, HostError> {
        let root = self.read_root(Capability::FsRead)?;
        let path = normalize_relative(path.as_ref())?;
        Ok(root.read(path)?)
    }

    /// Reads UTF-8 text relative to the configured read root.
    pub fn read_text(&self, path: impl AsRef<Path>) -> Result<String, HostError> {
        let root = self.read_root(Capability::FsRead)?;
        let path = normalize_relative(path.as_ref())?;
        Ok(root.read_to_string(path)?)
    }

    /// Writes or replaces file bytes relative to the configured write root.
    pub fn write_file(
        &self,
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Result<(), HostError> {
        let root = self.write_root(Capability::FsWrite)?;
        let path = normalize_relative(path.as_ref())?;
        Ok(root.write(path, contents)?)
    }

    /// Creates a directory and any missing parents under the write root.
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> Result<(), HostError> {
        let root = self.write_root(Capability::FsWrite)?;
        let path = normalize_relative(path.as_ref())?;
        Ok(root.create_dir_all(path)?)
    }

    /// Removes a file under the write root.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> Result<(), HostError> {
        let root = self.write_root(Capability::FsWrite)?;
        let path = normalize_relative(path.as_ref())?;
        Ok(root.remove_file(path)?)
    }

    /// Returns sorted names from a directory under the read root.
    pub fn read_dir(&self, path: impl AsRef<Path>) -> Result<Vec<OsString>, HostError> {
        let root = self.read_root(Capability::FsRead)?;
        let path = normalize_relative(path.as_ref())?;
        let mut names = Vec::new();
        for entry in root.read_dir(path)? {
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }

    /// Reads one host environment variable.
    pub fn get_env_os(&self, name: impl AsRef<OsStr>) -> Result<Option<OsString>, HostError> {
        self.require(Capability::EnvRead)?;
        Ok(env::var_os(name))
    }

    /// Reads one host environment variable and requires its value to be Unicode.
    pub fn get_env(&self, name: impl AsRef<OsStr>) -> Result<Option<String>, HostError> {
        self.get_env_os(name)?
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| HostError::EnvironmentValueNotUnicode)
            })
            .transpose()
    }

    /// Spawns an absolute executable without a shell or inherited environment.
    ///
    /// Standard input is closed; stdout and stderr are piped for the caller to
    /// drain. Granting `process.spawn` permits child programs to use the host
    /// account's operating-system authority; this method is not OS isolation.
    pub fn spawn<I, S>(&self, program: impl AsRef<OsStr>, args: I) -> Result<Child, HostError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.require(Capability::ProcessSpawn)?;
        let program = program.as_ref();
        if !Path::new(program).is_absolute() {
            return Err(HostError::ProcessPathMustBeAbsolute);
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().map_err(HostError::Process)
    }

    fn require(&self, capability: Capability) -> Result<(), HostError> {
        if self.capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(HostError::PermissionDenied(capability))
        }
    }

    fn read_root(&self, capability: Capability) -> Result<&Dir, HostError> {
        self.require(capability)?;
        self.read_root
            .as_ref()
            .ok_or(HostError::FilesystemRootNotConfigured(capability))
    }

    fn write_root(&self, capability: Capability) -> Result<&Dir, HostError> {
        self.require(capability)?;
        self.write_root
            .as_ref()
            .ok_or(HostError::FilesystemRootNotConfigured(capability))
    }
}
