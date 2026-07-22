//! Narrow filesystem helpers for private control-plane state.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileOwnership {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

pub fn dir_ownership(dir: &Path, mode: u32) -> std::io::Result<FileOwnership> {
    let metadata = fs::metadata(dir)?;
    Ok(FileOwnership {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode,
    })
}

/// Write state atomically in the destination directory and preserve the
/// bind-mounted directory owner's host visibility.
pub fn write_atomic(path: &Path, content: &str, ownership: FileOwnership) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".simchain-write.")
        .tempfile_in(dir)?;
    temp.write_all(content.as_bytes())?;
    temp.flush()?;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(ownership.mode))?;
    if let Err(error) =
        std::os::unix::fs::chown(temp.path(), Some(ownership.uid), Some(ownership.gid))
    {
        tracing::debug!(
            uid = ownership.uid,
            gid = ownership.gid,
            "could not align control-state file ownership: {error}"
        );
    }
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    // The rename above is the transaction commit point. A directory-fsync
    // failure after that point must not be reported as a pre-commit failure:
    // callers would otherwise roll runtime state back while the new file is
    // already visible. The file itself was synced before the atomic rename.
    if let Err(error) = File::open(dir).and_then(|directory| directory.sync_all()) {
        tracing::warn!(
            path = %path.display(),
            "could not fsync the control-state directory after commit: {error}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_applies_private_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let ownership = dir_ownership(dir.path(), 0o600).expect("ownership");
        write_atomic(&path, "{}\n", ownership).expect("write");
        let metadata = fs::metadata(&path).expect("metadata");
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(path).expect("read"), "{}\n");
    }
}
