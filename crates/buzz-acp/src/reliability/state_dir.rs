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
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    }
    Ok(())
}

/// Open `path` for appending, creating it 0600 if it does not exist.
pub fn open_append(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    let file = options.open(path)?;
    harden(path)?;
    Ok(file)
}

/// Create or replace `path` for writing, 0600.
pub fn open_create(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(FILE_MODE);
    }
    let file = options.open(path)?;
    harden(path)?;
    Ok(file)
}

/// Re-apply 0600 to a file that may pre-date this code (or a looser umask).
fn harden(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Replace `path` with `contents` atomically: write a sibling temp file, fsync
/// it, then rename over the target. A crash leaves either the old file or the
/// new one, never a half-written one.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
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
    // The rename above is what commits the write: `path` now holds
    // `contents` regardless of anything below. Syncing the parent directory
    // entry only hardens against an OS crash landing in the narrow window
    // before that entry itself reaches disk — a best-effort durability
    // improvement, not the thing that decides whether the write happened.
    //
    // So a failure here must never turn into `Err`: an earlier version
    // propagated it, which meant a caller (e.g. `ParkFile::commit`) that
    // sees `Err` assumes NOTHING was written and keeps its own copy for a
    // future retry — while the target file, on disk, right now, already
    // holds the new content. That caller then falls through to a legacy
    // path that requeues/re-parks the same batch, producing two live copies
    // of one message (T16 delta 1, finding 5). Log and move on instead.
    match fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not fsync the state directory entry after an atomic rename — \
                 the write itself already landed; durability is degraded only against \
                 an OS crash in the next instant, not lost"
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(not(unix))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_write_atomic_survives_parent_dir_fsync_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();

        // 0o300: write + execute, but NO read permission.
        // Creating and renaming temp files succeeds (needs only write+exec on
        // the directory), but fs::File::open(parent) — used only for the
        // trailing directory-entry fsync — fails with PermissionDenied.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o300)).unwrap();

        let target = sub.join("target.txt");
        let result = write_atomic(&target, b"test payload");

        // Restore permissions for clean tempdir teardown and to read the file back.
        let _ = fs::set_permissions(&sub, fs::Permissions::from_mode(0o700));

        // T16 delta 1, finding 5: the rename already committed the write
        // before the directory-fsync step ever runs, so a failure there
        // must never be reported as "nothing was written" — a caller that
        // saw `Err` here would keep its own copy and retry, producing two
        // live copies of the same durably-written batch.
        assert!(
            result.is_ok(),
            "write_atomic must not fail the whole write just because the \
             trailing directory-entry fsync could not run: {result:?}"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"test payload",
            "the content must be exactly what was requested — the rename \
             already committed it before the fsync step"
        );
    }
}
