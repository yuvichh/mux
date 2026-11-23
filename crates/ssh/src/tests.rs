//! The backend against a server written for these tests, which serves a
//! folder of this machine and can be told to misbehave, and against the
//! system's own `sftp-server` where there is one.

use crate::remote::Remote;
use crate::session::Session;
use crate::wire::served::{self, Asked, answer, asked};
use crate::wire::{Attrs, Named, Reply, status};
use engine::conformance::{Sample, check_source};
use model::artifact::{EntryKind, Timestamp};
use model::place::{Arrived, Failure, Source};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// How the server of a test behaves.
#[derive(Debug, Clone, Default)]
struct Manner {
    /// The most bytes it gives in one answer to a read.
    short: Option<usize>,
    /// The version it says it speaks, if not the real one.
    version: Option<u32>,
    /// It hangs up when it is asked to read a file.
    hangs_up_on_read: bool,
    /// It answers a read with more bytes than were asked for.
    sends_too_much: bool,
}

/// What a server has open, and how often it was asked each thing.
#[derive(Debug, Default)]
struct Seen {
    open: AtomicUsize,
    reads: AtomicUsize,
}

/// Something a server has open.
enum Open {
    File(PathBuf),
    /// The names of a directory that have not been given yet.
    Names(Vec<Named>),
}

fn attrs_of(path: &Path, follow: bool) -> Option<Attrs> {
    let found = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
    .ok()?;
    let kind = if found.is_dir() {
        0o040_000
    } else if found.is_symlink() {
        0o120_000
    } else if found.is_file() {
        0o100_000
    } else {
        // A socket: something that is neither.
        0o140_000
    };
    #[cfg(unix)]
    let bits = std::os::unix::fs::PermissionsExt::mode(&found.permissions()) & 0o777;
    #[cfg(not(unix))]
    let bits = if found.is_dir() { 0o755 } else { 0o644 };
    let modified = found.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Attrs {
        size: Some(found.len()),
        mode: Some(kind | bits),
        modified: u32::try_from(modified.as_secs()).ok(),
    })
}

fn failed(code: u32) -> Reply {
    Reply::Status {
        code,
        message: String::new(),
    }
}

/// Answers one request about the files of this machine.
fn serve(asked: &Asked, open: &mut HashMap<Vec<u8>, Open>, manner: &Manner, seen: &Seen) -> Reply {
    let path = PathBuf::from(String::from_utf8_lossy(&asked.text).into_owned());
    let as_name = |path: &Path| Named {
        name: path.to_string_lossy().into_owned().into_bytes(),
        attrs: Attrs::default(),
    };
    let mut hold = |what: Open| {
        let handle = format!("h{}", asked.id).into_bytes();
        open.insert(handle.clone(), what);
        seen.open.fetch_add(1, Ordering::Relaxed);
        Reply::Handle(handle)
    };
    match asked.kind {
        served::REALPATH => match path.canonicalize() {
            Ok(real) => Reply::Name(vec![as_name(&real)]),
            Err(_) => failed(status::NO_SUCH_FILE),
        },
        served::STAT | served::LSTAT => match attrs_of(&path, asked.kind == served::STAT) {
            _ if path.ends_with("forbidden") => failed(status::PERMISSION_DENIED),
            Some(attrs) => Reply::Attrs(attrs),
            None => failed(status::NO_SUCH_FILE),
        },
        served::OPENDIR => match std::fs::read_dir(&path) {
            Ok(entries) => {
                let mut names: Vec<Named> = [".", ".."].map(|name| as_name(Path::new(name))).into();
                for entry in entries.flatten() {
                    let attrs = attrs_of(&entry.path(), false).unwrap_or_default();
                    let name = entry
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes();
                    names.push(Named { name, attrs });
                }
                hold(Open::Names(names))
            }
            Err(_) => failed(status::NO_SUCH_FILE),
        },
        // A few names at a time, so that a directory takes several asks.
        served::READDIR => match open.get_mut(&asked.text) {
            Some(Open::Names(names)) if names.is_empty() => failed(status::EOF),
            Some(Open::Names(names)) => Reply::Name(names.drain(..names.len().min(3)).collect()),
            _ => failed(status::FAILURE),
        },
        served::READLINK => match std::fs::read_link(&path) {
            Ok(target) => Reply::Name(vec![as_name(&target)]),
            Err(_) => failed(status::NO_SUCH_FILE),
        },
        served::OPEN if path.is_file() => hold(Open::File(path)),
        served::OPEN => failed(status::NO_SUCH_FILE),
        served::FSTAT => match open.get(&asked.text) {
            Some(Open::File(path)) => {
                attrs_of(path, true).map_or(failed(status::FAILURE), Reply::Attrs)
            }
            _ => failed(status::FAILURE),
        },
        served::READ => {
            seen.reads.fetch_add(1, Ordering::Relaxed);
            let Some(Open::File(path)) = open.get(&asked.text) else {
                return failed(status::FAILURE);
            };
            let bytes = std::fs::read(path).unwrap_or_default();
            let from = usize::try_from(asked.offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let most = manner.short.unwrap_or(usize::MAX).min(asked.len as usize);
            let give = if manner.sends_too_much {
                asked.len as usize + 1
            } else {
                most
            };
            match &bytes[from..bytes.len().min(from.saturating_add(give))] {
                [] => failed(status::EOF),
                data => Reply::Data(data.to_vec()),
            }
        }
        served::CLOSE => {
            if open.remove(&asked.text).is_some() {
                seen.open.fetch_sub(1, Ordering::Relaxed);
            }
            failed(status::OK)
        }
        _ => failed(status::FAILURE),
    }
}

/// Starts a server on one end of a pipe, and gives a session over the
/// other.
async fn served(manner: Manner) -> (Result<Arc<Session>, Failure>, Arc<Seen>) {
    let (near, far) = tokio::io::duplex(64 * 1024);
    let seen = Arc::new(Seen::default());
    tokio::spawn(serving(far, manner, seen.clone()));
    let (output, input) = tokio::io::split(near);
    let session = Session::over(output, input, None, || {
        "the server of the test hung up".into()
    })
    .await;
    (session, seen)
}

async fn serving(mut pipe: DuplexStream, manner: Manner, seen: Arc<Seen>) {
    let mut open = HashMap::new();
    loop {
        let mut len = [0; 4];
        if pipe.read_exact(&mut len).await.is_err() {
            return;
        }
        let mut body = vec![0; u32::from_be_bytes(len) as usize];
        if pipe.read_exact(&mut body).await.is_err() {
            return;
        }
        let asked = asked(&body).unwrap();
        let reply = match asked.kind {
            served::INIT => Reply::Version(manner.version.unwrap_or(crate::wire::VERSION)),
            served::READ if manner.hangs_up_on_read => return,
            _ => serve(&asked, &mut open, &manner, &seen),
        };
        if pipe.write_all(&answer(asked.id, &reply)).await.is_err() {
            return;
        }
    }
}

/// A source over a server that behaves, for what is at `path`.
async fn remote(path: &Path) -> Remote {
    remote_of(path, Manner::default()).await.0
}

async fn remote_of(path: &Path, manner: Manner) -> (Remote, Arc<Seen>) {
    let (session, seen) = served(manner).await;
    let found = Remote::over(session.unwrap(), "host", path.to_str().unwrap()).await;
    (found.unwrap(), seen)
}

/// Writes the files of `sample` under `dir`, and gives where its root is.
fn written(sample: &Sample, dir: &Path) -> PathBuf {
    let tree = sample.artifact.tree();
    let root = dir.join(tree.name());
    for (entry, bytes) in tree.entries().iter().zip(&sample.files) {
        let path = root.join(&entry.path);
        match entry.kind {
            EntryKind::Directory => std::fs::create_dir_all(&path).unwrap(),
            _ => std::fs::write(&path, bytes).unwrap(),
        }
    }
    root.canonicalize().unwrap()
}

#[tokio::test]
async fn a_remote_folder_passes_the_checks_every_source_must() {
    let tmp = tempfile::tempdir().unwrap();
    let sample = Sample::new().unwrap();
    let root = written(&sample, tmp.path());
    let (remote, seen) = remote_of(&root, Manner::default()).await;
    assert_eq!(check_source(&remote, &sample).await, Ok(()));
    assert_eq!(remote.skipped(), 0);

    // Everything that was opened is let go of, once nothing reads it.
    drop(remote);
    for _ in 0..200 {
        if seen.open.load(Ordering::Relaxed) == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{} files or folders were left open",
        seen.open.load(Ordering::Relaxed)
    );
}

/// A server may answer a read with fewer bytes than it was asked for. It is
/// asked for the rest, and the file is still its bytes.
#[tokio::test]
async fn a_server_that_gives_less_than_it_was_asked_for_is_asked_for_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let sample = Sample::new().unwrap();
    let root = written(&sample, tmp.path());
    let short = Manner {
        short: Some(1000),
        ..Manner::default()
    };
    let (remote, seen) = remote_of(&root, short).await;
    assert_eq!(check_source(&remote, &sample).await, Ok(()));
    let bytes: usize = sample.files.iter().map(Vec::len).sum();
    assert!(seen.reads.load(Ordering::Relaxed) > bytes / 1000);
}

/// A megabyte is asked for in pieces, all of them before the first is
/// answered.
#[tokio::test]
async fn a_large_read_is_many_requests_and_comes_back_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..1_500_000u32)
        .map(|i| (i ^ (i >> 7)).to_le_bytes()[0])
        .collect();
    let path = tmp.path().join("big.bin");
    std::fs::write(&path, &bytes).unwrap();
    let (remote, seen) = remote_of(&path, Manner::default()).await;

    let arrived = Arrived::default();
    let mut reader = remote.open(0, 100, 1_400_000).await.unwrap();
    let first = reader.read(1_048_576, &arrived).await.unwrap();
    assert!(first == bytes[100..100 + 1_048_576]);
    assert_eq!(seen.reads.load(Ordering::Relaxed), 32);
    let second = reader.read(351_424, &arrived).await.unwrap();
    assert!(second == bytes[100 + 1_048_576..1_400_100]);
    assert_eq!(arrived.so_far(), 1_400_000);
    // No more than it was opened for.
    assert!(matches!(
        reader.read(1, &arrived).await,
        Err(Failure::Fatal(_))
    ));
    // And nothing past the end of the file can be opened.
    assert!(matches!(
        remote.open(0, 1_400_000, 100_001).await,
        Err(Failure::Absent(_))
    ));
    assert!(matches!(
        remote.open(1, 0, 1).await,
        Err(Failure::Absent(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn a_folder_is_found_with_what_it_holds_and_what_cannot_be_copied_is_left_out() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("site");
    std::fs::create_dir_all(root.join("b/deep/empty")).unwrap();
    std::fs::create_dir_all(root.join(".mux-trash/old")).unwrap();
    std::fs::write(root.join("a.txt"), b"hello").unwrap();
    std::fs::write(root.join("b/run.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::write(root.join("b/deep/z"), b"").unwrap();
    std::fs::write(root.join("b/half.bin.mux-part"), b"unfinished").unwrap();
    std::fs::set_permissions(
        root.join("b/run.sh"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    symlink("deep/z", root.join("b/inside")).unwrap();
    symlink("../../outside", root.join("b/outside")).unwrap();
    symlink("/etc/hosts", root.join("b/absolute")).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(root.join("b/socket")).unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    std::fs::File::open(root.join("a.txt"))
        .unwrap()
        .set_modified(time)
        .unwrap();

    let remote = remote(&root).await;
    let described = remote.describe().await.unwrap();
    assert_eq!(described.name, "site");
    let kinds: Vec<(&str, &EntryKind)> = described
        .entries
        .iter()
        .map(|(entry, _)| (entry.path.as_str(), &entry.kind))
        .collect();
    let link = EntryKind::Symlink {
        target: "deep/z".into(),
    };
    assert_eq!(
        kinds,
        [
            ("", &EntryKind::Directory),
            ("a.txt", &EntryKind::File { size: 5 }),
            ("b", &EntryKind::Directory),
            ("b/deep", &EntryKind::Directory),
            ("b/deep/empty", &EntryKind::Directory),
            ("b/deep/z", &EntryKind::File { size: 0 }),
            ("b/inside", &link),
            ("b/run.sh", &EntryKind::File { size: 10 }),
        ]
    );
    // The part file, the socket and the two links that leave the folder.
    assert_eq!(remote.skipped(), 4);

    let recorded = |path: &str| {
        let at = described
            .entries
            .iter()
            .position(|(entry, _)| entry.path == path)
            .unwrap();
        (
            described.entries[at].1,
            described.known[at].validator.clone(),
        )
    };
    let (file, validator) = recorded("a.txt");
    assert_eq!(
        file.modified,
        Some(Timestamp {
            seconds: 1_700_000_000,
            nanos: 0
        })
    );
    // What tells one version from another: the size, then the time.
    assert_eq!(
        validator,
        [&5u64.to_le_bytes()[..], &1_700_000_000u32.to_le_bytes()].concat()
    );
    assert_eq!(recorded("b/run.sh").0.mode, 0o750);
    // A link has no permissions or time of its own, and a folder nothing
    // to tell its versions apart.
    assert_eq!(
        (recorded("b/inside").0.mode, recorded("b/inside").0.modified),
        (0o777, None)
    );
    assert!(recorded("b").1.is_empty() && recorded("b/inside").1.is_empty());
}

#[tokio::test]
async fn a_file_is_a_place_by_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes.txt");
    std::fs::write(&path, b"one file").unwrap();
    let remote = remote(&path).await;
    let described = remote.describe().await.unwrap();
    assert_eq!(described.name, "notes.txt");
    assert_eq!(described.entries.len(), 1);
    assert_eq!(described.entries[0].0.kind, EntryKind::File { size: 8 });
    assert_eq!(remote.read(0, 4, 4).await.unwrap(), b"file");
}

/// A file that is not what it was when the folder was looked at is not
/// read: its bytes would be those of two versions.
#[tokio::test]
async fn a_file_that_has_changed_since_it_was_found_is_not_read() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes.txt");
    std::fs::write(&path, b"as it was").unwrap();
    let remote = remote(&path).await;
    assert!(remote.open(0, 0, 9).await.is_ok());

    // Another size.
    std::fs::write(&path, b"as it is now").unwrap();
    assert!(matches!(
        remote.open(0, 0, 9).await,
        Err(Failure::Changed(_))
    ));
    // The same size at another time.
    std::fs::write(&path, b"as it wax").unwrap();
    let later = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(matches!(
        remote.open(0, 0, 9).await,
        Err(Failure::Changed(_))
    ));
    // Gone.
    std::fs::remove_file(&path).unwrap();
    let gone = remote.open(0, 0, 9).await.unwrap_err();
    assert!(
        matches!(&gone, Failure::Changed(why) if why.contains("host:")),
        "{gone}"
    );
}

#[tokio::test]
async fn what_is_not_there_or_may_not_be_read_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let find = |name: &str| {
        let path = tmp.path().join(name);
        async move {
            let (session, _) = served(Manner::default()).await;
            Remote::over(session.unwrap(), "host", path.to_str().unwrap()).await
        }
    };
    let absent = find("nothing-here").await.unwrap_err();
    assert!(
        matches!(&absent, Failure::Absent(why) if why.contains("nothing is at host:")),
        "{absent}"
    );

    std::fs::write(tmp.path().join("forbidden"), b"").unwrap();
    let denied = find("forbidden").await.unwrap_err();
    assert!(
        matches!(&denied, Failure::Fatal(why) if why.contains("permission")),
        "{denied}"
    );

    // The root of a machine has no name to be copied under.
    let (session, _) = served(Manner::default()).await;
    let root = Remote::over(session.unwrap(), "host", "/")
        .await
        .unwrap_err();
    assert!(
        matches!(&root, Failure::Fatal(why) if why.contains("Name a folder")),
        "{root}"
    );
}

/// A connection that breaks off fails whoever is waiting, and whoever asks
/// afterwards, with what ended it.
#[tokio::test]
async fn a_connection_that_breaks_off_fails_what_waits_on_it() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes.txt");
    std::fs::write(&path, vec![7; 100_000]).unwrap();
    let hangs_up = Manner {
        hangs_up_on_read: true,
        ..Manner::default()
    };
    let (remote, _) = remote_of(&path, hangs_up).await;
    let mut reader = remote.open(0, 0, 100_000).await.unwrap();
    let broke = reader.read(100_000, &Arrived::default()).await.unwrap_err();
    assert_eq!(
        broke,
        Failure::Unreachable("the server of the test hung up".into())
    );
    let after = remote.open(0, 0, 1).await.unwrap_err();
    assert_eq!(after, broke);
}

#[tokio::test]
async fn a_server_that_sends_what_was_not_asked_for_is_not_believed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes.txt");
    std::fs::write(&path, vec![7; 5000]).unwrap();
    let too_much = Manner {
        sends_too_much: true,
        ..Manner::default()
    };
    let (remote, _) = remote_of(&path, too_much).await;
    let read = remote.read(0, 0, 1000).await;
    assert!(matches!(read, Err(Failure::Corrupt(_))), "{read:?}");
}

#[tokio::test]
async fn a_conversation_that_does_not_begin_says_why() {
    let old = Manner {
        version: Some(2),
        ..Manner::default()
    };
    let refused = served(old).await.0.unwrap_err();
    assert!(
        matches!(&refused, Failure::Fatal(why) if why.contains("version 2")),
        "{refused}"
    );

    // Something that is not a server: it says nothing and hangs up.
    let (near, far) = tokio::io::duplex(1024);
    drop(far);
    let (output, input) = tokio::io::split(near);
    let silent = Session::over(output, input, None, || "ssh: no route to host".into()).await;
    assert_eq!(
        silent.unwrap_err(),
        Failure::Unreachable("ssh: no route to host".into())
    );

    // A program that cannot be run at all.
    let missing = tokio::process::Command::new("/nonexistent/ssh-of-a-test");
    let cannot = Session::run(missing, "host").await.unwrap_err();
    assert!(
        matches!(&cannot, Failure::Fatal(why) if why.contains("cannot run")),
        "{cannot}"
    );
}

/// Where systems keep the server that `sshd` runs for SFTP.
#[cfg(unix)]
const SERVERS: [&str; 4] = [
    "/usr/libexec/sftp-server",
    "/usr/lib/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
];

/// The same checks against the server a real machine runs. It speaks on
/// its input and output, as it does under `sshd`, so no login is needed.
#[cfg(unix)]
#[tokio::test]
async fn the_servers_own_sftp_server_passes_the_checks() {
    let server = SERVERS.iter().find(|path| Path::new(path).exists());
    // macOS always has one. A Linux system has one if it has an SSH server.
    #[cfg(target_os = "macos")]
    let server = Some(server.expect("macOS has an sftp-server"));
    let Some(server) = server else {
        eprintln!("no sftp-server on this system: not checked against a real one");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let sample = Sample::new().unwrap();
    let root = written(&sample, tmp.path());
    let session = Session::run(tokio::process::Command::new(server), "this machine").await;
    let remote = Remote::over(session.unwrap(), "this machine", root.to_str().unwrap()).await;
    let remote = remote.unwrap();
    assert_eq!(check_source(&remote, &sample).await, Ok(()));

    // A megabyte and a half, in one read, as a transfer asks for a chunk.
    let bytes: Vec<u8> = (0..1_500_000u32)
        .map(|i| (i ^ (i >> 9)).to_le_bytes()[0])
        .collect();
    let big = tmp.path().canonicalize().unwrap().join("big.bin");
    std::fs::write(&big, &bytes).unwrap();
    let session = Session::run(tokio::process::Command::new(server), "this machine").await;
    let remote = Remote::over(session.unwrap(), "this machine", big.to_str().unwrap()).await;
    let read = remote.unwrap().read(0, 0, 1_500_000).await.unwrap();
    assert!(read == bytes);
}
