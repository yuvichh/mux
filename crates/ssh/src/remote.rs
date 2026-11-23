//! A file or a folder on another machine, as a place to read from.

use crate::session::Session;
use crate::wire::{self, Attrs, Kind, Named, Reply, Request, status};
use model::artifact::path::{depth, is_safe_target};
use model::artifact::{Entry, EntryKind, EntryMetadata, PERMISSION_BITS, Timestamp, is_part_name};
use model::mirror::TRASH;
use model::place::{
    Arrived, Capabilities, Description, Failure, Foreknown, Pending, Reader, Source,
};
use std::ffi::OsString;
use std::sync::Arc;
use tokio::process::Command;
use tokio::task::JoinSet;

/// The most directories that are being listed at one time.
const LISTED_AT_ONCE: usize = 16;

/// How the other machine is reached.
#[derive(Debug, Clone)]
pub struct Options {
    /// The program to run: `ssh`, unless another is to stand in for it. It
    /// is run as `ssh` is, and must carry SFTP on its input and output.
    pub program: OsString,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            program: "ssh".into(),
        }
    }
}

/// A file or a folder on a machine reached by SSH.
///
/// What it holds is looked at once, when it is found: every folder is
/// listed, and no file is read. A file that is then found to have another
/// size or another time when it is opened is reported as changed.
#[derive(Debug, Clone)]
pub struct Remote(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    session: Arc<Session>,
    /// The machine, for what is said of it.
    host: String,
    described: Description,
    /// Where each entry is on the other machine, and for a file, what was
    /// recorded of it.
    found: Vec<Found>,
    skipped: usize,
}

/// One entry as it was found.
#[derive(Debug, Clone)]
struct Found {
    /// Its path there, from the root.
    path: String,
    attrs: Attrs,
}

impl Remote {
    /// Reaches `host` with the system's `ssh`, and looks at what is at
    /// `path` there. A path that does not begin with `/` is read from the
    /// home folder, and an empty one is the home folder.
    ///
    /// `host` is whatever `ssh` takes: a name, an address, `user@host`, or
    /// a host of the user's SSH configuration. Keys, agents, jump hosts and
    /// a password typed at the terminal work as they do for `ssh`.
    ///
    /// # Errors
    ///
    /// Returns a failure if the machine cannot be reached or does not
    /// offer SFTP, if nothing is at the path, or if what is there cannot
    /// be listed.
    pub async fn find(
        host: &str,
        port: Option<u16>,
        path: &str,
        options: &Options,
    ) -> Result<Self, Failure> {
        let mut program = Command::new(&options.program);
        // What OpenSSH's own `sftp` passes: nothing but this conversation
        // goes over the connection, and no terminal is asked for.
        program.args([
            "-x",
            "-oForwardAgent=no",
            "-oPermitLocalCommand=no",
            "-oClearAllForwardings=yes",
            "-oRemoteCommand=none",
            "-oRequestTTY=no",
        ]);
        if let Some(port) = port {
            program.arg("-p").arg(port.to_string());
        }
        // After `--` nothing is read as an option, whatever the host is.
        program.args(["-s", "--", host, "sftp"]);
        let session = Session::run(program, host).await?;
        Self::over(session, host, path).await
    }

    /// Looks at what is at `path` in a conversation that has begun.
    pub(crate) async fn over(
        session: Arc<Session>,
        host: &str,
        path: &str,
    ) -> Result<Self, Failure> {
        let place = format!("{host}:{path}");
        let asked = if path.is_empty() { "." } else { path };
        let resolved = session
            .ask(&Request::Realpath(asked.as_bytes()))
            .answer()
            .await?;
        let root = match resolved {
            Reply::Name(mut names) if names.len() == 1 => text(names.remove(0).name, &place)?,
            other => return Err(refused(&other, &place)),
        };
        let attrs = match session
            .ask(&Request::Stat(root.as_bytes()))
            .answer()
            .await?
        {
            Reply::Attrs(attrs) => attrs,
            other => return Err(refused(&other, &place)),
        };
        let name = root.rsplit('/').find(|part| !part.is_empty());
        let Some(name) = name.map(str::to_owned) else {
            return Err(Failure::Fatal(format!(
                "{place} is the root of the machine, which has no name to be copied under. Name a folder in it."
            )));
        };

        let mut listed = Listing::default();
        let kind = match attrs.kind() {
            Kind::Directory => EntryKind::Directory,
            Kind::File => EntryKind::File {
                size: size_of(&attrs, &place)?,
            },
            Kind::Link | Kind::Other => {
                return Err(Failure::Fatal(format!(
                    "{place} is neither a file nor a folder"
                )));
            }
        };
        let is_folder = kind == EntryKind::Directory;
        listed.entries.push((
            String::new(),
            kind,
            Found {
                path: root.clone(),
                attrs,
            },
        ));
        if is_folder {
            listed.walk(&session, &root, host).await?;
        }
        listed.entries[1..].sort_by(|a, b| a.0.cmp(&b.0));

        let mut described = Description {
            name,
            entries: Vec::with_capacity(listed.entries.len()),
            known: Vec::with_capacity(listed.entries.len()),
        };
        let mut found = Vec::with_capacity(listed.entries.len());
        for (path, kind, at) in listed.entries {
            described.known.push(known(&kind, &at.attrs));
            described.entries.push((
                Entry {
                    path,
                    kind: kind.clone(),
                },
                recorded(&kind, &at.attrs),
            ));
            found.push(at);
        }
        Ok(Self(Arc::new(Inner {
            session,
            host: host.to_owned(),
            described,
            found,
            skipped: listed.skipped,
        })))
    }

    /// How many things were left out: devices, sockets and the like, links
    /// that point outside the folder, and files that a copy by mux never
    /// finished.
    pub fn skipped(&self) -> usize {
        self.0.skipped
    }

    /// The place on the other machine that entry `file` is, for what is
    /// said of it.
    fn named(&self, file: usize) -> String {
        let path = self
            .0
            .found
            .get(file)
            .map_or("", |found| found.path.as_str());
        format!("{}:{path}", self.0.host)
    }
}

/// What is recorded of an entry, as a manifest records it.
fn recorded(kind: &EntryKind, attrs: &Attrs) -> EntryMetadata {
    match kind {
        // A link's own permissions and time mean nothing and are not
        // restored, so they are recorded the same everywhere.
        EntryKind::Symlink { .. } => EntryMetadata {
            mode: PERMISSION_BITS,
            modified: None,
        },
        EntryKind::File { .. } | EntryKind::Directory => EntryMetadata {
            mode: attrs
                .mode
                .map_or(PERMISSION_BITS, |mode| (mode & 0o777) as u16),
            modified: attrs.modified.map(|seconds| Timestamp {
                seconds: i64::from(seconds),
                nanos: 0,
            }),
        },
    }
}

/// What tells one version of a file from another: its size and the time it
/// was last changed. Nothing, for a file whose time the server does not
/// give, and such a file is begun again if a copy of it is cut off.
fn known(kind: &EntryKind, attrs: &Attrs) -> Foreknown {
    let validator = match (kind, attrs.size, attrs.modified) {
        (EntryKind::File { .. }, Some(size), Some(modified)) => {
            [&size.to_le_bytes()[..], &modified.to_le_bytes()].concat()
        }
        _ => Vec::new(),
    };
    Foreknown {
        validator,
        digest: None,
    }
}

fn size_of(attrs: &Attrs, place: &str) -> Result<u64, Failure> {
    attrs.size.ok_or_else(|| {
        Failure::Fatal(format!(
            "the other machine does not say how large {place} is"
        ))
    })
}

/// A name or a path of the other machine, as text.
fn text(bytes: Vec<u8>, place: &str) -> Result<String, Failure> {
    String::from_utf8(bytes).map_err(|odd| {
        let shown = String::from_utf8_lossy(odd.as_bytes()).into_owned();
        Failure::Fatal(format!(
            "{place} holds the name {shown:?}, which is not UTF-8 and cannot be copied"
        ))
    })
}

/// What an answer that is not what was asked for means.
fn refused(reply: &Reply, place: &str) -> Failure {
    match reply {
        Reply::Status { code, .. } if *code == status::NO_SUCH_FILE => {
            Failure::Absent(format!("nothing is at {place}"))
        }
        Reply::Status { code, .. } if *code == status::PERMISSION_DENIED => {
            Failure::Fatal(format!("{place} may not be read: permission was denied"))
        }
        Reply::Status { message, .. } if !message.is_empty() => {
            Failure::Fatal(format!("{place} could not be read: {message}"))
        }
        Reply::Status { code, .. } => Failure::Fatal(format!(
            "{place} could not be read: the other machine answered {code}"
        )),
        _ => Failure::Corrupt(format!(
            "the other machine answered a question about {place} with something else"
        )),
    }
}

/// What a folder on the other machine holds, as it is found.
#[derive(Debug, Default)]
struct Listing {
    /// Each entry: its path in the tree, what it is, and where it is.
    entries: Vec<(String, EntryKind, Found)>,
    skipped: usize,
}

/// A directory still to be listed: its path in the tree, and where it is.
type Folder = (String, String);

impl Listing {
    /// Lists the folder at `root` and every folder in it, several at a
    /// time.
    async fn walk(
        &mut self,
        session: &Arc<Session>,
        root: &str,
        host: &str,
    ) -> Result<(), Failure> {
        let mut waiting: Vec<Folder> = vec![(String::new(), root.to_owned())];
        let mut listing = JoinSet::new();
        loop {
            while listing.len() < LISTED_AT_ONCE {
                let Some((path, at)) = waiting.pop() else {
                    break;
                };
                let (session, host) = (session.clone(), host.to_owned());
                listing.spawn(async move { list(&session, &path, &at, &host).await });
            }
            let Some(listed) = listing.join_next().await else {
                return Ok(());
            };
            let listed =
                listed.map_err(|e| Failure::Fatal(format!("listing was cut short: {e}")))??;
            self.skipped += listed.skipped;
            for (path, kind, found) in listed.entries {
                if kind == EntryKind::Directory {
                    waiting.push((path.clone(), found.path.clone()));
                }
                self.entries.push((path, kind, found));
            }
        }
    }
}

/// Lists one directory: `at` on the other machine, `path` in the tree.
async fn list(session: &Session, path: &str, at: &str, host: &str) -> Result<Listing, Failure> {
    let place = format!("{host}:{at}");
    let handle = match session
        .ask(&Request::Opendir(at.as_bytes()))
        .answer()
        .await?
    {
        Reply::Handle(handle) => Opened { session, handle },
        other => return Err(refused(&other, &place)),
    };
    let mut names: Vec<Named> = Vec::new();
    loop {
        match session
            .ask(&Request::Readdir(&handle.handle))
            .answer()
            .await?
        {
            Reply::Name(more) => names.extend(more),
            Reply::Status { code, .. } if code == status::EOF => break,
            other => return Err(refused(&other, &place)),
        }
    }
    drop(handle);

    let mut listed = Listing::default();
    for Named { name, attrs } in names {
        if name == b"." || name == b".." {
            continue;
        }
        // What a sync took away from this folder is not part of what the
        // folder holds.
        if path.is_empty() && name == TRASH.as_bytes() {
            continue;
        }
        let name = text(name, &place)?;
        // A part file is mux's own unfinished work, and no manifest may
        // name one.
        if is_part_name(&name) {
            listed.skipped += 1;
            continue;
        }
        let inside = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let there = format!("{}/{name}", at.trim_end_matches('/'));
        // Anything else is a device, a socket or a pipe, and a link is
        // kept only if it stays inside the folder.
        let kind = match attrs.kind() {
            Kind::Directory => Some(EntryKind::Directory),
            Kind::File => Some(EntryKind::File {
                size: size_of(&attrs, &format!("{host}:{there}"))?,
            }),
            Kind::Link => target(session, &there, &inside)
                .await?
                .map(|target| EntryKind::Symlink { target }),
            Kind::Other => None,
        };
        let Some(kind) = kind else {
            listed.skipped += 1;
            continue;
        };
        listed
            .entries
            .push((inside, kind, Found { path: there, attrs }));
    }
    Ok(listed)
}

/// Where the link at `there` points, if its target stays inside the tree.
/// `inside` is the link's own path in the tree.
async fn target(session: &Session, there: &str, inside: &str) -> Result<Option<String>, Failure> {
    let read = session
        .ask(&Request::Readlink(there.as_bytes()))
        .answer()
        .await?;
    let Reply::Name(mut names) = read else {
        return Ok(None);
    };
    let Some(Ok(target)) = names.pop().map(|named| String::from_utf8(named.name)) else {
        return Ok(None);
    };
    Ok(is_safe_target(target.as_bytes(), depth(inside.as_bytes())).then_some(target))
}

/// A file or a directory that is open on the other machine. It is let go
/// of there when it is let go of here.
#[derive(Debug)]
struct Opened<'a> {
    session: &'a Session,
    handle: Vec<u8>,
}

impl Drop for Opened<'_> {
    fn drop(&mut self) {
        self.session.send(&Request::Close(&self.handle));
    }
}

impl Source for Remote {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            // Every request is a packet of the one connection.
            multiplexed: true,
        }
    }

    fn describe(&self) -> Pending<'_, Description> {
        Box::pin(std::future::ready(Ok(self.0.described.clone())))
    }

    fn open(&self, file: usize, offset: u64, len: u64) -> Pending<'_, Box<dyn Reader>> {
        Box::pin(async move {
            let inner = &self.0;
            let place = self.named(file);
            let entry = inner.described.entries.get(file).zip(inner.found.get(file));
            let Some((
                (
                    Entry {
                        kind: EntryKind::File { size },
                        ..
                    },
                    _,
                ),
                found,
            )) = entry
            else {
                return Err(Failure::Absent(format!("{place} is not a file")));
            };
            if offset.checked_add(len).is_none_or(|end| end > *size) {
                return Err(Failure::Absent(format!(
                    "{place} is a file of {size} bytes"
                )));
            }
            let opened = inner.session.ask(&Request::Open(found.path.as_bytes()));
            let handle = match opened.answer().await? {
                Reply::Handle(handle) => handle,
                Reply::Status { code, .. } if code == status::NO_SUCH_FILE => {
                    return Err(changed(&place));
                }
                other => return Err(refused(&other, &place)),
            };
            let file = OpenFile {
                session: inner.session.clone(),
                handle,
            };
            // The file that is open is the one that was looked at, or the
            // bytes of two versions would be put together.
            let now = match inner
                .session
                .ask(&Request::Fstat(&file.handle))
                .answer()
                .await?
            {
                Reply::Attrs(now) => now,
                other => return Err(refused(&other, &place)),
            };
            if now.size != found.attrs.size || now.modified != found.attrs.modified {
                return Err(changed(&place));
            }
            let reader: Box<dyn Reader> = Box::new(Ranged {
                file,
                place,
                at: offset,
                left: len,
            });
            Ok(reader)
        })
    }
}

fn changed(place: &str) -> Failure {
    Failure::Changed(format!("{place} has changed since the copy began"))
}

/// A file that is open on the other machine, for as long as it is read.
#[derive(Debug)]
struct OpenFile {
    session: Arc<Session>,
    handle: Vec<u8>,
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        self.session.send(&Request::Close(&self.handle));
    }
}

/// The bytes of one file from where it was opened to where it was opened
/// to, read in order.
#[derive(Debug)]
struct Ranged {
    file: OpenFile,
    place: String,
    /// Where the next byte is in the file.
    at: u64,
    /// How many bytes are left of what it was opened for.
    left: u64,
}

impl Reader for Ranged {
    /// Every piece of what is asked for is asked for at once, so that the
    /// answers come one after another without a wait between them.
    fn read<'a>(&'a mut self, len: u32, arrived: &'a Arrived) -> Pending<'a, Vec<u8>> {
        Box::pin(async move {
            if u64::from(len) > self.left {
                return Err(Failure::Fatal(format!(
                    "{} was opened for fewer bytes than are read from it",
                    self.place
                )));
            }
            let (session, handle) = (&self.file.session, &self.file.handle);
            let ask = |offset: u64, len: u32| {
                session.ask(&Request::Read {
                    handle,
                    offset,
                    len,
                })
            };
            let pieces: Vec<_> = (0..len)
                .step_by(wire::READ_SIZE as usize)
                .map(|from| {
                    let piece = wire::READ_SIZE.min(len - from);
                    (from, piece, ask(self.at + u64::from(from), piece))
                })
                .collect();
            let mut read = vec![0; len as usize];
            for (from, piece, mut asked) in pieces {
                // A server may give fewer bytes than it was asked for, and
                // is then asked for the rest.
                let mut got = 0;
                while got < piece {
                    let data = match asked.answer().await? {
                        Reply::Data(data)
                            if !data.is_empty() && data.len() <= (piece - got) as usize =>
                        {
                            data
                        }
                        Reply::Data(_) => {
                            return Err(Failure::Corrupt(format!(
                                "the other machine sent bytes of {} that were not asked for",
                                self.place
                            )));
                        }
                        // The file ends before the size it was found with.
                        Reply::Status { code, .. } if code == status::EOF => {
                            return Err(changed(&self.place));
                        }
                        other => return Err(refused(&other, &self.place)),
                    };
                    let start = (from + got) as usize;
                    read[start..start + data.len()].copy_from_slice(&data);
                    arrived.add(data.len());
                    got += u32::try_from(data.len()).unwrap_or(piece);
                    if got == piece {
                        break;
                    }
                    asked = ask(self.at + u64::from(from + got), piece - got);
                }
            }
            self.at += u64::from(len);
            self.left -= u64::from(len);
            Ok(read)
        })
    }
}
