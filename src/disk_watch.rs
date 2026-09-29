//! Noticing when another program changes the open file on disk.
//!
//! The app polls a cheap [`DiskStamp`] on every tick and compares it with the
//! [`DiskBaseline`] — what Pure itself last loaded from or wrote to the file.
//! Only when the stamp moves (and then holds still) is the file read, and the
//! bytes are compared with the baseline's hash so a `touch`, an identical
//! rewrite or Pure's own save never count as a change.

use std::{
    fs,
    hash::{DefaultHasher, Hasher},
    io,
    path::Path,
    time::SystemTime,
};

/// What the file system reports about a file, without reading its contents.
///
/// Stamps are only ever compared for equality, never ordered: `mv`, `cp -p`,
/// `rsync -t` or `git checkout` can move a file's modification time backwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    len: u64,
    modified: Option<SystemTime>,
    /// Device and inode catch rename-into-place saves (vim, `sed -i`, `git
    /// checkout`) that keep the size and land within one mtime tick.
    #[cfg(unix)]
    file_id: (u64, u64),
    /// The status-change time also moves on in-place rewrites of equal size.
    #[cfg(unix)]
    changed: (i64, i64),
}

impl DiskStamp {
    /// Stamp the file at `path`, following symlinks. Fails when the file is
    /// missing, its metadata can't be read, or it isn't a regular file — a
    /// FIFO or device would block (or never end) when read.
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            #[cfg(unix)]
            file_id: (meta.dev(), meta.ino()),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

/// A fingerprint of a file's contents. `DefaultHasher::new` uses fixed keys, so
/// the value is stable for the life of the process, which is all it needs.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

/// The version of the file Pure last loaded or wrote, and the change it is
/// currently watching settle.
#[derive(Clone, Debug)]
pub struct DiskBaseline {
    stamp: DiskStamp,
    hash: u64,
    /// A differing stamp seen on the previous poll: the change is acted on
    /// once a second poll agrees, so a writer that is still busy isn't read.
    pending: Option<DiskStamp>,
    /// The stamp a read error was last reported for, so it is shown once.
    read_error_reported: Option<DiskStamp>,
}

impl DiskBaseline {
    /// `stamp` must be taken *before* `bytes` were read, so a change racing
    /// the read still shows on the next poll.
    pub fn new(stamp: DiskStamp, bytes: &[u8]) -> Self {
        Self {
            stamp,
            hash: content_hash(bytes),
            pending: None,
            read_error_reported: None,
        }
    }

    /// Feed the current stamp. Returns it once it differs from the baseline and
    /// has been seen on two consecutive polls — and keeps returning it on each
    /// later poll until the baseline adopts it, so a failed read is retried.
    pub fn observe(&mut self, now: DiskStamp) -> Option<DiskStamp> {
        if now == self.stamp {
            self.pending = None;
            return None;
        }
        if self.pending.as_ref() == Some(&now) {
            return Some(now);
        }
        self.pending = Some(now);
        None
    }

    /// Drop a change being watched (the file vanished or moved on mid-read).
    pub fn forget_pending(&mut self) {
        self.pending = None;
    }

    /// Record a read error for `now`; `true` the first time for that stamp.
    pub fn note_read_error(&mut self, now: &DiskStamp) -> bool {
        if self.read_error_reported.as_ref() == Some(now) {
            return false;
        }
        self.read_error_reported = Some(now.clone());
        true
    }

    /// Take `stamp` as seen, without claiming the editor matches the file: the
    /// hash still describes the version on screen.
    pub fn adopt(&mut self, stamp: DiskStamp) {
        self.stamp = stamp;
        self.pending = None;
        self.read_error_reported = None;
    }

    /// Take `stamp` and `bytes` as the version on screen.
    pub fn adopt_content(&mut self, stamp: DiskStamp, bytes: &[u8]) {
        self.adopt(stamp);
        self.hash = content_hash(bytes);
    }

    pub fn stamp(&self) -> &DiskStamp {
        &self.stamp
    }

    /// Whether `bytes` are the contents this baseline was taken from.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        content_hash(bytes) == self.hash
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, UNIX_EPOCH},
    };

    use super::{DiskBaseline, DiskStamp};

    /// A temp file removed on drop.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(content: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("pure-stamp-{}-{id}", std::process::id()));
            fs::write(&path, content).unwrap();
            Self(path)
        }

        fn stamp(&self) -> DiskStamp {
            DiskStamp::of(&self.0).unwrap()
        }

        fn set_mtime(&self, secs: u64) {
            fs::File::options()
                .write(true)
                .open(&self.0)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn stamp_is_stable_for_an_untouched_file() {
        let file = TempFile::new("hello");
        assert_eq!(file.stamp(), file.stamp());
    }

    #[test]
    fn stamp_changes_with_modification_time() {
        let file = TempFile::new("hello");
        file.set_mtime(1_000);
        let before = file.stamp();
        file.set_mtime(2_000);
        assert_ne!(before, file.stamp());
    }

    #[test]
    fn missing_file_or_directory_has_no_stamp() {
        let file = TempFile::new("hello");
        let missing = file.0.with_extension("missing");
        assert!(DiskStamp::of(&missing).is_err());
        assert!(DiskStamp::of(&std::env::temp_dir()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rename_replacement_with_same_size_and_mtime_changes_stamp() {
        let file = TempFile::new("hello");
        file.set_mtime(1_000);
        let before = file.stamp();
        let replacement = TempFile::new("HELLO");
        replacement.set_mtime(1_000);
        fs::rename(&replacement.0, &file.0).unwrap();
        assert_ne!(before, file.stamp());
    }

    #[test]
    fn change_is_reported_on_second_identical_observation() {
        let file = TempFile::new("one");
        file.set_mtime(1_000);
        let mut baseline = DiskBaseline::new(file.stamp(), b"one");
        assert_eq!(baseline.observe(file.stamp()), None, "unchanged");
        file.set_mtime(2_000);
        assert_eq!(baseline.observe(file.stamp()), None, "first sighting");
        assert_eq!(baseline.observe(file.stamp()), Some(file.stamp()));
        assert_eq!(
            baseline.observe(file.stamp()),
            Some(file.stamp()),
            "retried until adopted"
        );
        baseline.adopt(file.stamp());
        assert_eq!(baseline.observe(file.stamp()), None);
    }

    #[test]
    fn change_that_keeps_moving_is_not_reported() {
        let file = TempFile::new("one");
        file.set_mtime(1_000);
        let mut baseline = DiskBaseline::new(file.stamp(), b"one");
        for secs in 2..6 {
            file.set_mtime(secs * 1_000);
            assert_eq!(baseline.observe(file.stamp()), None);
        }
    }

    #[test]
    fn change_that_reverts_is_forgotten() {
        let file = TempFile::new("one");
        file.set_mtime(1_000);
        let original = file.stamp();
        let mut baseline = DiskBaseline::new(original.clone(), b"one");
        file.set_mtime(2_000);
        let changed = file.stamp();
        assert_eq!(baseline.observe(changed.clone()), None);
        assert_eq!(baseline.observe(original), None);
        // The very same stamp again (not a fresh `stat`: on Unix, touching the
        // mtime also moves the ctime) counts as a first sighting.
        assert_eq!(
            baseline.observe(changed),
            None,
            "the earlier sighting was dropped"
        );
    }

    #[test]
    fn matches_uses_the_known_bytes() {
        let file = TempFile::new("one");
        let mut baseline = DiskBaseline::new(file.stamp(), b"one");
        assert!(baseline.matches(b"one"));
        assert!(!baseline.matches(b"two"));
        baseline.adopt(file.stamp());
        assert!(baseline.matches(b"one"), "adopt keeps the hash");
        baseline.adopt_content(file.stamp(), b"two");
        assert!(baseline.matches(b"two"));
    }

    #[test]
    fn read_error_is_reported_once_per_stamp() {
        let file = TempFile::new("one");
        file.set_mtime(1_000);
        let mut baseline = DiskBaseline::new(file.stamp(), b"one");
        file.set_mtime(2_000);
        let changed = file.stamp();
        assert!(baseline.note_read_error(&changed));
        assert!(!baseline.note_read_error(&changed));
        file.set_mtime(3_000);
        assert!(baseline.note_read_error(&file.stamp()));
    }
}
