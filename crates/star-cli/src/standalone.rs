//! Appended-bytecode container used by host-native standalone applications.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const TRAILER_MAGIC: &[u8; 8] = b"STARAPP1";
const TRAILER_BYTES: u64 = 24;
pub(crate) const MAX_BYTECODE_BYTES: usize = 64 * 1024 * 1024;
const KNOWN_CAPABILITY_BITS: u64 = (1 << 9) - 1;

pub(crate) struct EmbeddedApplication {
    pub bytecode: Vec<u8>,
    pub capability_bits: u64,
}

pub(crate) fn read_current_executable() -> io::Result<Option<EmbeddedApplication>> {
    let path = std::env::current_exe()?;
    read_application(&path)
}

fn read_application(path: &Path) -> io::Result<Option<EmbeddedApplication>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < TRAILER_BYTES {
        return Ok(None);
    }

    let trailer_start = file_len - TRAILER_BYTES;
    file.seek(SeekFrom::Start(trailer_start))?;
    let mut trailer = [0_u8; TRAILER_BYTES as usize];
    file.read_exact(&mut trailer)?;
    if &trailer[16..24] != TRAILER_MAGIC {
        return Ok(None);
    }

    let bytecode_len = u64::from_le_bytes(trailer[0..8].try_into().expect("fixed trailer slice"));
    let capability_bits =
        u64::from_le_bytes(trailer[8..16].try_into().expect("fixed trailer slice"));
    if bytecode_len == 0 || bytecode_len > MAX_BYTECODE_BYTES as u64 {
        return Err(invalid_data("standalone bytecode length is invalid"));
    }
    if capability_bits & !KNOWN_CAPABILITY_BITS != 0 {
        return Err(invalid_data("standalone capability mask has unknown bits"));
    }
    let bytecode_start = trailer_start
        .checked_sub(bytecode_len)
        .ok_or_else(|| invalid_data("standalone bytecode extends before the executable"))?;
    let bytecode_size = usize::try_from(bytecode_len)
        .map_err(|_| invalid_data("standalone bytecode does not fit in memory"))?;
    let mut bytecode = vec![0; bytecode_size];
    file.seek(SeekFrom::Start(bytecode_start))?;
    file.read_exact(&mut bytecode)?;
    Ok(Some(EmbeddedApplication {
        bytecode,
        capability_bits,
    }))
}

pub(crate) fn write_application(
    runtime_path: &Path,
    output_path: &Path,
    bytecode: &[u8],
    capability_bits: u64,
) -> io::Result<()> {
    if bytecode.is_empty() || bytecode.len() > MAX_BYTECODE_BYTES {
        return Err(invalid_input(
            "compiled bytecode exceeds the standalone app limit",
        ));
    }
    if capability_bits & !KNOWN_CAPABILITY_BITS != 0 {
        return Err(invalid_input("standalone capability mask has unknown bits"));
    }

    let runtime_path = runtime_path.canonicalize()?;
    let output_parent = output_parent(output_path);
    let canonical_output = output_parent.canonicalize()?.join(
        output_path
            .file_name()
            .ok_or_else(|| invalid_input("standalone output path must name a file"))?,
    );
    let existing_output = fs::canonicalize(output_path).ok();
    if existing_output.as_deref() == Some(runtime_path.as_path())
        || canonical_output == runtime_path
    {
        return Err(invalid_input(
            "standalone output must not replace the running Star executable",
        ));
    }

    let runtime_metadata = fs::metadata(&runtime_path)?;
    let (temporary_path, mut temporary_file) = create_temporary_file(output_path)?;
    let result = (|| {
        let mut runtime_file = File::open(&runtime_path)?;
        io::copy(&mut runtime_file, &mut temporary_file)?;
        temporary_file.write_all(bytecode)?;
        temporary_file.write_all(&(bytecode.len() as u64).to_le_bytes())?;
        temporary_file.write_all(&capability_bits.to_le_bytes())?;
        temporary_file.write_all(TRAILER_MAGIC)?;
        temporary_file.sync_all()?;
        Ok(())
    })();
    drop(temporary_file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = fs::set_permissions(&temporary_path, runtime_metadata.permissions()) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = publish(&temporary_path, output_path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

fn create_temporary_file(output_path: &Path) -> io::Result<(PathBuf, File)> {
    let parent = output_parent(output_path);
    let original_name = output_path
        .file_name()
        .ok_or_else(|| invalid_input("standalone output path must name a file"))?;
    for suffix in 0..100_u32 {
        let mut name = original_name.to_os_string();
        name.push(format!(".{}.{}.tmp", std::process::id(), suffix));
        let path = parent.join(&name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique standalone output file",
    ))
}

fn publish(temporary_path: &Path, output_path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    match fs::symlink_metadata(output_path) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(invalid_input("standalone output path is a directory"));
        }
        Ok(_) => fs::remove_file(output_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(temporary_path, output_path)
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
