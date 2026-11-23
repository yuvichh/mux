//! Packets of SFTP arrive from another machine's server. Reading one must
//! not panic, whatever it holds, and what a packet says it holds is never
//! more than the bytes it has.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssh::wire::{Reply, decode};

fuzz_target!(|data: &[u8]| {
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
