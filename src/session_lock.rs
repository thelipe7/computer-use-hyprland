//! Machine-wide input lock.
//!
//! Two servers driving the same desktop interleave their pointer and keyboard
//! events, so the first input action of a process takes `flock` on
//! `$XDG_RUNTIME_DIR/computer-use-hyprland.lock`. Read-only tools never take
//! it. The file carries the holder's pid so a second server can name it.
//!
//! The lock is a lease rather than a lifetime. Every call to this server
//! renews it, and once no call has arrived for
//! `COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS` seconds (30 unless set; 0 means
//! never) the descriptor is closed, so a session that finished driving the
//! desktop without exiting stops blocking the next one. The next input action
//! takes the lock again. A release never lands in the middle of an operation:
//! each one holds an [`InputLeaseGuard`], and the timer only lets go while
//! none is alive.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

const LOCK_FILE_NAME: &str = "computer-use-hyprland.lock";
/// Seconds without a call before a held lock is given back; `0` keeps it.
const IDLE_SECS_ENV: &str = "COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS";
const DEFAULT_IDLE: Duration = Duration::from_secs(30);

/// The one lock this process holds, on the file every server on the machine
/// shares.
static GLOBAL: LazyLock<Arc<InputLock>> = LazyLock::new(|| {
    let release = IdleRelease::from_env_value(std::env::var(IDLE_SECS_ENV).ok().as_deref());
    Arc::new(InputLock::new(lock_path(), release))
});

/// When a held lock is given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleRelease {
    /// Once this long has passed without a call to this server.
    After(Duration),
    /// Never: the lock lives as long as the process.
    Never,
}

impl IdleRelease {
    /// The policy the variable's value names: whole seconds, `0` for never,
    /// and the default when it is unset, blank or not a number. Seconds are
    /// capped at what fits a `u32`, 136 years, so a deadline can always be
    /// added to an instant.
    fn from_env_value(value: Option<&str>) -> Self {
        match value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::parse::<u64>)
        {
            Some(Ok(0)) => Self::Never,
            Some(Ok(secs)) => Self::After(Duration::from_secs(secs.min(u64::from(u32::MAX)))),
            Some(Err(_)) | None => Self::After(DEFAULT_IDLE),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LockError {
    /// Another process holds the lock; its pid when the lock file names one.
    Busy {
        pid: Option<u32>,
    },
    Io(String),
}

impl LockError {
    /// The refusal a caller reads, with what it can do next: wait out the
    /// holder's idle window, whose length `release` gives.
    pub(crate) fn message(&self, release: IdleRelease) -> String {
        match self {
            LockError::Busy { pid } => {
                let holder =
                    pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
                let outcome = match release {
                    IdleRelease::After(idle) => format!(
                        "It frees {} s after that session's last call ({IDLE_SECS_ENV}), so wait that long and retry once.",
                        idle.as_secs()
                    ),
                    IdleRelease::Never => {
                        format!("It is held until that session exits ({IDLE_SECS_ENV}=0).")
                    }
                };
                format!("Computer use is in use by another session ({holder}). {outcome}")
            }
            LockError::Io(detail) => {
                format!("Could not take the computer-use session lock: {detail}")
            }
        }
    }
}

/// The lock file, the release policy, and the lease this process holds on it.
#[derive(Debug)]
pub(crate) struct InputLock {
    path: PathBuf,
    release: IdleRelease,
    held: Mutex<Option<Lease>>,
}

/// The open descriptor that is the flock, and how it is being used.
#[derive(Debug)]
struct Lease {
    /// Closing it is what releases the flock, so it is kept and never read.
    _file: File,
    last_used: Instant,
    /// Operations whose guard is alive. The timer never releases above zero.
    in_flight: u32,
}

/// One operation's hold on the lock. While any guard is alive the lock is in
/// use whatever the clock says; dropping the last one starts the idle window.
#[must_use = "a guard dropped at once ends the hold before the operation has run"]
#[derive(Debug)]
pub(crate) struct InputLeaseGuard {
    lock: Arc<InputLock>,
}

impl Drop for InputLeaseGuard {
    fn drop(&mut self) {
        let mut held = self
            .lock
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = held.as_mut() {
            lease.in_flight = lease.in_flight.saturating_sub(1);
            lease.last_used = Instant::now();
        }
    }
}

/// What the release timer found when it looked.
enum IdleCheck {
    /// Given back, or nothing was held: the timer is done.
    Done,
    /// Still in use; look again at this instant.
    Again(Instant),
}

impl InputLock {
    fn new(path: PathBuf, release: IdleRelease) -> Self {
        Self {
            path,
            release,
            held: Mutex::new(None),
        }
    }

    /// Take the lock for one operation, or say who holds it.
    fn acquire(self: &Arc<Self>) -> Result<InputLeaseGuard, LockError> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = held.as_mut() {
            lease.in_flight += 1;
            lease.last_used = Instant::now();
        } else {
            let file = acquire_at(&self.path)?;
            *held = Some(Lease {
                _file: file,
                last_used: Instant::now(),
                in_flight: 1,
            });
            if let IdleRelease::After(idle) = self.release {
                spawn_release_timer(Arc::clone(self), idle);
            }
        }
        Ok(InputLeaseGuard {
            lock: Arc::clone(self),
        })
    }

    /// Join an operation to a lock already held, or `None` when this process
    /// holds none.
    fn hold(self: &Arc<Self>) -> Option<InputLeaseGuard> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let lease = held.as_mut()?;
        lease.in_flight += 1;
        lease.last_used = Instant::now();
        Some(InputLeaseGuard {
            lock: Arc::clone(self),
        })
    }

    /// Renew the lease, if one is held.
    fn touch(&self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = held.as_mut() {
            lease.last_used = Instant::now();
        }
    }

    /// Give the lock back if nothing has used it for `idle`, or say when to
    /// look again. The deadline comes from the lease, not from when the timer
    /// happened to run, so a late first poll cannot stretch the window.
    fn release_if_idle(&self, idle: Duration) -> IdleCheck {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(lease) = held.as_ref() else {
            return IdleCheck::Done;
        };
        if lease.in_flight > 0 {
            // An operation is running; its guard stamps last_used when it ends.
            return IdleCheck::Again(Instant::now() + idle);
        }
        let deadline = lease.last_used + idle;
        if Instant::now() < deadline {
            return IdleCheck::Again(deadline);
        }
        *held = None;
        IdleCheck::Done
    }

    #[cfg(test)]
    fn is_held(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// Close the descriptor once the lock has gone `idle` without use. Outside a
/// runtime (a CLI subcommand) nothing can run the timer, and the lock then
/// lives as long as the process, which is as long as a subcommand lives.
fn spawn_release_timer(lock: Arc<InputLock>, idle: Duration) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async move {
        loop {
            match lock.release_if_idle(idle) {
                IdleCheck::Done => return,
                IdleCheck::Again(at) => tokio::time::sleep_until(at).await,
            }
        }
    });
}

/// Take the process-wide input lock for one operation, or say who holds it.
pub(crate) fn acquire_input_lock() -> Result<InputLeaseGuard, String> {
    GLOBAL
        .acquire()
        .map_err(|error| error.message(GLOBAL.release))
}

/// Keep a held lock in use for an operation that outlives its caller. `None`
/// when this process holds no lock, which is nothing to keep.
pub(crate) fn hold_input_lock() -> Option<InputLeaseGuard> {
    GLOBAL.hold()
}

/// Renew the lease on every call to this server: a session reading the
/// screen between two inputs is still driving.
pub(crate) fn touch_input_lock() {
    GLOBAL.touch();
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

#[expect(
    clippy::verbose_file_reads,
    reason = "this reads through the caller's already-open descriptor, the one holding the lock; fs::read_to_string would open a second one"
)]
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

    const IDLE: Duration = Duration::from_secs(30);

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

    /// Two locks on one path stand in for two processes: `flock` belongs to
    /// the open file description, so a second `open` of the same path contends
    /// with the first even inside one process.
    fn pair(release: IdleRelease) -> (PathBuf, Arc<InputLock>, Arc<InputLock>) {
        let path = scratch_lock_path();
        let ours = Arc::new(InputLock::new(path.clone(), release));
        let theirs = Arc::new(InputLock::new(path.clone(), release));
        (path, ours, theirs)
    }

    /// Move the paused clock and let the release timer run.
    async fn pass(duration: Duration) {
        tokio::time::advance(duration).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    fn assert_busy(result: Result<InputLeaseGuard, LockError>) {
        match result {
            Err(LockError::Busy { pid }) => assert_eq!(
                pid,
                Some(std::process::id()),
                "the lock file names the holder"
            ),
            Ok(_) => panic!("expected a busy lock, got a lease"),
            Err(other) => panic!("expected a busy lock, got {other:?}"),
        }
    }

    #[test]
    fn second_acquisition_names_the_holder_until_the_first_is_dropped() {
        let path = scratch_lock_path();
        let first = acquire_at(&path).unwrap();

        match acquire_at(&path) {
            Err(LockError::Busy { pid }) => assert_eq!(
                pid,
                Some(std::process::id()),
                "the lock file names the holder"
            ),
            other => panic!("expected a busy lock, got {other:?}"),
        }

        drop(first);
        let again = acquire_at(&path).unwrap();
        drop(again);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn busy_message_says_when_the_lock_frees() {
        let busy = LockError::Busy { pid: Some(4242) };
        assert_eq!(
            busy.message(IdleRelease::After(IDLE)),
            "Computer use is in use by another session (pid 4242). It frees 30 s after that session's last call (COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS), so wait that long and retry once."
        );
        assert_eq!(
            busy.message(IdleRelease::Never),
            "Computer use is in use by another session (pid 4242). It is held until that session exits (COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS=0)."
        );
        assert_eq!(
            LockError::Busy { pid: None }.message(IdleRelease::After(IDLE)),
            "Computer use is in use by another session (pid unknown). It frees 30 s after that session's last call (COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS), so wait that long and retry once."
        );
    }

    #[test]
    fn idle_seconds_come_from_the_variable_and_zero_means_never() {
        let default = IdleRelease::After(IDLE);
        assert_eq!(IdleRelease::from_env_value(None), default);
        assert_eq!(IdleRelease::from_env_value(Some("")), default);
        assert_eq!(IdleRelease::from_env_value(Some("soon")), default);
        assert_eq!(IdleRelease::from_env_value(Some("-5")), default);
        assert_eq!(
            IdleRelease::from_env_value(Some(" 45 ")),
            IdleRelease::After(Duration::from_secs(45))
        );
        assert_eq!(
            IdleRelease::from_env_value(Some("18446744073709551615")),
            IdleRelease::After(Duration::from_secs(u64::from(u32::MAX))),
            "an absurd value is capped rather than overflowing a deadline"
        );
        assert_eq!(IdleRelease::from_env_value(Some("0")), IdleRelease::Never);
    }

    #[tokio::test(start_paused = true)]
    async fn lock_is_given_back_after_the_idle_window_and_not_before() {
        let (path, ours, theirs) = pair(IdleRelease::After(IDLE));
        let guard = ours.acquire().unwrap();
        assert_busy(theirs.acquire());
        drop(guard);

        pass(Duration::from_secs(29)).await;
        assert!(ours.is_held(), "released before the idle window closed");
        assert_busy(theirs.acquire());

        pass(Duration::from_secs(2)).await;
        assert!(!ours.is_held(), "still held after the idle window closed");
        let theirs_guard = theirs
            .acquire()
            .expect("the released lock is free for the next process");
        assert_busy(ours.acquire());
        drop(theirs_guard);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_in_flight_holds_the_lock_past_the_window() {
        let (path, ours, theirs) = pair(IdleRelease::After(IDLE));
        let guard = ours.acquire().unwrap();

        pass(IDLE * 3).await;
        assert!(ours.is_held(), "released under a live operation");
        assert_busy(theirs.acquire());

        drop(guard);
        pass(IDLE / 2).await;
        assert!(
            ours.is_held(),
            "the idle window starts when the operation ends, not when it began"
        );
        pass(IDLE).await;
        assert!(!ours.is_held(), "still held after the idle window closed");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn any_call_renews_the_lease() {
        let (path, ours, theirs) = pair(IdleRelease::After(IDLE));
        drop(ours.acquire().unwrap());

        pass(Duration::from_secs(20)).await;
        ours.touch();
        pass(Duration::from_secs(20)).await;
        assert!(ours.is_held(), "released 20 s after a renewal");
        assert_busy(theirs.acquire());

        pass(Duration::from_secs(20)).await;
        assert!(!ours.is_held(), "still held 40 s after the last renewal");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_detached_operation_holds_the_lock_like_its_caller_did() {
        let (path, ours, _theirs) = pair(IdleRelease::After(IDLE));
        assert!(
            ours.hold().is_none(),
            "nothing to hold before the first acquisition"
        );

        let guard = ours.acquire().unwrap();
        let detached = ours.hold().expect("a held lock can be joined");
        drop(guard);
        pass(IDLE * 2).await;
        assert!(
            ours.is_held(),
            "released while a detached operation still held it"
        );

        drop(detached);
        pass(IDLE * 2).await;
        assert!(!ours.is_held(), "still held after the last hold ended");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn zero_keeps_the_lock_for_the_life_of_the_process() {
        let (path, ours, theirs) = pair(IdleRelease::Never);
        drop(ours.acquire().unwrap());

        pass(Duration::from_secs(3600)).await;
        assert!(ours.is_held(), "released although the policy says never");
        assert_busy(theirs.acquire());
        std::fs::remove_file(&path).unwrap();
    }
}
