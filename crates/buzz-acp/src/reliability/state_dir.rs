//! The per-agent state directory that holds the ledger and the park file.
//!
//! `BUZZ_ACP_STATE_DIR` is the authority: the desktop sets it at spawn to
//! `<app data>/agents/state/<pubkey>/`. The fallback exists for a harness
//! started from a shell and is keyed by the first 16 characters of the agent's
//! public key so two agents on one machine never share a ledger.
//!
//! The directory is created 0700 and every file in it is created 0600: parked
//! batches hold client messages.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! { static WRITTEN_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(super) fn take_written_bytes() -> u64 {
    WRITTEN_BYTES.with(|count| count.replace(0))
}

/// Environment variable naming the state directory. Set explicitly by the
/// desktop at spawn; never inherited by accident (it is on the desktop's
/// reserved-env-key list, so a saved user env cannot supply it).
pub const STATE_DIR_ENV: &str = "BUZZ_ACP_STATE_DIR";

/// Characters of the agent public key used in the fallback directory name.
pub const PUBKEY_PREFIX_LEN: usize = 16;

/// Directory mode: owner-only.
#[cfg(unix)]
pub const DIR_MODE: u32 = 0o700;

/// File mode: owner read/write only.
#[cfg(unix)]
pub const FILE_MODE: u32 = 0o600;

/// Resolve, create and lock down the state directory for `pubkey_hex`.
///
/// Returns the directory. An unset or empty `BUZZ_ACP_STATE_DIR` falls back to
/// `~/.buzz/.state/<pubkey prefix>/`.
pub fn resolve_state_dir(pubkey_hex: &str) -> io::Result<PathBuf> {
    let dir = match std::env::var(STATE_DIR_ENV) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => fallback_state_dir(pubkey_hex)?,
    };
    ensure_dir(&dir)?;
    Ok(dir)
}

fn fallback_state_dir(pubkey_hex: &str) -> io::Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no home directory and no BUZZ_ACP_STATE_DIR — cannot place the agent state directory",
        )
    })?;
    Ok(home
        .join(".buzz")
        .join(".state")
        .join(pubkey_prefix(pubkey_hex)))
}

/// A filesystem-safe directory name derived from the agent public key.
///
/// Only hex characters survive: the key reaches the harness from configuration
/// and a `/` or `..` in it would place the state directory somewhere else.
/// A key with no usable characters falls back to a constant rather than an
/// empty path segment.
pub fn pubkey_prefix(pubkey_hex: &str) -> String {
    let cleaned: String = pubkey_hex
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(PUBKEY_PREFIX_LEN)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.to_ascii_lowercase()
    }
}

/// Create `dir` (and its parents) and set owner-only permissions on it.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
        if dir.as_os_str().as_bytes().len() >= libc::PATH_MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "state directory exceeds platform path limit",
            ));
        }
        let mut ancestor = PathBuf::new();
        for component in dir.components() {
            ancestor.push(component);
            match fs::symlink_metadata(&ancestor) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    // macOS exposes root-owned /var and /tmp as aliases. They
                    // are OS authority, unlike a user-supplied state alias.
                    let system_alias =
                        meta.uid() == 0 && matches!(ancestor.to_str(), Some("/var" | "/tmp"));
                    if !system_alias {
                        return Err(io::Error::other("state directory contains a symlink"));
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    }
    Ok(())
}

/// Open a regular state file without following a final-component symlink.
pub(super) fn open_read(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("state source is not a regular file"));
    }
    Ok(file)
}

/// Open `path` for appending, creating it 0600 if it does not exist.
pub fn open_append(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("state destination is not a regular file"));
    }
    harden(&file)?;
    Ok(file)
}

/// Create or replace `path` for writing, 0600.
pub fn open_create(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("state destination is not a regular file"));
    }
    harden(&file)?;
    Ok(file)
}

/// Re-apply 0600 to a file that may pre-date this code (or a looser umask).
fn harden(file: &fs::File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

/// Replace `path` with `contents` atomically: write a sibling temp file, fsync
/// it, then rename over the target. A crash leaves either the old file or the
/// new one, never a half-written one.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    WRITTEN_BYTES.with(|count| count.set(count.get() + contents.len() as u64));
    write_atomic_with_sync(path, contents, sync_dir)
}

pub(super) fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

fn write_atomic_with_sync(
    path: &Path,
    contents: &[u8],
    sync: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "state file path has no parent directory",
        )
    })?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state")
    ));
    {
        let mut file = open_create(&temp)?;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    // A visible rename is not a durable commit. The pending-operation journal
    // retains custody until this succeeds, so callers must propagate failure.
    sync(parent).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("state rename committed but durability is unconfirmed: {error}"),
        )
    })
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(unix))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(all(test, unix))]
mod tests {
    #[cfg(unix)]
    #[test]
    fn state_directory_rejects_user_symlinks_and_overlong_paths() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("actual");
        std::fs::create_dir(&target).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        assert!(super::ensure_dir(&alias.join("state")).is_err());
        assert!(!target.join("state").exists());
        assert!(super::ensure_dir(&dir.path().join("x".repeat(libc::PATH_MAX as usize))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn state_file_open_never_follows_symlink_or_changes_target_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"preserve bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        assert!(super::open_create(&alias).is_err());
        assert!(super::open_append(&alias).is_err());
        assert!(super::open_read(&alias).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve bytes");
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    use super::*;

    #[test]
    fn test_write_atomic_reports_committed_but_unconfirmed_directory_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let error = write_atomic_with_sync(&path, b"new image", |_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected sync failure",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("durability is unconfirmed"));
        assert_eq!(fs::read(&path).unwrap(), b"new image");
    }
}
