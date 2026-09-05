//! Machine-wide input lock.
//!
//! Two servers driving the same desktop interleave their pointer and keyboard
//! events, so the first input action of a process takes `flock` on
//! `$XDG_RUNTIME_DIR/computer-use-hyprland.lock` and keeps it until the process
//! exits. Read-only tools never touch it. The file carries the holder's pid so
//! a second server can name it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const LOCK_FILE_NAME: &str = "computer-use-hyprland.lock";

static HELD: Mutex<Option<File>> = Mutex::new(None);

#[derive(Debug)]
pub(crate) enum LockError {
    /// Another process holds the lock; its pid when the lock file names one.
    Busy {
        pid: Option<u32>,
    },
    Io(String),
}

impl LockError {
    pub(crate) fn message(&self) -> String {
        match self {
            LockError::Busy { pid: Some(pid) } => {
                format!("Computer use is in use by another session (pid {pid}).")
            }
            LockError::Busy { pid: None } => {
                "Computer use is in use by another session (pid unknown).".to_string()
            }
            LockError::Io(detail) => {
                format!("Could not take the computer-use session lock: {detail}")
            }
        }
    }
}

/// Take the process-wide input lock, or say who holds it.
pub(crate) fn acquire_input_lock() -> Result<(), String> {
    let mut held = HELD
        .lock()
        .map_err(|_| "session lock state poisoned".to_string())?;
    if held.is_some() {
        return Ok(());
    }
    let file = acquire_at(&lock_path()).map_err(|error| error.message())?;
    *held = Some(file);
    Ok(())
}

fn lock_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/run/user/{uid}"))
        });
    runtime_dir.join(LOCK_FILE_NAME)
}

pub(crate) fn acquire_at(path: &Path) -> Result<File, LockError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| LockError::Io(format!("{}: {error}", path.display())))?;
    // SAFETY: the descriptor belongs to `file`, which outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(LockError::Busy {
                pid: read_pid(&mut file),
            });
        }
        return Err(LockError::Io(format!("flock {}: {error}", path.display())));
    }
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| writeln!(file, "{}", std::process::id()))
        .and_then(|()| file.flush())
        .map_err(|error| LockError::Io(format!("write {}: {error}", path.display())))?;
    Ok(file)
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_lock_path() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "computer-use-hyprland-test-{}-{nanos}.lock",
            std::process::id()
        ))
    }

    #[test]
    fn second_acquisition_names_the_holder_until_the_first_is_dropped() {
        let path = scratch_lock_path();
        let first = acquire_at(&path).unwrap();

        match acquire_at(&path) {
            Err(LockError::Busy { pid }) => assert_eq!(pid, Some(std::process::id())),
            other => panic!("expected a busy lock, got {other:?}"),
        }
        assert_eq!(
            LockError::Busy { pid: Some(4242) }.message(),
            "Computer use is in use by another session (pid 4242)."
        );

        drop(first);
        let again = acquire_at(&path).unwrap();
        drop(again);
        std::fs::remove_file(&path).unwrap();
    }
}
