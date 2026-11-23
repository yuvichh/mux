//! `mux get`: fetch what four words, a ticket or an address names, into
//! the current folder or the one that is named. It is `mux cp` with its
//! places the other way round and the folder left out.

use super::cp::{self, Get};
use super::{Reach, Sharing};
use anyhow::Context;
use mux::Location;
use mux::swarm::{Code, TicketPeer};
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

#[derive(clap::Args)]
pub(super) struct Args {
    /// What to fetch: the four words that `mux send` gave, a ticket, a web
    /// address, or `host:path` on a machine reached by SSH
    #[arg(value_name = "WHAT")]
    what: String,

    /// The folder to put it in; what is there already is kept if it is
    /// right [default: the current folder]
    #[arg(value_name = "FOLDER", conflicts_with = "out")]
    folder: Option<PathBuf>,

    /// The folder to put it in, as a flag
    #[arg(short, long, value_name = "DIR", hide = true)]
    out: Option<PathBuf>,

    /// Go on offering what was fetched to other machines: in its swarm if
    /// a ticket is a swarm's, and under a ticket of its own if not
    #[arg(long)]
    seed: bool,

    #[command(flatten)]
    sharing: Sharing,

    /// Additional peer to try for a ticket, by its node id and, if known,
    /// where it is (repeatable)
    #[arg(long = "peer", value_name = "NODE[@HOST:PORT]")]
    peers: Vec<TicketPeer>,

    /// Where the machine that gave the four words is, if no relay is used
    /// to find it (repeatable)
    #[arg(long, value_name = "HOST:PORT")]
    at: Vec<String>,

    #[command(flatten)]
    reach: Reach,

    /// Peers to download from at once
    #[arg(long, default_value_t = 8, value_name = "N")]
    max_peers: usize,

    /// Give up after this long without receiving any data
    #[arg(long, default_value_t = 60, value_name = "SECONDS")]
    stall_timeout: u64,
}

/// The four words in `text`, if that is what it is. A place is never
/// taken for them: nothing with a `:` or a slash in it is a code.
fn code(text: &str) -> Option<Code> {
    if text.contains([':', '/', '\\']) {
        return None;
    }
    text.parse().ok()
}

pub(super) async fn run(args: Args, json: bool, key: Option<&Path>) -> anyhow::Result<()> {
    let sources = match code(&args.what) {
        // The words stand for a ticket for each thing that is offered.
        Some(code) => {
            let node = super::node(key, &args.reach).await?;
            let found = mux::swarm::tickets(&node, code, &args.at).await;
            node.shutdown().await;
            let tickets = found.context("cannot fetch with those words")?;
            tickets.iter().map(ToString::to_string).collect()
        }
        None => match mux::locate(&args.what)? {
            // A peer named beside a ticket is one more peer of the ticket.
            Location::Ticket(mut ticket) => {
                ticket.peers.extend(args.peers);
                vec![ticket.to_string()]
            }
            _ => vec![args.what],
        },
    };
    let folder = args
        .folder
        .or(args.out)
        .unwrap_or_else(|| PathBuf::from("."));
    // Written as a folder, so that one that is not there yet is made.
    let folder = format!("{}{MAIN_SEPARATOR}", folder.display());
    let told = Get {
        seed: args.seed,
        max_peers: args.max_peers,
        stall_timeout: args.stall_timeout,
        reach: args.reach,
        sharing: args.sharing,
    };
    cp::run(cp::Args::get(sources, folder, told), json, key).await
}

#[cfg(test)]
mod tests {
    use super::code;

    #[test]
    fn four_words_are_a_code_and_a_place_is_not() {
        assert!(code("harbor-velvet-piano-sunset").is_some());
        assert!(code("Harbor Velvet Piano Sunset").is_some());
        assert!(code("harb-velv-pian-suns").is_some());
        // Four words of the list, and still a place: on another machine,
        // a path, a web address.
        assert!(code("open:source/code/base").is_none());
        assert!(code("open/source/code/base").is_none());
        assert!(code("./open-source-code-base").is_none());
        assert!(code("https://open.source.code/base").is_none());
        // Not four words of the list.
        assert!(code("harbor-velvet-piano").is_none());
        assert!(code("notes.txt").is_none());
    }
}
