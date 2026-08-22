//! Cross-process maintenance lock for a graph database.
//!
//! Rebuild, refresh and repair take an exclusive advisory lock on `<graph.db>.lock` for their
//! whole duration, so two maintenance runs can never interleave. The lock is an OS file lock: it
//! is released when the holder exits, even after a crash, so a dead holder never wedges the
//! graph. The file itself is left in place (deleting it would race with a new holder).

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::graph::maintenance::GraphMaintenanceError;

pub struct MaintenanceLock {
    file: File,
    path: PathBuf,
}

/// Lock file guarding maintenance of `db_path`.
pub fn lock_path(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "graph.db".into());
    name.push(".lock");
    db_path.with_file_name(name)
}

impl MaintenanceLock {
    /// Take the lock, failing immediately with `GRAPH_MAINTENANCE_LOCKED` when another process
    /// (or another handle in this process) holds it.
    pub fn acquire(db_path: &Path) -> Result<Self, GraphMaintenanceError> {
        Self::acquire_within(db_path, Duration::ZERO)
    }

    /// Take the lock, retrying for up to `wait` before giving up.
    pub fn acquire_within(db_path: &Path, wait: Duration) -> Result<Self, GraphMaintenanceError> {
        let path = lock_path(db_path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                GraphMaintenanceError::new(
                    "GRAPH_MAINTENANCE_LOCK_FAILED",
                    format!("Could not open the graph maintenance lock {}: {}", path.display(), e),
                )
            })?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        let holder = holder.trim();
                        return Err(GraphMaintenanceError::new(
                            "GRAPH_MAINTENANCE_LOCKED",
                            format!(
                                "Another graph maintenance run (rebuild, refresh or repair) is in progress{}. \
                                 Retry when it finishes.",
                                if holder.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", holder)
                                }
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::Error(e)) => {
                    return Err(GraphMaintenanceError::new(
                        "GRAPH_MAINTENANCE_LOCK_FAILED",
                        format!("Could not lock {}: {}", path.display(), e),
                    ))
                }
            }
        }
        // Holder description for diagnostics only; the OS lock is what excludes.
        let mut f = &file;
        let _ = file.set_len(0);
        let _ = writeln!(
            f,
            "pid {} since {}",
            std::process::id(),
            chrono::Utc::now().to_rfc3339()
        );
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}
