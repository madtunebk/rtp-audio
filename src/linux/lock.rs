//! The per-user sender lock: an flock(2) on a file in the user's runtime directory, held by the
//! kernel for as long as the process lives, so it can't go stale even after `kill -9`.

use std::error::Error;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;

pub struct SenderLock {
    _file: File,
}

/// Where the lock and the routing state live: `$XDG_RUNTIME_DIR/rtp-audio`, which is private to
/// the user and is emptied when they log out, just like the sound server's state.
pub fn runtime_dir() -> Result<PathBuf, Box<dyn Error>> {
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(base) if !base.is_empty() => PathBuf::from(base).join("rtp-audio"),
        _ => std::env::temp_dir().join(format!("rtp-audio-{uid}")),
    };
    match DirBuilder::new().mode(0o700).create(&dir) {
        Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(format!("cannot create {}: {err}", dir.display()).into());
        }
        _ => {}
    }
    // Refuse a directory someone else made (or a symlink to one).
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!("{} must be a directory only you can access", dir.display()).into());
    }
    Ok(dir)
}

/// Take the lock, or fail with "sender already running" if another sender holds it.
pub fn acquire() -> Result<SenderLock, Box<dyn Error>> {
    let path = runtime_dir()?.join("sender.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut pid = String::new();
            let _ = file.read_to_string(&mut pid);
            let pid = pid.trim();
            if pid.parse().is_ok_and(super::service::is_service_process) {
                return Err("sender already running as the rtp-audio service. \
                    Stop it with `rtp-audio service stop` (or remove it: `rtp-audio service uninstall`)."
                    .into());
            }
            let which = if pid.is_empty() { String::new() } else { format!(" (process {pid})") };
            return Err(format!(
                "sender already running{which}. Stop it first with Ctrl+C in its terminal{}.",
                if pid.is_empty() { String::new() } else { format!(" or `kill {pid}`") }
            )
            .into());
        }
        Err(TryLockError::Error(err)) => return Err(format!("cannot lock {}: {err}", path.display()).into()),
    }
    // Only informational, for the message above; the lock itself is what counts.
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
    Ok(SenderLock { _file: file })
}
