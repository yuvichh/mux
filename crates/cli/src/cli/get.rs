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

/// `path` written as a folder, with a separator at its end, so that one
/// that is not there yet is made. One that is written so already is left
/// as it is.
fn as_a_folder(path: &Path) -> String {
    let mut written = path.display().to_string();
    if !written.ends_with(['/', MAIN_SEPARATOR]) {
        written.push(MAIN_SEPARATOR);
    }
    written
}

pub(super) async fn run(args: Args, json: bool, key: Option<&Path>) -> anyhow::Result<()> {
    let (sources, node) = match code(&args.what) {
        // The words stand for a ticket for each thing that is offered.
        // The node that asked goes on to fetch: it has found the other
        // machine, which is most of the time a small copy takes.
        Some(code) => {
            let node = super::node(key, &args.reach).await?;
            let tickets = match mux::swarm::tickets(&node, code, &args.at).await {
                Ok(tickets) => tickets,
                Err(failed) => {
                    super::leave(&node).await;
                    return Err(failed).context("cannot fetch with those words");
                }
            };
            let sources = tickets.iter().map(ToString::to_string).collect();
            (sources, Some(node))
        }
        None => match mux::locate(&args.what)? {
            // A peer named beside a ticket is one more peer of the ticket.
            Location::Ticket(mut ticket) => {
                ticket.peers.extend(args.peers);
                (vec![ticket.to_string()], None)
            }
            _ => (vec![args.what], None),
        },
    };
    let folder = args
        .folder
        .or(args.out)
        .unwrap_or_else(|| PathBuf::from("."));
    let folder = as_a_folder(&folder);
    let told = Get {
        seed: args.seed,
        max_peers: args.max_peers,
        stall_timeout: args.stall_timeout,
        reach: args.reach,
        sharing: args.sharing,
    };
    cp::run(cp::Args::get(sources, folder, told, node), json, key).await
}

#[cfg(test)]
mod tests {
    use super::{as_a_folder, code};
    use std::path::{MAIN_SEPARATOR, Path};

    #[test]
    fn a_folder_is_written_with_one_separator_at_its_end() {
        let ends = |text: &str| format!("{text}{MAIN_SEPARATOR}");
        assert_eq!(as_a_folder(Path::new("out")), ends("out"));
        assert_eq!(as_a_folder(Path::new(".")), ends("."));
        // Typed as a folder already: nothing is added.
        assert_eq!(as_a_folder(Path::new("out/")), "out/");
        assert_eq!(as_a_folder(Path::new(&ends("out"))), ends("out"));
    }

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
