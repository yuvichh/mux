//! A tree's files in a folder on another machine, as a place to write to.
//!
//! It follows the rules of a local folder. A file is written under a part
//! name beside its final one, and is given its final name in one step once
//! every byte of it is there. What the engine notes beside a file is kept
//! after the file's own bytes, and is cut off as the file is named.
//!
//! Every call here waits for the other machine, on a thread that may wait.
//! What makes it quick is that many requests are on their way at once: all
//! the pieces of a chunk, and everything that names a file.

use crate::remote::{Options, reach};
use crate::session::{Asked, Session};
use crate::wire::{self, Attrs, Kind, Reply, Request, status};
use model::artifact::{Bitfield, EntryKind, Hash, PART_SUFFIX, Tree};
use model::assembly::Assembly;
use model::place::{
    Arrived, Begun, Capabilities, Description, Expected, Failure, Naming, Pending, Reader, Sink,
    Source,
};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// The most part files that are kept open on the other machine at one
/// time. A server lets one user have so many files open, often 1024, and a
/// tree can have more files than that being written. It is more than the
/// files a copy has on their way at once, so that a file is still open
/// when it comes to be named.
const OPEN_AT_ONCE: usize = 256;

/// The most bytes that are on their way to the other machine at one time,
/// written and not yet answered for. Many small files fit in it together,
/// which is what makes them quick. A few large chunks fill it, and the
/// rest wait their turn here and not in memory.
const ON_THEIR_WAY: usize = 16 * 1024 * 1024;

/// A machine that has been reached by SSH, to put things on.
#[derive(Debug, Clone)]
pub struct Machine {
    session: Arc<Session>,
    host: String,
}

impl Machine {
    /// Reaches `host` with the system's `ssh`, as [`crate::Remote::find`]
    /// does.
    ///
    /// # Errors
    ///
    /// Returns a failure if the machine cannot be reached or does not offer
    /// SFTP.
    pub async fn reach(host: &str, port: Option<u16>, options: &Options) -> Result<Self, Failure> {
        Ok(Self {
            session: reach(host, port, options).await?,
            host: host.to_owned(),
        })
    }

    /// A machine at the other end of a conversation that has begun.
    #[cfg(test)]
    pub(crate) fn over(session: Arc<Session>, host: &str) -> Self {
        Self {
            session,
            host: host.to_owned(),
        }
    }

    /// Whether a folder is at `path` there. `None` if nothing is. A path
    /// that does not begin with `/` is read from the home folder, and an
    /// empty one is the home folder.
    ///
    /// # Errors
    ///
    /// Returns a failure if the path cannot be looked at.
    pub async fn is_folder(&self, path: &str) -> Result<Option<bool>, Failure> {
        let asked = if path.is_empty() { "." } else { path };
        let found = self.session.ask(&Request::Stat(asked.as_bytes()));
        match found.answer().await? {
            Reply::Attrs(attrs) => Ok(Some(attrs.kind() == Kind::Directory)),
            Reply::Status { code, .. } if code == status::NO_SUCH_FILE => Ok(None),
            other => Err(refused(&other, "look at", &format!("{}:{path}", self.host))),
        }
    }
}

/// A tree's files in a folder on a machine reached by SSH.
///
/// It is a [`Sink`], and a [`Source`] of what it holds. Reading through
/// [`Source`] runs on a thread that may wait, and so needs a Tokio
/// runtime.
#[derive(Debug, Clone)]
pub struct RemoteFolder(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    session: Arc<Session>,
    /// The machine, for what is said of it.
    host: String,
    tree: Arc<Tree>,
    /// Where the tree is there: the file itself, or the directory that
    /// holds the tree.
    root: String,
    /// The folder the tree is put in, which is made if it is not there.
    within: String,
    /// What was found there for each entry, the last time it was looked
    /// at, until the place is made ready with it.
    seen: Mutex<Option<Arc<Vec<Seen>>>>,
    /// Whether it has been looked at: from then on it is known which
    /// entries have a part file.
    looked: AtomicBool,
    /// Whether each entry has its final name.
    in_place: Vec<AtomicBool>,
    /// The files that are taken as they are, unread. See [`Sink::keep`].
    kept: Vec<AtomicBool>,
    /// Whether a part file is there for each entry, as far as is known.
    parted: Vec<AtomicBool>,
    /// The part files that are open.
    open: Mutex<Open>,
    /// How many bytes are on their way, and whoever waits for room.
    on_their_way: (Mutex<usize>, Condvar),
    /// Links the tree holds that the other machine was not asked to make.
    skipped_links: AtomicUsize,
}

/// What is there for one entry, under its final name.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    there: Option<Attrs>,
}

/// The part files that are open, and the order they were last used in.
#[derive(Debug, Default)]
struct Open {
    files: HashMap<usize, Arc<Handle>>,
    used: VecDeque<usize>,
}

/// A file that is open on the other machine. It is let go of there when it
/// is let go of here.
#[derive(Debug)]
struct Handle {
    session: Arc<Session>,
    handle: Vec<u8>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.session.send(&Request::Close(&self.handle));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RemoteFolder {
    /// The tree inside the folder `dir` on `machine`, under its own name,
    /// to be filled in. Nothing is touched until it is asked something.
    pub fn inside(machine: &Machine, tree: Arc<Tree>, dir: &str) -> Self {
        let within = if dir.is_empty() { "." } else { dir };
        let root = format!("{}/{}", within.trim_end_matches('/'), tree.name());
        // The root of the machine is the one path that ends in a slash.
        let root = if within == "/" {
            format!("/{}", tree.name())
        } else {
            root
        };
        let flags = || tree.entries().iter().map(|_| AtomicBool::new(false));
        Self(Arc::new(Inner {
            session: machine.session.clone(),
            host: machine.host.clone(),
            in_place: flags().collect(),
            kept: flags().collect(),
            parted: flags().collect(),
            seen: Mutex::default(),
            looked: AtomicBool::new(false),
            open: Mutex::default(),
            on_their_way: (Mutex::new(0), Condvar::new()),
            skipped_links: AtomicUsize::new(0),
            within: within.to_owned(),
            root,
            tree,
        }))
    }

    /// Where the tree is on the other machine.
    pub fn root(&self) -> &str {
        &self.0.root
    }

    /// How many links of the tree were left out, because the other
    /// machine's server cannot put one in place in one step.
    pub fn skipped_links(&self) -> usize {
        self.0.skipped_links.load(Ordering::Relaxed)
    }

    fn final_path(&self, entry: usize) -> String {
        match self.0.tree.entries().get(entry).map(|e| e.path.as_str()) {
            Some(path) if !path.is_empty() => format!("{}/{path}", self.0.root),
            _ => self.0.root.clone(),
        }
    }

    /// Where `file` is written until it is whole: beside its final place.
    fn part_path(&self, file: usize) -> String {
        format!("{}{PART_SUFFIX}", self.final_path(file))
    }

    /// A path there, for what is said of it.
    fn named(&self, path: &str) -> String {
        format!("{}:{path}", self.0.host)
    }

    fn size_of(&self, file: usize) -> Result<u64, Failure> {
        let entry = self.0.tree.entries().get(file);
        entry
            .map(model::artifact::Entry::size)
            .ok_or_else(|| Failure::Absent(format!("there is no file {file}")))
    }

    fn is_in_place(&self, file: usize) -> bool {
        self.0
            .in_place
            .get(file)
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn is_parted(&self, file: usize) -> bool {
        self.0
            .parted
            .get(file)
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn set(flags: &[AtomicBool], file: usize, value: bool) {
        if let Some(flag) = flags.get(file) {
            flag.store(value, Ordering::Release);
        }
    }

    /// Asks, and waits for the answer.
    fn ask(&self, request: &Request<'_>) -> Result<Reply, Failure> {
        self.0.session.ask(request).wait()
    }

    /// Makes sure it is known which entries have a part file.
    fn look(&self) -> Result<(), Failure> {
        if self.0.looked.load(Ordering::Acquire) {
            return Ok(());
        }
        self.seen().map(|_| ())
    }

    /// What is there for each entry: looked at in one go, with every
    /// question on its way before the first answer is read.
    fn seen(&self) -> Result<Arc<Vec<Seen>>, Failure> {
        let mut seen = lock(&self.0.seen);
        if let Some(seen) = &*seen {
            return Ok(seen.clone());
        }
        let entries = self.0.tree.entries();
        let asked: Vec<(Asked, Option<Asked>)> = (0..entries.len())
            .map(|entry| {
                let there = self.final_path(entry);
                let there = self.0.session.ask(&Request::Lstat(there.as_bytes()));
                let part = entries[entry].is_file().then(|| {
                    let part = self.part_path(entry);
                    self.0.session.ask(&Request::Lstat(part.as_bytes()))
                });
                (there, part)
            })
            .collect();
        let mut found = Vec::with_capacity(entries.len());
        for (entry, (there, part)) in asked.into_iter().enumerate() {
            let there = self.attrs(there.wait()?, &self.final_path(entry))?;
            let part = match part {
                Some(part) => self.attrs(part.wait()?, &self.part_path(entry))?,
                None => None,
            };
            Self::set(&self.0.parted, entry, part.is_some());
            found.push(Seen { there });
        }
        let found = Arc::new(found);
        *seen = Some(found.clone());
        self.0.looked.store(true, Ordering::Release);
        Ok(found)
    }

    /// What an answer says is at `path`: nothing, or what is recorded of
    /// it.
    fn attrs(&self, reply: Reply, path: &str) -> Result<Option<Attrs>, Failure> {
        match reply {
            Reply::Attrs(attrs) => Ok(Some(attrs)),
            Reply::Status { code, .. } if code == status::NO_SUCH_FILE => Ok(None),
            other => Err(refused(&other, "look at", &self.named(path))),
        }
    }

    /// The part file of `file`, open to be written and read. It is made if
    /// it is not there.
    fn part(&self, file: usize) -> Result<Arc<Handle>, Failure> {
        if let Some(open) = self.reused(file) {
            return Ok(open);
        }
        let path = self.part_path(file);
        let handle = match self.ask(&Request::Create(path.as_bytes()))? {
            Reply::Handle(handle) => handle,
            other => return Err(refused(&other, "write", &self.named(&path))),
        };
        Self::set(&self.0.parted, file, true);
        let opened = Arc::new(Handle {
            session: self.0.session.clone(),
            handle,
        });
        let mut open = lock(&self.0.open);
        // Another thread may have opened it in the meantime: one is kept.
        let kept = open.files.entry(file).or_insert(opened).clone();
        open.used.retain(|&other| other != file);
        open.used.push_back(file);
        while open.used.len() > OPEN_AT_ONCE {
            // Whoever is still using it keeps it open until they are done.
            if let Some(oldest) = open.used.pop_front() {
                open.files.remove(&oldest);
            }
        }
        Ok(kept)
    }

    fn reused(&self, file: usize) -> Option<Arc<Handle>> {
        let mut open = lock(&self.0.open);
        let found = open.files.get(&file).cloned()?;
        open.used.retain(|&other| other != file);
        open.used.push_back(file);
        Some(found)
    }

    /// Lets go of the part file of `file`, if it is open.
    fn let_go(&self, file: usize) -> Option<Arc<Handle>> {
        let mut open = lock(&self.0.open);
        open.used.retain(|&other| other != file);
        open.files.remove(&file)
    }

    /// Reads `len` bytes at `offset` of an open file, every piece asked for
    /// at once. A file that ends before them is a failure.
    fn read_from(
        &self,
        open: &Handle,
        offset: u64,
        len: u32,
        place: &str,
    ) -> Result<Vec<u8>, Failure> {
        let ask = |at: u64, len: u32| {
            self.0.session.ask(&Request::Read {
                handle: &open.handle,
                offset: at,
                len,
            })
        };
        let pieces: Vec<_> = (0..len)
            .step_by(wire::READ_SIZE as usize)
            .map(|from| {
                let piece = wire::READ_SIZE.min(len - from);
                (from, piece, ask(offset + u64::from(from), piece))
            })
            .collect();
        let mut read = vec![0; len as usize];
        for (from, piece, mut asked) in pieces {
            // A server may give fewer bytes than it was asked for, and is
            // then asked for the rest.
            let mut got = 0;
            while got < piece {
                let data = match asked.wait()? {
                    Reply::Data(data)
                        if !data.is_empty() && data.len() <= (piece - got) as usize =>
                    {
                        data
                    }
                    Reply::Data(_) => {
                        return Err(Failure::Corrupt(format!(
                            "the other machine sent bytes of {place} that were not asked for"
                        )));
                    }
                    Reply::Status { code, .. } if code == status::EOF => {
                        return Err(Failure::Absent(format!(
                            "{place} ends before what is read of it"
                        )));
                    }
                    other => return Err(refused(&other, "read", place)),
                };
                let start = (from + got) as usize;
                read[start..start + data.len()].copy_from_slice(&data);
                got += u32::try_from(data.len()).unwrap_or(piece);
                if got == piece {
                    break;
                }
                asked = ask(offset + u64::from(from + got), piece - got);
            }
        }
        Ok(read)
    }

    /// Reads from whichever file holds the bytes of `file` at the moment.
    fn read_at(&self, file: usize, offset: u64, len: u32) -> Result<Vec<u8>, Failure> {
        if !self.is_in_place(file) {
            let path = self.named(&self.part_path(file));
            if !self.is_parted(file) {
                return Err(Failure::Absent(format!("nothing is at {path}")));
            }
            return self.read_from(&*self.part(file)?, offset, len, &path);
        }
        let path = self.final_path(file);
        let place = self.named(&path);
        let handle = match self.ask(&Request::Open(path.as_bytes()))? {
            Reply::Handle(handle) => handle,
            Reply::Status { code, .. } if code == status::NO_SUCH_FILE => {
                return Err(Failure::Absent(format!("nothing is at {place}")));
            }
            other => return Err(refused(&other, "read", &place)),
        };
        let open = Handle {
            session: self.0.session.clone(),
            handle,
        };
        self.read_from(&open, offset, len, &place)
    }

    /// Writes `data` at `offset` of an open file, every piece on its way
    /// before the first answer is read.
    fn write_to(
        &self,
        open: &Handle,
        offset: u64,
        data: &[u8],
        place: &str,
    ) -> Result<(), Failure> {
        // Room is waited for before anything is sent. A write larger than
        // all the room there is goes when nothing else is on its way.
        let (count, room) = &self.0.on_their_way;
        let wanted = data.len().min(ON_THEIR_WAY);
        let mut going = lock(count);
        while *going + wanted > ON_THEIR_WAY {
            going = room.wait(going).unwrap_or_else(PoisonError::into_inner);
        }
        *going += wanted;
        drop(going);
        let written = self.send_to(open, offset, data, place);
        *lock(count) -= wanted;
        room.notify_all();
        written
    }

    fn send_to(&self, open: &Handle, offset: u64, data: &[u8], place: &str) -> Result<(), Failure> {
        let mut at = offset;
        let asked: Vec<Asked> = data
            .chunks(wire::WRITE_SIZE)
            .map(|piece| {
                let asked = self.0.session.ask(&Request::Write {
                    handle: &open.handle,
                    offset: at,
                    data: piece,
                });
                at += piece.len() as u64;
                asked
            })
            .collect();
        // Every answer is read, so that none is left to be taken for the
        // answer to something else.
        let mut written = Ok(());
        for asked in asked {
            let done = asked.wait().and_then(|reply| done(&reply, "write", place));
            written = written.and(done);
        }
        written
    }

    /// The chunks of `file` that are right in an open file of `len` bytes.
    /// Only a chunk whose digest is given is read.
    fn verify(
        &self,
        file: usize,
        open: &Handle,
        len: u64,
        expected: &Expected<'_>,
        examined: &(dyn Fn(u64) + Sync),
        place: &str,
    ) -> Result<Vec<u64>, Failure> {
        let tree = &self.0.tree;
        let mut right = Vec::new();
        for index in tree.file_chunks(file) {
            let (Some(at), Some(digest)) = (tree.locate(index), expected(index)) else {
                continue;
            };
            if at.offset + u64::from(at.len) > len {
                continue;
            }
            examined(u64::from(at.len));
            if Hash::of(&self.read_from(open, at.offset, at.len, place)?) == digest {
                right.push(index);
            }
        }
        Ok(right)
    }

    /// Gives a file that has every byte its final name: cuts off what was
    /// noted beside it, sets its time and permissions, and renames it over
    /// whatever is there. All of it is on its way before the first answer
    /// is read, and the other machine does it in that order.
    fn name(&self, file: usize) -> Result<(), Failure> {
        let size = self.size_of(file)?;
        let open = self.part(file)?;
        let (part, path) = (self.part_path(file), self.final_path(file));
        let place = self.named(&part);
        let session = &self.0.session;
        let fsetstat = |attrs: Attrs| {
            session.ask(&Request::Fsetstat {
                handle: &open.handle,
                attrs,
            })
        };
        let mut asked = vec![fsetstat(Attrs {
            size: Some(size),
            ..Attrs::default()
        })];
        if let Some(recorded) = self.0.tree.metadata(file) {
            let modified = recorded.modified.map(|time| time.seconds);
            asked.push(fsetstat(Attrs {
                size: None,
                mode: Some(u32::from(recorded.mode)),
                // The protocol counts seconds in 32 bits, from 1970.
                modified: modified.and_then(|seconds| u32::try_from(seconds).ok()),
            }));
        }
        // The file is closed before it is renamed. Both are asked for here,
        // and in this order.
        drop(self.let_go(file));
        drop(open);
        let replaces = session.greeting().replaces;
        let rename = if replaces {
            Request::Replace {
                from: part.as_bytes(),
                to: path.as_bytes(),
            }
        } else {
            Request::Rename {
                from: part.as_bytes(),
                to: path.as_bytes(),
            }
        };
        let renamed = session.ask(&rename);

        let mut ready = Ok(());
        for asked in asked {
            let done = asked
                .wait()
                .and_then(|reply| done(&reply, "finish", &place));
            ready = ready.and(done);
        }
        let renamed = renamed.wait();
        ready?;
        match renamed? {
            Reply::Status { code, .. } if code == status::OK => {}
            // A server without the request that replaces refuses to rename
            // over a file. What is there is removed first, and for that
            // moment nothing has the name.
            other if !replaces => {
                let _ = self.ask(&Request::Remove(path.as_bytes()));
                let again = self.ask(&Request::Rename {
                    from: part.as_bytes(),
                    to: path.as_bytes(),
                })?;
                done(&again, "name", &self.named(&path))
                    .map_err(|_| refused(&other, "name", &self.named(&path)))?;
            }
            other => return Err(refused(&other, "name", &self.named(&path))),
        }
        Self::set(&self.0.parted, file, false);
        Self::set(&self.0.in_place, file, true);
        Ok(())
    }

    /// Removes the part file of `file`, if one is known to be there.
    fn discard(&self, file: usize) -> Result<(), Failure> {
        drop(self.let_go(file));
        if !self.is_parted(file) {
            return Ok(());
        }
        let part = self.part_path(file);
        match self.ask(&Request::Remove(part.as_bytes()))? {
            Reply::Status { code, .. } if code == status::OK || code == status::NO_SUCH_FILE => {
                Self::set(&self.0.parted, file, false);
                Ok(())
            }
            other => Err(refused(&other, "remove", &self.named(&part))),
        }
    }

    /// Makes the folder the tree is put in, and every folder on the way to
    /// it that is not there.
    fn make_room(&self) -> Result<(), Failure> {
        let within = self.0.within.as_str();
        let found = self.ask(&Request::Stat(within.as_bytes()))?;
        if let Reply::Attrs(attrs) = &found {
            if attrs.kind() == Kind::Directory {
                return Ok(());
            }
            return Err(Failure::Fatal(format!(
                "{} is there and is not a folder",
                self.named(within)
            )));
        }
        // Each folder of the path, the outermost first. One that is there
        // already refuses, and what counts is that the last one is there.
        let mut so_far = String::new();
        let mut made = Vec::new();
        for (number, part) in within.split('/').enumerate() {
            if number > 0 {
                so_far.push('/');
            }
            so_far.push_str(part);
            if !part.is_empty() && part != "." {
                made.push(self.0.session.ask(&Request::Mkdir(so_far.as_bytes())));
            }
        }
        let last = made.pop();
        for asked in made {
            asked.wait()?;
        }
        match last {
            Some(last) => done(&last.wait()?, "make", &self.named(within)),
            None => Err(refused(&found, "look at", &self.named(within))),
        }
    }

    /// Makes sure each directory of the tree is there, and is a directory:
    /// a link standing where one belongs is refused, so that nothing is
    /// written through a link to somewhere else.
    fn make_directories(&self, seen: &[Seen]) -> Result<(), Failure> {
        let entries = self.0.tree.entries();
        let mut made = Vec::new();
        for (entry, described) in entries.iter().enumerate() {
            if described.kind != EntryKind::Directory {
                continue;
            }
            let path = self.final_path(entry);
            match seen.get(entry).and_then(|seen| seen.there) {
                Some(attrs) if attrs.kind() == Kind::Directory => {}
                Some(_) => {
                    return Err(Failure::Fatal(format!(
                        "{} is there and is not a folder",
                        self.named(&path)
                    )));
                }
                // In the order of the tree, which has each folder after
                // the one that holds it.
                None => made.push((self.0.session.ask(&Request::Mkdir(path.as_bytes())), path)),
            }
        }
        let mut all = Ok(());
        for (asked, path) in made {
            let done = asked
                .wait()
                .and_then(|reply| done(&reply, "make", &self.named(&path)));
            all = all.and(done);
        }
        all
    }

    /// Puts the links of the tree in place, each by a rename, and counts
    /// those that are left out.
    fn place_links(&self) -> Result<(), Failure> {
        let entries = self.0.tree.entries();
        let links =
            entries
                .iter()
                .enumerate()
                .filter_map(|(entry, described)| match &described.kind {
                    EntryKind::Symlink { target } => Some((entry, target)),
                    _ => None,
                });
        // Only OpenSSH's server is known to read the two paths of a link
        // in the order they are sent in here, and it is the one that has
        // the rename that replaces.
        if !self.0.session.greeting().replaces {
            self.0.skipped_links.store(links.count(), Ordering::Relaxed);
            return Ok(());
        }
        let mut asked = Vec::new();
        for (entry, target) in links {
            let (path, part) = (self.final_path(entry), self.part_path(entry));
            let session = &self.0.session;
            let there = session.ask(&Request::Readlink(path.as_bytes()));
            asked.push((entry, target, path, part, there));
        }
        let mut made = Vec::new();
        for (entry, target, path, part, there) in asked {
            let same = matches!(
                there.wait()?,
                Reply::Name(names) if names.len() == 1 && names[0].name == target.as_bytes()
            );
            if !same {
                let session = &self.0.session;
                session.send(&Request::Remove(part.as_bytes()));
                let linked = session.ask(&Request::Symlink {
                    target: target.as_bytes(),
                    link: part.as_bytes(),
                });
                let renamed = session.ask(&Request::Replace {
                    from: part.as_bytes(),
                    to: path.as_bytes(),
                });
                made.push((path, linked, renamed));
            }
            Self::set(&self.0.in_place, entry, true);
        }
        let mut all = Ok(());
        for (path, linked, renamed) in made {
            let place = self.named(&path);
            let linked = linked.wait().and_then(|reply| done(&reply, "link", &place));
            let renamed = renamed
                .wait()
                .and_then(|reply| done(&reply, "name", &place));
            all = all.and(linked).and(renamed);
        }
        all
    }

    /// See [`Sink::begin`].
    fn prepare(
        &self,
        expected: &Expected<'_>,
        examined: &(dyn Fn(u64) + Sync),
    ) -> Result<Begun, Failure> {
        let tree = &self.0.tree;
        let seen = self.seen()?;
        self.make_room()?;
        self.make_directories(&seen)?;

        let mut held = Bitfield::new(tree.chunk_count());
        for (file, entry) in tree.entries().iter().enumerate() {
            let EntryKind::File { size } = entry.kind else {
                // A directory is there now, and a link is made afterwards.
                Self::set(&self.0.in_place, file, entry.kind == EntryKind::Directory);
                continue;
            };
            if self.0.kept[file].load(Ordering::Acquire) {
                continue;
            }
            Self::set(&self.0.in_place, file, false);
            let chunks = tree.file_chunks(file);
            let whole = |right: &[u64]| right.len() as u64 == chunks.end - chunks.start;
            let there = seen.get(file).and_then(|seen| seen.there);
            let there = there.filter(|a| a.kind() == Kind::File && a.size == Some(size));
            // A file with no bytes that is there has nothing to be checked.
            if chunks.is_empty() && there.is_some() {
                self.discard(file)?;
                Self::set(&self.0.in_place, file, true);
                continue;
            }
            let vouched = chunks.clone().any(|index| expected(index).is_some());
            if !vouched {
                continue;
            }
            // Under its final name, whole and right: it stays as it is.
            if let Some(attrs) = there {
                let path = self.final_path(file);
                let place = self.named(&path);
                if let Reply::Handle(handle) = self.ask(&Request::Open(path.as_bytes()))? {
                    let open = Handle {
                        session: self.0.session.clone(),
                        handle,
                    };
                    let len = attrs.size.unwrap_or(0);
                    let right = self.verify(file, &open, len, expected, examined, &place)?;
                    if whole(&right) {
                        for &index in &right {
                            held.set(index);
                        }
                        self.discard(file)?;
                        Self::set(&self.0.in_place, file, true);
                        continue;
                    }
                }
            }
            // What an earlier try left under the part name keeps each
            // chunk that is right.
            if self.is_parted(file) {
                let place = self.named(&self.part_path(file));
                let open = self.part(file)?;
                let len = match self.ask(&Request::Fstat(&open.handle))? {
                    Reply::Attrs(attrs) => attrs.size.unwrap_or(0),
                    other => return Err(refused(&other, "look at", &place)),
                };
                let right = self.verify(file, &open, len, expected, examined, &place)?;
                for &index in &right {
                    held.set(index);
                }
            }
        }

        // A part file with every chunk right stopped just short of its
        // name, and a file with no chunks has nothing to wait for.
        let assembly = Assembly::new(tree.layout().clone(), held.clone());
        let mut named = Vec::new();
        for (file, entry) in tree.entries().iter().enumerate() {
            if entry.is_file() && !self.is_in_place(file) && assembly.is_whole(file) {
                self.name(file)?;
                named.push(file);
            }
        }
        self.place_links()?;
        // What was seen is no longer what is there.
        *lock(&self.0.seen) = None;
        Ok(Begun {
            held,
            named,
            older: false,
        })
    }

    /// See [`Sink::notes`].
    fn notes_of(&self, file: usize, limit: usize) -> Result<Vec<u8>, Failure> {
        let size = self.size_of(file)?;
        self.look()?;
        if !self.is_parted(file) {
            return Ok(Vec::new());
        }
        let place = self.named(&self.part_path(file));
        let open = self.part(file)?;
        let len = match self.ask(&Request::Fstat(&open.handle))? {
            Reply::Attrs(attrs) => attrs.size.unwrap_or(0),
            other => return Err(refused(&other, "look at", &place)),
        };
        let after = usize::try_from(len.saturating_sub(size)).unwrap_or(usize::MAX);
        let wanted = u32::try_from(after.min(limit)).unwrap_or(u32::MAX);
        if wanted == 0 {
            return Ok(Vec::new());
        }
        self.read_from(&open, size, wanted, &place)
    }

    /// See [`Sink::is_current`].
    fn looks_current(&self, file: usize) -> Result<bool, Failure> {
        let size = self.size_of(file)?;
        let recorded = self.0.tree.metadata(file).and_then(|m| m.modified);
        let seen = self.seen()?;
        let there = seen.get(file).and_then(|seen| seen.there);
        let (Some(recorded), Some(there)) = (recorded, there) else {
            return Ok(false);
        };
        // The other machine keeps times to the second.
        let same_time = there.modified.map(i64::from) == Some(recorded.seconds);
        Ok(there.kind() == Kind::File && there.size == Some(size) && same_time)
    }

    /// See [`Sink::finish`].
    fn seal(&self) -> Result<(), Failure> {
        let entries = self.0.tree.entries();
        if !(0..entries.len()).all(|entry| self.is_in_place(entry)) {
            return Ok(());
        }
        // Children before parents: filling a directory changes its time.
        let mut asked = Vec::new();
        for (entry, described) in entries.iter().enumerate().rev() {
            let recorded = self.0.tree.metadata(entry);
            if let (EntryKind::Directory, Some(recorded)) = (&described.kind, recorded) {
                let path = self.final_path(entry);
                let modified = recorded.modified.map(|time| time.seconds);
                let attrs = Attrs {
                    size: None,
                    mode: Some(u32::from(recorded.mode)),
                    modified: modified.and_then(|seconds| u32::try_from(seconds).ok()),
                };
                let set = self.0.session.ask(&Request::Setstat {
                    path: path.as_bytes(),
                    attrs,
                });
                asked.push((set, path));
            }
        }
        let mut all = Ok(());
        for (set, path) in asked {
            let done = set
                .wait()
                .and_then(|reply| done(&reply, "finish", &self.named(&path)));
            all = all.and(done);
        }
        all
    }
}

/// Whether an answer says the request was carried out.
fn done(reply: &Reply, doing: &str, place: &str) -> Result<(), Failure> {
    match reply {
        Reply::Status { code, .. } if *code == status::OK => Ok(()),
        other => Err(refused(other, doing, place)),
    }
}

/// What an answer that is not what was asked for means.
fn refused(reply: &Reply, doing: &str, place: &str) -> Failure {
    match reply {
        Reply::Status { code, .. } if *code == status::NO_SUCH_FILE => {
            Failure::Absent(format!("cannot {doing} {place}: nothing is there"))
        }
        Reply::Status { code, .. } if *code == status::PERMISSION_DENIED => {
            Failure::Fatal(format!("cannot {doing} {place}: permission was denied"))
        }
        Reply::Status { message, .. } if !message.is_empty() => {
            Failure::Fatal(format!("cannot {doing} {place}: {message}"))
        }
        Reply::Status { code, .. } => Failure::Fatal(format!(
            "cannot {doing} {place}: the other machine answered {code}"
        )),
        _ => Failure::Corrupt(format!(
            "the other machine answered a question about {place} with something else"
        )),
    }
}

/// Reads on a thread that may wait on the other machine.
async fn waiting<T: Send + 'static>(
    read: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    match tokio::task::spawn_blocking(read).await {
        Ok(read) => read,
        Err(stopped) => Err(Failure::Fatal(format!(
            "reading from the other machine was cut short: {stopped}"
        ))),
    }
}

impl Source for RemoteFolder {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            // Every request is a packet of the one connection.
            multiplexed: true,
        }
    }

    fn describe(&self) -> Pending<'_, Description> {
        Box::pin(std::future::ready(Ok(Description::of(&self.0.tree))))
    }

    fn open(&self, file: usize, offset: u64, _len: u64) -> Pending<'_, Box<dyn Reader>> {
        let reader: Box<dyn Reader> = Box::new(InOrder {
            folder: self.clone(),
            file,
            offset,
        });
        Box::pin(std::future::ready(Ok(reader)))
    }

    fn read(&self, file: usize, offset: u64, len: u32) -> Pending<'_, Vec<u8>> {
        let folder = self.clone();
        Box::pin(waiting(move || folder.read_at(file, offset, len)))
    }
}

/// A file of the folder that is read in order.
#[derive(Debug)]
struct InOrder {
    folder: RemoteFolder,
    file: usize,
    offset: u64,
}

impl Reader for InOrder {
    fn read<'a>(&'a mut self, len: u32, arrived: &'a Arrived) -> Pending<'a, Vec<u8>> {
        let (folder, file, offset) = (self.folder.clone(), self.file, self.offset);
        Box::pin(async move {
            let data = waiting(move || folder.read_at(file, offset, len)).await?;
            self.offset += u64::from(len);
            arrived.add(data.len());
            Ok(data)
        })
    }
}

impl Sink for RemoteFolder {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_access: true,
            multiplexed: true,
        }
    }

    fn begin(
        &self,
        expected: &Expected<'_>,
        examined: &(dyn Fn(u64) + Sync),
    ) -> Result<Begun, Failure> {
        self.prepare(expected, examined)
    }

    fn write(&self, file: usize, offset: u64, data: &[u8]) -> Result<(), Failure> {
        let place = self.named(&self.part_path(file));
        self.write_to(&*self.part(file)?, offset, data, &place)
    }

    fn read_back(&self, file: usize, offset: u64, len: u32) -> Result<Vec<u8>, Failure> {
        let place = self.named(&self.part_path(file));
        self.read_from(&*self.part(file)?, offset, len, &place)
    }

    fn notes(&self, file: usize, limit: usize) -> Result<Vec<u8>, Failure> {
        self.notes_of(file, limit)
    }

    fn note(&self, file: usize, offset: u64, data: &[u8]) -> Result<(), Failure> {
        let place = self.named(&self.part_path(file));
        let start = self.size_of(file)?.checked_add(offset);
        let start = start.ok_or_else(|| Failure::Fatal("a note is out of range".into()))?;
        self.write_to(&*self.part(file)?, start, data, &place)
    }

    /// A file is named as it is committed. The protocol has no way to make
    /// many files last together, so there is nothing to hold one back for.
    fn commit(&self, file: usize) -> Result<Naming, Failure> {
        self.name(file)?;
        Ok(Naming::Named)
    }

    fn settle(&self) -> Result<Vec<usize>, Failure> {
        Ok(Vec::new())
    }

    fn abort(&self, file: usize) -> Result<(), Failure> {
        // Asked before the place is made ready: what is there is not known
        // until it is looked at.
        self.look()?;
        self.discard(file)
    }

    fn is_current(&self, file: usize) -> Result<bool, Failure> {
        self.looks_current(file)
    }

    fn keep(&self, file: usize) -> Result<(), Failure> {
        self.look()?;
        self.discard(file)?;
        Self::set(&self.0.kept, file, true);
        Self::set(&self.0.in_place, file, true);
        Ok(())
    }

    fn finish(&self) -> Result<(), Failure> {
        self.seal()
    }
}
