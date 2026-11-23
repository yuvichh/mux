//! Packets of SFTP arrive from another machine's server. Reading one must
//! not panic, whatever it holds, and what a packet says it holds is never
//! more than the bytes it has.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssh::wire::{Reply, decode, greeting};

fuzz_target!(|data: &[u8]| {
    // The first packet of a server is read by itself, and names the
    // requests of its own that it has: as many as it likes.
    let _ = greeting(data);
    let Ok((_, reply)) = decode(data) else {
        return;
    };
    // Whatever was read was in the packet.
    let held = match &reply {
        Reply::Handle(bytes) | Reply::Data(bytes) => bytes.len(),
        Reply::Name(names) => names.iter().map(|named| named.name.len()).sum(),
        Reply::Status { message, .. } => message.len() / 3,
        Reply::Version(_) | Reply::Attrs(_) => 0,
    };
    assert!(held <= data.len());
});
