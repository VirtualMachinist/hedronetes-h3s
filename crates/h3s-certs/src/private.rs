//! Small, explicit runtime credential-file operations. Secret contents never
//! enter errors. Paths must be inside operator-selected private directories.
use crate::{Error, Result};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

pub fn directory(path: &Path) -> Result<()> {
    super::private_directory(path)
}
pub fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    check(&file)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid("credential file exceeds size limit".into()));
    }
    Ok(bytes)
}
fn check(file: &fs::File) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Invalid("credential must be a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid(
                "credential requires private mode 0600".into(),
            ));
        }
    }
    Ok(())
}
pub fn write(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("credential parent directory required".into()))?;
    directory(parent)?;
    if replace && path.try_exists()? {
        let _ = read(path, 1024 * 1024)?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if replace {
        temp.persist(path).map_err(|e| Error::Io(e.error))?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| Error::Io(e.error))?;
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
/// Hold the returned file for the process lifetime. A second agent must fail,
/// not silently start another reconciler for the same local state directory.
pub fn exclusive_process_lock(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    check(&file)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}
