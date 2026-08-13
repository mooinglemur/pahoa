//! Where saves go: a plain directory, and nothing else.
//!
//! pahoa knows about a directory. In the target cluster that is a subdirectory
//! of a CephFS RWX volume, under Docker it is a volume per room, and in tests it
//! is a temp dir — one code path for all three. Object storage is a backup tier
//! written by ordinary tooling (`restic`, `rclone`) against the same tree, not
//! something this crate has ever heard of.
//!
//! # What a shared filesystem changes
//!
//! CephFS does not fail fast. An MDS failover **blocks** — seconds to tens of
//! seconds, and the same during recovery or rebalance — rather than returning an
//! error. So every call here may hang for an unbounded time, and the design
//! answer is that none of it happens anywhere the room can notice: writes run on
//! a blocking thread that the actor never awaits, and a save that fails is
//! logged rather than propagated. Losing a recovery point is bad; losing a live
//! 2000-slot room because its filesystem hiccuped is worse.
//!
//! # Durability
//!
//! Write to a temp file **in the same directory** (so the rename is a
//! metadata-only operation), `fsync` it, rename over the target, then `fsync`
//! the directory so the rename itself survives a power cut. The target is
//! therefore never torn: a reader sees either the whole old save or the whole
//! new one. Python truncates its save in place and can leave a corrupt one
//! behind (`Utils.py`'s `store_data_package` aside, `customserver.py:207-213`
//! writes straight over the row).
//!
//! There is deliberately no previous-generation copy. Temp+rename plus the
//! format's checksum covers tearing and truncation, and everything else —
//! media rot, an operator mistake, a bad deploy — is what the backup tier is
//! for. Keeping a `.prev` here would double the write volume to cover a case
//! the CronJob already covers better.
//!
//! # One writer
//!
//! RWX means two pods genuinely can mount the same directory: a NotReady node
//! whose kubelet is still running, or a controller that starts a replacement
//! before the old room is gone. Both would then write whole snapshots over each
//! other — last writer wins, silently, which is the worst possible shape for
//! data loss. An exclusive `flock` held for the life of the process turns that
//! into a refusal at startup with a clear message. The kernel CephFS client
//! supports `flock` across nodes, which is what makes this work at all.

use std::fs::{File, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where a snapshot goes once it has been encoded.
///
/// One method, because that is the entire contract: pahoa hands over a finished
/// blob and the sink makes it durable or says why it could not. [`SaveStore`] is
/// the only implementation that ships. The seam exists so an object tier stays
/// *additive* if volume-less pods ever become worth supporting, and so a test
/// can stand in a sink that behaves like a filesystem having a bad day.
///
/// Reading is deliberately not here. A restore happens once, at startup, before
/// anything is serving, and the binary knows exactly which store it opened.
pub trait SaveSink: Send + Sync + 'static {
    /// Replace the saved state. Blocking, and expected to be — callers run it
    /// on a thread the room never waits for.
    fn store(&self, bytes: &[u8]) -> io::Result<()>;
}

/// The current snapshot.
const SAVE_NAME: &str = "room.save";
/// Held open for the process lifetime; the lock releases when it is dropped, or
/// when the process dies for any reason including SIGKILL.
const LOCK_NAME: &str = "room.lock";

pub struct SaveStore {
    dir: PathBuf,
    /// Never read. Its existence is the lock; dropping it releases.
    _lock: File,
}

impl SaveStore {
    /// Claim a save directory, creating it if absent.
    ///
    /// Fails rather than waits when another process holds the lock: a room that
    /// blocks here would present to Kubernetes as a pod that never goes ready,
    /// with no indication why.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let lock = File::create(dir.join(LOCK_NAME))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "another pahoa process is already serving {}; \
                         refusing to share a save directory",
                        dir.display()
                    ),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(e),
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(SAVE_NAME)
    }

    /// Read the current save, or `None` if the room has never saved.
    pub fn load(&self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path()) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Replace the save atomically.
    ///
    /// Inherent as well as trait method so callers holding a concrete
    /// `SaveStore` — the tests below, and startup — do not need the trait in
    /// scope.
    pub fn store(&self, bytes: &[u8]) -> io::Result<()> {
        let target = self.path();
        // Same directory, so the rename below never crosses a filesystem and
        // stays a metadata-only operation. The pid keeps two processes from
        // colliding on the temp name in the split second before one of them
        // loses the lock race.
        let temp = self
            .dir
            .join(format!(".{SAVE_NAME}.{}.tmp", std::process::id()));

        let write = (|| {
            let mut file = File::create(&temp)?;
            file.write_all(bytes)?;
            // Before the rename, not after: a rename that lands before the data
            // does would publish a file of zeroes.
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, &target)?;
            // And the rename itself has to be durable, or a power cut can leave
            // the directory entry pointing at nothing.
            File::open(&self.dir)?.sync_all()
        })();

        if write.is_err() {
            // Best-effort: a leftover temp file is harmless but untidy, and on
            // a full filesystem it is the thing standing between us and the
            // next successful save.
            let _ = std::fs::remove_file(&temp);
        }
        write
    }
}

impl SaveSink for SaveStore {
    fn store(&self, bytes: &[u8]) -> io::Result<()> {
        SaveStore::store(self, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pahoa-save-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_save_round_trips_through_the_directory() {
        let dir = tempdir("roundtrip");
        let store = SaveStore::open(&dir).unwrap();
        assert!(
            store.load().unwrap().is_none(),
            "a fresh directory is empty"
        );

        store.store(b"first").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"first"[..]));

        store.store(b"second").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&b"second"[..]));

        // No temp files left behind — those would accumulate one per save.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_process_is_refused_rather_than_allowed_to_overwrite() {
        let dir = tempdir("lock");
        let first = SaveStore::open(&dir).unwrap();

        // Same-process `flock` is per-file-description, not per-process, so a
        // second `open` here really does exercise the cross-pod case.
        let second = SaveStore::open(&dir);
        assert!(
            second.is_err(),
            "two writers must not share a save directory"
        );

        drop(first);
        // And the lock is released when the holder goes away, so a restarted
        // room can take over without an operator clearing anything.
        SaveStore::open(&dir).expect("the lock should be free again");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
