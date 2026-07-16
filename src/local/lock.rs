//! Advisory run lock at `testsprite_tests/tmp/execution.lock`.
//!
//! The real TestSprite plugin serializes runs with an `execution.lock` so two
//! concurrent invocations against one project don't stomp each other's shared
//! state (the auth session, the app, the DB). Without it, two parallel runs
//! race on the same seeded users/URLs — exactly the shared-worker collision
//! class that shows up as `ECONNRESET`/flaky teardown when a browser suite is
//! launched twice against one dev app.
//!
//! [`RunLock::acquire`] creates the lock atomically (`create_new`) and removes
//! it on drop (RAII, so a panic or early return still releases it). A lock
//! left by a crashed run is *stale*: it is stolen once it ages past a TTL, or
//! immediately when its recorded pid is no longer alive (Linux `/proc` check).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default staleness TTL: a lock older than this is assumed abandoned. Long
/// enough to cover a slow real suite, short enough that a crash doesn't wedge
/// the project. Overridable via `TESTSPRITE_LOCK_TTL_SECS`.
const DEFAULT_TTL_SECS: u64 = 3 * 60 * 60;

/// A held run lock. Dropping it deletes the lock file.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    held: bool,
}

impl RunLock {
    /// Acquire the project run lock. `Ok(Some(lock))` when held; the lock
    /// releases on drop. Returns an error describing the conflicting run when
    /// a live, non-stale lock is present.
    pub fn acquire(root: &Path) -> anyhow::Result<RunLock> {
        let path = crate::paths::Paths::new(root).execution_lock();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        match try_create(&path) {
            Ok(()) => Ok(RunLock { path, held: true }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if steal_if_stale(&path) {
                    // The prior holder is gone; take the lock.
                    try_create(&path).map_err(|e| {
                        anyhow::anyhow!(
                            "could not acquire run lock after stealing a stale one: {e}"
                        )
                    })?;
                    Ok(RunLock { path, held: true })
                } else {
                    let who = std::fs::read_to_string(&path).unwrap_or_default();
                    anyhow::bail!(
                        "another testsprite-rs run is in progress ({}). Wait for it to finish, \
                         or remove {} if it is stale.",
                        who.trim().replace('\n', " "),
                        path.display()
                    )
                }
            }
            Err(e) => Err(anyhow::anyhow!("creating run lock {}: {e}", path.display())),
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Atomically create the lock file, failing if it already exists.
fn try_create(path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let pid = std::process::id();
    let ts = now_secs();
    writeln!(f, "pid={pid} started={ts}")?;
    Ok(())
}

/// True (and the file removed) when the existing lock is stale: older than the
/// TTL, or its recorded pid is provably not running.
fn steal_if_stale(path: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        // Unreadable but present — treat as live to be safe.
        return false;
    };
    let started = field(&body, "started=").and_then(|s| s.parse::<u64>().ok());
    let pid = field(&body, "pid=").and_then(|s| s.parse::<u32>().ok());

    let aged_out = started.is_some_and(|s| now_secs().saturating_sub(s) >= ttl_secs());
    let dead_pid = pid.is_some_and(|p| !pid_alive(p));

    if aged_out || dead_pid {
        std::fs::remove_file(path).is_ok()
    } else {
        false
    }
}

/// Parse `key=value` on any line of the lock body.
fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.split_whitespace()
        .find_map(|tok| tok.strip_prefix(key))
}

fn ttl_secs() -> u64 {
    std::env::var("TESTSPRITE_LOCK_TTL_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_TTL_SECS)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort liveness check. On Linux a dead pid has no `/proc/<pid>`; where
/// that is unavailable we conservatively report "alive" and let the TTL decide.
fn pid_alive(pid: u32) -> bool {
    let proc = Path::new("/proc").join(pid.to_string());
    if proc.exists() {
        return true;
    }
    // No /proc entry: on Linux that means dead; elsewhere /proc may not exist
    // at all, so fall back to "alive" unless /proc itself is present.
    !Path::new("/proc").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_then_released_on_drop() {
        let root = crate::local::tmp_root();
        let path = crate::paths::Paths::new(&root).execution_lock();

        let lock = RunLock::acquire(&root).expect("first acquire");
        assert!(path.exists(), "lock file should exist while held");
        // A second acquire while the first is held must fail with a clear msg.
        let err = RunLock::acquire(&root).unwrap_err();
        assert!(
            err.to_string().contains("another testsprite-rs run"),
            "{err}"
        );

        drop(lock);
        assert!(!path.exists(), "lock file removed on drop");
        // Now it can be re-acquired.
        let _lock2 = RunLock::acquire(&root).expect("re-acquire after release");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_stale_lock_is_stolen() {
        let root = crate::local::tmp_root();
        let path = crate::paths::Paths::new(&root).execution_lock();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A lock from a dead pid, timestamped far in the past.
        std::fs::write(&path, "pid=999999999 started=1\n").unwrap();

        // With a tiny TTL it's aged out and stolen.
        let _g = crate::testutil::env_guard(&[("TESTSPRITE_LOCK_TTL_SECS", Some("1"))]);
        let lock = RunLock::acquire(&root).expect("should steal the stale lock");
        assert!(path.exists());
        drop(lock);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_lock_from_a_dead_pid_is_stolen_even_when_fresh() {
        let root = crate::local::tmp_root();
        let path = crate::paths::Paths::new(&root).execution_lock();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Fresh timestamp but a pid that cannot be alive.
        std::fs::write(&path, format!("pid=4294967295 started={}\n", now_secs())).unwrap();

        // Only meaningful where /proc exists (Linux); elsewhere skip the assert.
        if Path::new("/proc").exists() {
            let lock = RunLock::acquire(&root).expect("dead-pid lock should be stolen");
            drop(lock);
        }
        std::fs::remove_dir_all(root).ok();
    }
}
