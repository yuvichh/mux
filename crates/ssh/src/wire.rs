//! The packets of SFTP, version 3: what this side asks, and what the other
//! machine answers.
//!
//! Nothing here touches a pipe. A packet is written into bytes and read
//! from bytes that are already there, and what the other machine sent is
//! never taken to be well formed.

/// The version of the protocol that is spoken.
pub const VERSION: u32 = 3;

/// The most bytes asked for in one read. Every server gives this many in
/// one answer, and some give no more.
pub const READ_SIZE: u32 = 32 * 1024;

/// The largest packet taken from the other machine. OpenSSH sends none
/// over 256 KiB.
pub const MAX_PACKET: usize = 512 * 1024;

/// What an answer's status says, by its number.
pub mod status {
    /// The request was carried out.
    pub const OK: u32 = 0;
    /// There is no more: of a file, or of a directory's names.
    pub const EOF: u32 = 1;
    /// Nothing is at that path.
    pub const NO_SUCH_FILE: u32 = 2;
    /// The user may not do that.
    pub const PERMISSION_DENIED: u32 = 3;
    /// The request failed, for a reason the message gives.
    pub const FAILURE: u32 = 4;
}

/// The bits of a mode that say what kind of thing it is of.
const KIND: u32 = 0o170_000;
const DIRECTORY: u32 = 0o040_000;
const FILE: u32 = 0o100_000;
const LINK: u32 = 0o120_000;

pub(crate) mod kind {
    pub(crate) const INIT: u8 = 1;
    pub(crate) const VERSION: u8 = 2;
    pub(crate) const OPEN: u8 = 3;
    pub(crate) const CLOSE: u8 = 4;
    pub(crate) const READ: u8 = 5;
    pub(crate) const LSTAT: u8 = 7;
    pub(crate) const FSTAT: u8 = 8;
    pub(crate) const OPENDIR: u8 = 11;
    pub(crate) const READDIR: u8 = 12;
    pub(crate) const REALPATH: u8 = 16;
    pub(crate) const STAT: u8 = 17;
    pub(crate) const READLINK: u8 = 19;
    pub(crate) const STATUS: u8 = 101;
    pub(crate) const HANDLE: u8 = 102;
    pub(crate) const DATA: u8 = 103;
    pub(crate) const NAME: u8 = 104;
    pub(crate) const ATTRS: u8 = 105;
}

/// Which of a set of attributes a packet carries.
mod flag {
    pub(crate) const SIZE: u32 = 0x1;
    pub(crate) const OWNER: u32 = 0x2;
    pub(crate) const PERMISSIONS: u32 = 0x4;
    pub(crate) const TIMES: u32 = 0x8;
    pub(crate) const EXTENDED: u32 = 0x8000_0000;
}

/// Open a file to read it.
const OPEN_TO_READ: u32 = 0x1;

/// Something asked of the other machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// The first packet: which version this side speaks.
    Init,
    /// Open the file at a path, to read it.
    Open(&'a [u8]),
    /// Let go of a file or a directory that was opened.
    Close(&'a [u8]),
    /// Read bytes of an open file.
    Read {
        /// The file, as [`Reply::Handle`] named it.
        handle: &'a [u8],
        /// Where in the file the bytes begin.
        offset: u64,
        /// How many bytes, at most.
        len: u32,
    },
    /// Describe what is at a path. A link is described and not followed.
    Lstat(&'a [u8]),
    /// Describe what is at a path, following links.
    Stat(&'a [u8]),
    /// Describe an open file.
    Fstat(&'a [u8]),
    /// Open the directory at a path, to list it.
    Opendir(&'a [u8]),
    /// List the next names of an open directory.
    Readdir(&'a [u8]),
    /// Give the path as the other machine has it, from its root.
    Realpath(&'a [u8]),
    /// Say where the link at a path points.
    Readlink(&'a [u8]),
}

impl Request<'_> {
    /// The packet for this request, with its length before it. `id` comes
    /// back with the answer. [`Request::Init`] carries none.
    pub fn encode(&self, id: u32) -> Vec<u8> {
        let mut packet = vec![0; 4];
        let (kind, text) = match *self {
            Self::Init => {
                packet.push(kind::INIT);
                packet.extend(VERSION.to_be_bytes());
                return framed(packet);
            }
            Self::Open(path) => (kind::OPEN, path),
            Self::Close(handle) => (kind::CLOSE, handle),
            Self::Read { handle, .. } => (kind::READ, handle),
            Self::Lstat(path) => (kind::LSTAT, path),
            Self::Stat(path) => (kind::STAT, path),
            Self::Fstat(handle) => (kind::FSTAT, handle),
            Self::Opendir(path) => (kind::OPENDIR, path),
            Self::Readdir(handle) => (kind::READDIR, handle),
            Self::Realpath(path) => (kind::REALPATH, path),
            Self::Readlink(path) => (kind::READLINK, path),
        };
        packet.push(kind);
        packet.extend(id.to_be_bytes());
        put_bytes(&mut packet, text);
        match *self {
            Self::Open(_) => {
                packet.extend(OPEN_TO_READ.to_be_bytes());
                // No attributes: nothing is made.
                packet.extend(0u32.to_be_bytes());
            }
            Self::Read { offset, len, .. } => {
                packet.extend(offset.to_be_bytes());
                packet.extend(len.to_be_bytes());
            }
            _ => {}
        }
        framed(packet)
    }
}

/// Writes the length of a packet into the four bytes kept for it.
fn framed(mut packet: Vec<u8>) -> Vec<u8> {
    let len = u32::try_from(packet.len() - 4).unwrap_or(u32::MAX);
    packet[..4].copy_from_slice(&len.to_be_bytes());
    packet
}

fn put_bytes(packet: &mut Vec<u8>, bytes: &[u8]) {
    packet.extend(u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    packet.extend(bytes);
}

/// What is recorded about a file, a directory or a link. A server leaves
/// out what it does not know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
    /// How many bytes a file holds.
    pub size: Option<u64>,
    /// What kind of thing it is, and its permission bits.
    pub mode: Option<u32>,
    /// When it was last changed, in seconds since 1970.
    pub modified: Option<u32>,
}

/// What kind of thing a set of attributes is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Directory,
    /// A file with bytes in it.
    File,
    /// A link to another path.
    Link,
    /// Anything else: a device, a socket, a pipe, or a thing whose kind
    /// the server did not say.
    Other,
}

impl Attrs {
    /// The kind of thing these are the attributes of.
    pub fn kind(&self) -> Kind {
        match self.mode.map(|mode| mode & KIND) {
            Some(DIRECTORY) => Kind::Directory,
            Some(FILE) => Kind::File,
            Some(LINK) => Kind::Link,
            _ => Kind::Other,
        }
    }
}

/// One name of a directory, with what is recorded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The name, as the other machine has it: bytes, which need not be
    /// text.
    pub name: Vec<u8>,
    /// What is recorded about it.
    pub attrs: Attrs,
}

/// An answer from the other machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The answer to [`Request::Init`]: the version the server speaks.
    Version(u32),
    /// How a request ended, when it has nothing else to give.
    Status {
        /// One of the numbers of [`status`].
        code: u32,
        /// What the server says of it, for a person.
        message: String,
    },
    /// A name for a file or a directory that was opened.
    Handle(Vec<u8>),
    /// Bytes of a file.
    Data(Vec<u8>),
    /// Names: of a directory, or the one name a path was resolved to.
    Name(Vec<Named>),
    /// What is recorded about a path or an open file.
    Attrs(Attrs),
}

/// A packet that is not one of this protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

/// Reads a packet from the other machine: everything after its length.
/// Gives the request it answers, which [`Reply::Version`] has none of.
///
/// # Errors
///
/// Returns [`Malformed`] if the bytes are cut short, say they hold more
/// than they do, or are of a kind this side never asked for.
pub fn decode(packet: &[u8]) -> Result<(Option<u32>, Reply), Malformed> {
    let mut rest = Bytes(packet);
    let kind = rest.byte()?;
    if kind == kind::VERSION {
        // Extensions may follow. None is used.
        return Ok((None, Reply::Version(rest.u32()?)));
    }
    let id = rest.u32()?;
    let reply = match kind {
        kind::STATUS => {
            let code = rest.u32()?;
            // A message and a language follow, and an old server leaves
            // both out.
            let message = if rest.is_empty() {
                String::new()
            } else {
                String::from_utf8_lossy(rest.bytes()?).into_owned()
            };
            Reply::Status { code, message }
        }
        kind::HANDLE => Reply::Handle(rest.bytes()?.to_vec()),
        kind::DATA => Reply::Data(rest.bytes()?.to_vec()),
        kind::NAME => {
            let count = rest.u32()?;
            let mut names = Vec::new();
            for _ in 0..count {
                let name = rest.bytes()?.to_vec();
                // The line `ls -l` would print: not used.
                rest.bytes()?;
                let attrs = rest.attrs()?;
                names.push(Named { name, attrs });
            }
            Reply::Name(names)
        }
        kind::ATTRS => Reply::Attrs(rest.attrs()?),
        _ => return Err(Malformed("a packet of a kind that answers nothing")),
    };
    Ok((Some(id), reply))
}

/// Bytes that are read from their start.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        let (taken, rest) = self
            .0
            .split_at_checked(count)
            .ok_or(Malformed("a packet that ends before what it says it holds"))?;
        self.0 = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, Malformed> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        let mut number = [0; 4];
        number.copy_from_slice(self.take(4)?);
        Ok(u32::from_be_bytes(number))
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        let mut number = [0; 8];
        number.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(number))
    }

    /// A length, and that many bytes.
    fn bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let len = self.u32()?;
        self.take(usize::try_from(len).unwrap_or(usize::MAX))
    }

    fn attrs(&mut self) -> Result<Attrs, Malformed> {
        let flags = self.u32()?;
        let mut attrs = Attrs::default();
        if flags & flag::SIZE != 0 {
            attrs.size = Some(self.u64()?);
        }
        if flags & flag::OWNER != 0 {
            // The numbers of the user and the group: not used.
            self.take(8)?;
        }
        if flags & flag::PERMISSIONS != 0 {
            attrs.mode = Some(self.u32()?);
        }
        if flags & flag::TIMES != 0 {
            // When it was last read, then when it was last changed.
            self.take(4)?;
            attrs.modified = Some(self.u32()?);
        }
        if flags & flag::EXTENDED != 0 {
            for _ in 0..self.u32()? {
                self.bytes()?;
                self.bytes()?;
            }
        }
        Ok(attrs)
    }
}

/// The other half of the protocol: reading what this side asks, and
/// writing what a server answers. Only a server written for the tests
/// needs it.
#[cfg(test)]
pub(crate) mod served {
    use super::{Attrs, Bytes, Malformed, Named, Reply, flag, framed, kind, put_bytes};

    /// A request as a server reads it: its kind, the path or the handle it
    /// names, and for a read, where and how much.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Asked {
        pub(crate) kind: u8,
        pub(crate) id: u32,
        pub(crate) text: Vec<u8>,
        pub(crate) offset: u64,
        pub(crate) len: u32,
    }

    pub(crate) use super::kind::{
        CLOSE, FSTAT, INIT, LSTAT, OPEN, OPENDIR, READ, READDIR, READLINK, REALPATH, STAT,
    };

    /// Reads a request: everything after its length.
    pub(crate) fn asked(packet: &[u8]) -> Result<Asked, Malformed> {
        let mut rest = Bytes(packet);
        let kind = rest.byte()?;
        let id = rest.u32()?;
        if kind == INIT {
            // What is read as the id is the version.
            return Ok(Asked {
                kind,
                id,
                text: Vec::new(),
                offset: 0,
                len: 0,
            });
        }
        let text = rest.bytes()?.to_vec();
        let (offset, len) = match kind {
            READ => (rest.u64()?, rest.u32()?),
            _ => (0, 0),
        };
        Ok(Asked {
            kind,
            id,
            text,
            offset,
            len,
        })
    }

    /// The packet for an answer, with its length before it.
    pub(crate) fn answer(id: u32, reply: &Reply) -> Vec<u8> {
        let mut packet = vec![0; 4];
        let kind = match reply {
            Reply::Version(_) => kind::VERSION,
            Reply::Status { .. } => kind::STATUS,
            Reply::Handle(_) => kind::HANDLE,
            Reply::Data(_) => kind::DATA,
            Reply::Name(_) => kind::NAME,
            Reply::Attrs(_) => kind::ATTRS,
        };
        packet.push(kind);
        if let Reply::Version(version) = reply {
            packet.extend(version.to_be_bytes());
            return framed(packet);
        }
        packet.extend(id.to_be_bytes());
        match reply {
            Reply::Version(_) => {}
            Reply::Status { code, message } => {
                packet.extend(code.to_be_bytes());
                put_bytes(&mut packet, message.as_bytes());
                put_bytes(&mut packet, b"en");
            }
            Reply::Handle(bytes) | Reply::Data(bytes) => put_bytes(&mut packet, bytes),
            Reply::Name(names) => {
                packet.extend(u32::try_from(names.len()).unwrap_or(u32::MAX).to_be_bytes());
                for Named { name, attrs } in names {
                    put_bytes(&mut packet, name);
                    put_bytes(&mut packet, name);
                    put_attrs(&mut packet, attrs);
                }
            }
            Reply::Attrs(attrs) => put_attrs(&mut packet, attrs),
        }
        framed(packet)
    }

    fn put_attrs(packet: &mut Vec<u8>, attrs: &Attrs) {
        let has = |there: bool, bit: u32| if there { bit } else { 0 };
        let flags = has(attrs.size.is_some(), flag::SIZE)
            | has(attrs.mode.is_some(), flag::PERMISSIONS)
            | has(attrs.modified.is_some(), flag::TIMES);
        packet.extend(flags.to_be_bytes());
        if let Some(size) = attrs.size {
            packet.extend(size.to_be_bytes());
        }
        if let Some(mode) = attrs.mode {
            packet.extend(mode.to_be_bytes());
        }
        if let Some(modified) = attrs.modified {
            packet.extend(modified.to_be_bytes());
            packet.extend(modified.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::served::{answer, asked};
    use super::*;
    use proptest::prelude::*;

    fn attrs() -> impl Strategy<Value = Attrs> {
        (
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u32>()),
            proptest::option::of(any::<u32>()),
        )
            .prop_map(|(size, mode, modified)| Attrs {
                size,
                mode,
                modified,
            })
    }

    fn reply() -> impl Strategy<Value = Reply> {
        let bytes = || proptest::collection::vec(any::<u8>(), 0..40);
        let named = (bytes(), attrs()).prop_map(|(name, attrs)| Named { name, attrs });
        prop_oneof![
            any::<u32>().prop_map(Reply::Version),
            (any::<u32>(), "[ -~]{0,20}")
                .prop_map(|(code, message)| Reply::Status { code, message }),
            bytes().prop_map(Reply::Handle),
            bytes().prop_map(Reply::Data),
            proptest::collection::vec(named, 0..5).prop_map(Reply::Name),
            attrs().prop_map(Reply::Attrs),
        ]
    }

    proptest! {
        /// What a server writes is what this side reads.
        #[test]
        fn an_answer_is_read_as_it_was_written(id in any::<u32>(), reply in reply()) {
            let packet = answer(id, &reply);
            let len = u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize;
            prop_assert_eq!(len, packet.len() - 4);
            let expected = if matches!(reply, Reply::Version(_)) { None } else { Some(id) };
            prop_assert_eq!(decode(&packet[4..]), Ok((expected, reply)));
        }

        /// Whatever bytes arrive, they are read as an answer or refused,
        /// and one that is cut short is refused.
        #[test]
        fn bytes_from_anywhere_are_an_answer_or_refused(
            bytes in proptest::collection::vec(any::<u8>(), 0..200),
            id in any::<u32>(),
            reply in reply(),
            cut in any::<prop::sample::Index>(),
        ) {
            let _ = decode(&bytes);
            let whole = answer(id, &reply);
            let body = &whole[4..];
            let short = &body[..cut.index(body.len())];
            // A version may be followed by more, so less of it can still
            // be one. Nothing else can lose its end and stay whole.
            if !matches!(reply, Reply::Version(_) | Reply::Status { .. }) {
                prop_assert!(decode(short).is_err(), "{:?}", short);
            }
        }

        /// What this side writes is what a server reads.
        #[test]
        fn a_request_is_read_as_it_was_written(
            id in any::<u32>(),
            text in proptest::collection::vec(any::<u8>(), 0..40),
            offset in any::<u64>(),
            len in any::<u32>(),
        ) {
            let requests = [
                (Request::Open(&text), served::OPEN),
                (Request::Close(&text), served::CLOSE),
                (Request::Read { handle: &text, offset, len }, served::READ),
                (Request::Lstat(&text), served::LSTAT),
                (Request::Stat(&text), served::STAT),
                (Request::Fstat(&text), served::FSTAT),
                (Request::Opendir(&text), served::OPENDIR),
                (Request::Readdir(&text), served::READDIR),
                (Request::Realpath(&text), served::REALPATH),
                (Request::Readlink(&text), served::READLINK),
            ];
            for (request, kind) in requests {
                let packet = request.encode(id);
                let said = u32::from_be_bytes(packet[..4].try_into().unwrap()) as usize;
                prop_assert_eq!(said, packet.len() - 4);
                let read = asked(&packet[4..]).unwrap();
                prop_assert_eq!((read.kind, read.id, &read.text), (kind, id, &text));
                if kind == served::READ {
                    prop_assert_eq!((read.offset, read.len), (offset, len));
                }
            }
        }
    }

    #[test]
    fn the_first_packet_says_the_version() {
        assert_eq!(Request::Init.encode(9), [0, 0, 0, 5, 1, 0, 0, 0, 3]);
        assert_eq!(asked(&Request::Init.encode(9)[4..]).unwrap().id, VERSION);
    }

    /// The bytes of two packets, as OpenSSH's server writes them.
    #[test]
    fn packets_of_a_real_server_are_read() {
        // A status: request 7, "no such file", with a message and a language.
        let status = [
            101, 0, 0, 0, 7, 0, 0, 0, 2, 0, 0, 0, 12, 78, 111, 32, 115, 117, 99, 104, 32, 102, 105,
            108, 101, 0, 0, 0, 0,
        ];
        let read = decode(&status).unwrap();
        assert_eq!(
            read,
            (
                Some(7),
                Reply::Status {
                    code: status::NO_SUCH_FILE,
                    message: "No such file".into()
                }
            )
        );
        // Attributes with a size, an owner, permissions and times: the
        // owner and the time of reading are passed over.
        let mut attrs = vec![105, 0, 0, 0, 1, 0, 0, 0, 0x0f];
        attrs.extend(1234u64.to_be_bytes());
        attrs.extend([0, 0, 1, 245, 0, 0, 0, 20]);
        attrs.extend(0o100_644u32.to_be_bytes());
        attrs.extend(5u32.to_be_bytes());
        attrs.extend(1_700_000_000u32.to_be_bytes());
        let (id, reply) = decode(&attrs).unwrap();
        let expected = Attrs {
            size: Some(1234),
            mode: Some(0o100_644),
            modified: Some(1_700_000_000),
        };
        assert_eq!((id, &reply), (Some(1), &Reply::Attrs(expected)));
        assert_eq!(expected.kind(), Kind::File);
        // An old server's status, with no message.
        assert_eq!(
            decode(&[101, 0, 0, 0, 1, 0, 0, 0, 0]).unwrap().1,
            Reply::Status {
                code: 0,
                message: String::new()
            }
        );
    }

    #[test]
    fn a_mode_says_what_kind_of_thing_it_is_of() {
        let of = |mode| {
            Attrs {
                mode,
                ..Attrs::default()
            }
            .kind()
        };
        assert_eq!(of(Some(0o040_755)), Kind::Directory);
        assert_eq!(of(Some(0o100_600)), Kind::File);
        assert_eq!(of(Some(0o120_777)), Kind::Link);
        // A socket, a device, and a thing whose kind was not said.
        assert_eq!(of(Some(0o140_755)), Kind::Other);
        assert_eq!(of(Some(0o020_620)), Kind::Other);
        assert_eq!(of(None), Kind::Other);
    }

    #[test]
    fn a_packet_that_says_more_than_it_holds_is_refused() {
        // A name packet that says it has a thousand million names.
        let many = [104, 0, 0, 0, 1, 59, 154, 202, 0];
        assert!(decode(&many).is_err());
        // Data that says it is longer than the packet.
        assert!(decode(&[103, 0, 0, 0, 1, 0, 0, 1, 0, 1, 2, 3]).is_err());
        assert!(decode(&[]).is_err());
        assert!(decode(&[200, 0, 0, 0, 1]).is_err());
    }
}
