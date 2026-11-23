//! Four words in place of a ticket: the machine that offers listens as the
//! key the words give, and the other finds it by them.

mod common;

use common::{CHUNK, addr_of, direct, get, index, node, pattern};
use peer::Error;
use peer::net::Identity;
use peer::swarm::{Access, Code, Guests, Node, tickets};
use std::fs;
use std::time::Duration;

#[tokio::test]
async fn four_words_stand_for_an_offer_to_one_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("model.bin");
    let data = pattern(CHUNK * 6 + 5, 21);
    fs::write(&source, &data).unwrap();
    let second = tmp.path().join("notes.txt");
    fs::write(&second, b"a second thing").unwrap();

    // The machine that offers: a node of the key the words give, which
    // answers to the secret they give, once.
    let code: Code = "harbor-velvet-piano-sunset".parse().unwrap();
    let keys = code.keys();
    let sender = Node::bind(&Identity::from_secret(keys.node), &direct())
        .await
        .unwrap();
    let offer = |path: &std::path::Path| {
        let (artifact, root) = index(path);
        let access = Access {
            secret: keys.secret,
            swarm: false,
            guests: Guests::first(1),
        };
        sender.seed(artifact, root, access)
    };
    let (first_share, second_share) = (offer(&source), offer(&second));
    let at = [addr_of(&sender).to_string()];

    // The other machine has the words, and where to look.
    let receiver = node().await;
    let found = tickets(&receiver, code, &at).await.unwrap();
    let mut wanted = [first_share.artifact().id(), second_share.artifact().id()];
    wanted.sort_unstable();
    let ids: Vec<_> = found.iter().map(|ticket| ticket.artifact).collect();
    assert_eq!(ids, wanted);
    assert!(
        found
            .iter()
            .all(|ticket| ticket.secret == keys.secret && !ticket.swarm)
    );

    let out = tmp.path().join("out");
    for ticket in &found {
        get(&receiver, ticket, &out).await.unwrap();
    }
    assert_eq!(fs::read(out.join("model.bin")).unwrap(), data);
    assert_eq!(fs::read(out.join("notes.txt")).unwrap(), b"a second thing");

    // The machine that offered is told that each is whole, so it can stop.
    let told = async {
        first_share.fetched_by(1).await;
        second_share.fetched_by(1).await;
    };
    tokio::time::timeout(Duration::from_secs(10), told)
        .await
        .expect("the sender was not told that the copy is whole");

    // The code was for one machine. Another that has it is refused, and
    // the first can still ask.
    let late = tickets(&node().await, code, &at).await;
    assert!(matches!(late, Err(Error::Refused)), "{late:?}");
    assert_eq!(tickets(&receiver, code, &at).await.unwrap().len(), 2);
}

/// Other words give another key. Nothing answers to it, and the machine
/// that offers learns nothing of who asked.
#[tokio::test]
async fn other_words_find_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("model.bin");
    fs::write(&source, pattern(CHUNK, 22)).unwrap();
    let code: Code = "harbor-velvet-piano-sunset".parse().unwrap();
    let keys = code.keys();
    let sender = Node::bind(&Identity::from_secret(keys.node), &direct())
        .await
        .unwrap();
    let (artifact, root) = index(&source);
    let access = Access {
        secret: keys.secret,
        swarm: false,
        guests: Guests::first(1),
    };
    let share = sender.seed(artifact, root, access);
    let at = [addr_of(&sender).to_string()];

    let wrong: Code = "harbor-velvet-piano-sunny".parse().unwrap();
    let nothing = tickets(&node().await, wrong, &at).await;
    assert!(matches!(nothing, Err(Error::NothingOffered)), "{nothing:?}");
    // The place is still there for the machine that has the right words.
    assert_eq!(tickets(&node().await, code, &at).await.unwrap().len(), 1);
    assert_eq!(share.uploaded_bytes(), 0);
}

#[test]
fn a_new_code_is_four_words_and_no_two_are_the_same() {
    let (one, other) = (peer::net::random_code(), peer::net::random_code());
    assert_ne!(one, other);
    assert_eq!(one.to_string().split('-').count(), 4);
    assert_eq!(one.to_string().parse(), Ok(one));
}
