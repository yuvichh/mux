//! Positional file I/O that does not depend on a shared cursor, so chunks can
//! be read and written from many threads at once.

use model::artifact::{EntryKind, EntryMetadata, PERMISSION_BITS, Timestamp};
use std::fs::File;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    in_parts(buf.len(), io::ErrorKind::UnexpectedEof, |done| {
        file.seek_read(&mut buf[done..], offset + done as u64)
    })
}

#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    in_parts(buf.len(), io::ErrorKind::WriteZero, |done| {
        file.seek_write(&buf[done..], offset + done as u64)
    })
}

/// Repeats a positional read or write until `len` bytes have been handled.
/// Windows may transfer fewer bytes than asked for in one call. `step` is
/// given the number of bytes done so far and returns how many more it moved;
/// moving none is reported as `stuck`.
#[cfg(windows)]
fn in_parts(
    len: usize,
    stuck: io::ErrorKind,
    mut step: impl FnMut(usize) -> io::Result<usize>,
) -> io::Result<()> {
    let mut done = 0;
    while done < len {
        match step(done) {
            Ok(0) => return Err(stuck.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The permission bits recorded for a file or directory.
#[cfg(unix)]
pub(crate) fn mode_of(metadata: &std::fs::Metadata) -> u16 {
    use std::os::unix::fs::PermissionsExt;
    u16::try_from(metadata.permissions().mode() & u32::from(PERMISSION_BITS)).unwrap_or(0)
}

/// Windows has one permission to record: whether the file may be written.
#[cfg(not(unix))]
pub(crate) fn mode_of(metadata: &std::fs::Metadata) -> u16 {
    match (metadata.is_dir(), metadata.permissions().readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

/// Gives a file the recorded permission bits.
#[cfg(unix)]
pub(crate) fn set_mode(file: &File, mode: u16) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(u32::from(
        mode & PERMISSION_BITS,
    )))
}

/// Makes a file read-only if nobody may write it, which is the one
/// permission Windows has.
#[cfg(not(unix))]
pub(crate) fn set_mode(file: &File, mode: u16) -> io::Result<()> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    file.set_permissions(permissions)
}

/// Gives the file at `path` the recorded permission bits.
#[cfg(unix)]
pub(crate) fn set_mode_at(path: &std::path::Path, mode: u16) -> io::Result<()> {
    set_mode(&File::open(path)?, mode)
}

/// Makes the file at `path` read-only if nobody may write it. It is done
/// by the file's name: Windows lets a file be opened for writing only if
/// it may be written, which is the very thing to be changed.
#[cfg(not(unix))]
pub(crate) fn set_mode_at(path: &std::path::Path, mode: u16) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(path, permissions)
}

/// When a file or directory was last modified, if the system says.
pub(crate) fn modified_of(metadata: &std::fs::Metadata) -> Option<Timestamp> {
    let time = metadata.modified().ok()?;
    Some(match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Timestamp {
            seconds: i64::try_from(after.as_secs()).ok()?,
            nanos: after.subsec_nanos(),
        },
        // Before 1970: count back, keeping the nanoseconds positive.
        Err(before) => {
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).ok()?;
            match before.subsec_nanos() {
                0 => Timestamp {
                    seconds: -whole,
                    nanos: 0,
                },
                nanos => Timestamp {
                    seconds: -whole - 1,
                    nanos: 1_000_000_000 - nanos,
                },
            }
        }
    })
}

/// The recorded time as the system's own kind of time, or `None` if the
/// system cannot represent it.
pub(crate) fn system_time(time: Timestamp) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(time.nanos));
    match u64::try_from(time.seconds) {
        Ok(seconds) => UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))?
            .checked_add(nanos),
        Err(_) => UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.seconds.unsigned_abs()))?
            .checked_add(nanos),
    }
}

/// Gives a directory its recorded time and permissions.
/// What is recorded of an entry, as this system keeps it: everything.
#[cfg(unix)]
pub(crate) fn as_kept(_kind: &EntryKind, recorded: EntryMetadata) -> EntryMetadata {
    recorded
}

/// What is recorded of an entry, as this system keeps it: of a file's
/// permissions whether it may be written, and of a directory nothing.
#[cfg(not(unix))]
pub(crate) fn as_kept(kind: &EntryKind, recorded: EntryMetadata) -> EntryMetadata {
    match kind {
        EntryKind::File { .. } => EntryMetadata {
            mode: if recorded.mode & 0o222 == 0 {
                0o444
            } else {
                0o644
            },
            ..recorded
        },
        EntryKind::Directory => EntryMetadata {
            mode: 0o755,
            modified: None,
        },
        EntryKind::Symlink { .. } => recorded,
    }
}

#[cfg(unix)]
pub(crate) fn restore_directory(path: &std::path::Path, metadata: EntryMetadata) -> io::Result<()> {
    let dir = File::open(path)?;
    if let Some(time) = metadata.modified.and_then(system_time) {
        dir.set_modified(time)?;
    }
    set_mode(&dir, metadata.mode)
}

/// Windows offers no way to do either through the standard library.
#[cfg(not(unix))]
pub(crate) fn restore_directory(
    _path: &std::path::Path,
    _metadata: EntryMetadata,
) -> io::Result<()> {
    Ok(())
}

/// Creates a symbolic link at `path` that points at `target`.
#[cfg(unix)]
pub(crate) fn symlink(target: &str, path: &std::path::Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

/// What tells one file system from another. A flush of one says nothing of
/// what is on another.
pub(crate) type Device = u64;

/// The file system `file` is on.
#[cfg(unix)]
pub(crate) fn device_of(file: &File) -> io::Result<Device> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.dev())
}

/// Where each file is flushed by itself, it does not matter which file
/// system it is on.
#[cfg(not(unix))]
pub(crate) fn device_of(_file: &File) -> io::Result<Device> {
    Ok(0)
}

// A file that is whole is made to last in two steps, so that the dear step
// is paid once for many files. [`hand_over`] is done for each file as it is
// committed. [`make_last`] is done once for all the files that wait, and no
// file is named before it has returned. The directories that files were
// renamed into are made to last the same way when the tree is finished.

/// What "to last" takes, which differs from one system to the next.
#[allow(
    dead_code,
    reason = "a system goes one of these ways, and never the others"
)]
enum Way {
    /// macOS. `fsync` hands a file's bytes to the drive, which may keep
    /// them in its own memory for a while. Only `F_FULLFSYNC` has the
    /// drive write out what it holds, and it does that for everything it
    /// holds, whichever file it was asked through. It takes about four
    /// milliseconds, and `fsync` takes almost none. So each file is handed
    /// over with `fsync`, and one `F_FULLFSYNC` makes all of them last. Git
    /// does the same for the objects of a commit
    /// (`core.fsyncMethod=batch`).
    DriveOnce,
    /// Linux. `fsync` does both for one file, and costs a millisecond or
    /// more on a disk that is honest about it. `syncfs` does both for
    /// every file of a file system in one go: its manual says it gives
    /// what `fsync` on each file would. So nothing is done as a file is
    /// committed, and one `syncfs` makes all that wait last. A file that
    /// waits alone is flushed by itself, as it always was: a flush of the
    /// whole file system would also write out whatever other programs
    /// have waiting. `syncfs` also reports a write of any other program
    /// that failed on that file system meanwhile, and the copy then stops
    /// with it: that is rare, and it errs on the side of not naming a file.
    FileSystemOnce,
    /// Anywhere else. Each file is flushed by itself as it is committed,
    /// as it always was, and nothing is left to do for them all.
    Each,
}

#[cfg(target_vendor = "apple")]
const WAY: Way = Way::DriveOnce;
#[cfg(any(target_os = "linux", target_os = "android"))]
const WAY: Way = Way::FileSystemOnce;
#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
const WAY: Way = Way::Each;

/// Hands the bytes of a file that is whole, and what is recorded of it, to
/// the disk. They last once [`make_last`] has returned.
pub(crate) fn hand_over(file: &File) -> io::Result<()> {
    match WAY {
        Way::DriveOnce => write_out(file),
        Way::FileSystemOnce => Ok(()),
        Way::Each => file.sync_all(),
    }
}

/// Makes every file last that was handed over on the file system `file` is
/// on. `alone` says that `file` is the only one.
pub(crate) fn make_last(file: &File, alone: bool) -> io::Result<()> {
    match WAY {
        // `F_FULLFSYNC`, or `fsync` on a file system that has none.
        Way::DriveOnce => file.sync_all(),
        Way::FileSystemOnce if alone => file.sync_all(),
        Way::FileSystemOnce => flush_file_system(file),
        Way::Each => Ok(()),
    }
}

/// `fsync`, which on macOS is less than the standard library's flush.
#[cfg(target_vendor = "apple")]
fn write_out(file: &File) -> io::Result<()> {
    Ok(nix::unistd::fsync(file)?)
}

/// Never called: only macOS tells the two apart.
#[cfg(not(target_vendor = "apple"))]
fn write_out(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// `syncfs`: every file of the file system `file` is on.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn flush_file_system(file: &File) -> io::Result<()> {
    Ok(nix::unistd::syncfs(file)?)
}

/// Never called: only Linux has it.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn flush_file_system(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Files, or directories, that have been handed over and are to be made
/// to last together: for each file system, the one handed over last, kept
/// open to ask the flush through, and whether it is the only one.
#[derive(Debug, Default)]
pub(crate) struct Together(Vec<(Device, File, bool)>);

impl Together {
    /// Adds `file`, which is on `device` and has been handed over.
    pub(crate) fn add(&mut self, device: Device, file: File) {
        // Where each file is flushed by itself there is nothing left to
        // ask for, and so no reason to keep a file open: on Windows a file
        // that is open and is removed keeps its name until it is closed.
        if matches!(WAY, Way::Each) {
            return;
        }
        match self.0.iter_mut().find(|last| last.0 == device) {
            Some(last) => (last.1, last.2) = (file, false),
            None => self.0.push((device, file, true)),
        }
    }

    /// Makes all of them last. They are closed when this returns.
    pub(crate) fn make_last(self) -> io::Result<()> {
        self.0
            .iter()
            .try_for_each(|(_, file, alone)| make_last(file, *alone))
    }
}

/// Makes the directories last, so that the renames inside them survive a
/// power cut.
#[cfg(unix)]
pub(crate) fn sync_dirs<'a>(dirs: impl IntoIterator<Item = &'a std::path::Path>) -> io::Result<()> {
    let mut together = Together::default();
    for dir in dirs {
        let dir = File::open(dir)?;
        hand_over(&dir)?;
        together.add(device_of(&dir)?, dir);
    }
    together.make_last()
}

/// Windows cannot flush a directory through a file handle. NTFS records a
/// rename in its journal, which serves the same purpose.
#[cfg(not(unix))]
pub(crate) fn sync_dirs<'a>(
    _dirs: impl IntoIterator<Item = &'a std::path::Path>,
) -> io::Result<()> {
    Ok(())
}
