//! `mux send`: offer files to one other machine, under four words.

use super::exit::misuse;
use super::{Reach, Sharing, seed};
use anyhow::Context;
use mux::artifact::DEFAULT_CHUNK_SIZE;
use mux::net::{Identity, Relays};
use mux::swarm::{Access, Guests, Node, Share};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Files or folders to send
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Print a ticket in place of the four words. A ticket is long, and is
    /// for more than one machine: it goes with --to and --swarm, and the
    /// command then runs until it is stopped
    #[arg(long)]
    ticket: bool,

    #[command(flatten)]
    sharing: Sharing,

    #[command(flatten)]
    reach: Reach,
}

pub(super) async fn run(args: Args, key: Option<&Path>, json: bool) -> anyhow::Result<()> {
    if args.ticket {
        let offering = seed::Args::of(args.paths, args.reach, args.sharing);
        return seed::run(offering, key, json).await;
    }
    if args.sharing.is_said() {
        return misuse(
            "--to, --once and --swarm go with --ticket. Without it, `send` gives four words, \
             which are for the first machine that uses them.",
        );
    }

    // The words come first, and the key this machine listens as is worked
    // out from them: so it has a key of its own for this one offer, and
    // not the one it is known by.
    let code = mux::net::random_code();
    let keys = tokio::task::spawn_blocking(move || code.keys())
        .await
        .context("the code could not be worked out")?;
    let node = Node::bind(&Identity::from_secret(keys.node), &args.reach.options())
        .await
        .context("cannot open a socket to listen on")?;
    let words = code.to_string();
    let relayed = args.reach.relays() != Relays::Off;

    // Two things at once: this machine makes itself reachable, which can
    // take seconds, and what it sends is hashed. With a relay in use the
    // other machine finds this one by its key, so a person is shown the
    // words as soon as there is something to fetch, and has them typed on
    // the other machine no sooner than this one can be reached.
    let offering = async {
        let mut shares: Vec<Arc<Share>> = Vec::new();
        for path in &args.paths {
            let offered = seed::Offered {
                path,
                chunk_size: DEFAULT_CHUNK_SIZE,
                origins: &[],
            };
            // The first machine that shows it has the words, and no other.
            let access = Access {
                secret: keys.secret,
                swarm: false,
                guests: Guests::first(1),
            };
            shares.push(seed::index_and_seed(&node, &offered, access).await?);
        }
        if relayed && !json {
            say_how(&words, &format!("mux get {words}"));
        }
        anyhow::Ok(shares)
    };
    let (advertised, shares) = tokio::join!(seed::advertised(&node, &args.reach, &[]), offering);
    let shares = shares?;

    if json {
        for share in &shares {
            let ticket = node.ticket(share, &advertised).to_string();
            seed::say_offer(share, &ticket, Some(&words));
        }
    } else {
        if !relayed {
            // Where there is nothing to look a machine up in, the other
            // machine has to be told where this one is.
            let mut told = format!("mux get {words} --no-relay");
            for at in &advertised {
                told.push_str(" --at ");
                told.push_str(at);
            }
            say_how(&words, &told);
        }
        // For a program: the words alone, and only now that this machine
        // can be reached, so that one that reads them and fetches at once
        // finds it. A person has been shown them already.
        if !std::io::stdout().is_terminal() {
            println!("{words}");
        }
    }

    seed::serve(&shares, true, json).await;
    super::leave(&node).await;
    Ok(())
}

/// Tells a person the words, and what to run on the other machine.
fn say_how(words: &str, command: &str) {
    eprintln!("code     {words}");
    eprintln!("on the other machine, run:  {command}");
}
