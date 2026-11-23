//! Four words that stand for an offer.
//!
//! A ticket is some two hundred characters, and can only be pasted. A code
//! is what a person can read out, type from another screen, or keep in
//! mind for a minute: `harbor-velvet-piano-sunset`.
//!
//! The words are not looked up anywhere. As the words of a wallet are its
//! keys, the words of a code are the offer's address: both machines work
//! out the same key pair and the same secret from them. The machine that
//! offers listens as that key, and the other, having worked out the same
//! key, knows whom to ask and what to show.
//!
//! Whoever carries packets between the two sees the public half of the
//! key. Working the words out from it means trying codes, and each try is
//! made to cost about a million hashes ([`ROUNDS`]). With forty-four bits
//! in a code that is 2^64 hashes for all of them, against an offer that
//! lasts minutes and lets one machine in.

mod words;

use crate::access::Secret;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;
use words::WORDS;

/// How many words a code has.
pub const COUNT: usize = 4;

/// How many times the words are hashed before anything is made of them.
pub const ROUNDS: u32 = 1 << 20;

/// What each word stands for: eleven bits, since there are 2,048 words.
const BITS: u32 = 11;

/// A code: four words of the list, by their places in it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Code([u16; COUNT]);

impl Code {
    /// Makes a code of eight bytes, which must come from a source of
    /// randomness: eleven bits of them for each word. Every code is as
    /// likely as every other, since the words are a power of two in
    /// number.
    pub const fn from_random(bytes: [u8; 8]) -> Self {
        let mut bits = u64::from_le_bytes(bytes);
        let mut places = [0; COUNT];
        let mut word = 0;
        while word < COUNT {
            places[word] = (bits & ((1 << BITS) - 1)) as u16;
            bits >>= BITS;
            word += 1;
        }
        Self(places)
    }

    /// The words, in order.
    pub fn words(&self) -> [&'static str; COUNT] {
        self.0.map(|place| WORDS[usize::from(place)])
    }

    /// What both machines work out from the code. It takes about a
    /// million hashes, which is the point of it: see the module.
    pub fn keys(&self) -> Keys {
        let mut text = String::new();
        for (at, word) in self.words().into_iter().enumerate() {
            if at > 0 {
                text.push('-');
            }
            text.push_str(word);
        }
        let mut stretched = blake3::derive_key("mux 2026 code v1", text.as_bytes());
        let mut round = 0;
        while round < ROUNDS {
            stretched = *blake3::hash(&stretched).as_bytes();
            round += 1;
        }
        let secret = blake3::derive_key("mux 2026 code v1 access secret", &stretched);
        let mut half = [0; 16];
        half.copy_from_slice(&secret[..16]);
        Keys {
            node: blake3::derive_key("mux 2026 code v1 node key", &stretched),
            secret: Secret::from_bytes(half),
        }
    }
}

/// What a code stands for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    /// The private half of the key the offering machine listens as: an
    /// Ed25519 key, as a node's is.
    pub node: [u8; 32],
    /// What a machine shows it knows before it is answered.
    pub secret: Secret,
}

/// Says only that these are keys: a log line must not give them away.
impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys(..)")
    }
}

/// The words with a dash between them: how a code is shown and passed on.
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [first, second, third, fourth] = self.words();
        write!(f, "{first}-{second}-{third}-{fourth}")
    }
}

/// Says only that it is a code: whoever has the words can fetch.
impl fmt::Debug for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Code(..)")
    }
}

/// Why a text is not a code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCode {
    /// It does not have four words.
    #[error("a code is four words, and this has {0}")]
    Count(usize),
    /// One of its words is not in the list.
    #[error("{0:?} is not one of the words a code is made of")]
    Word(String),
}

impl FromStr for Code {
    type Err = InvalidCode;

    /// Reads a code as a person types it. Capital letters are read as
    /// small ones, anything that is not a letter separates two words, and
    /// the first four letters of a word stand for it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let typed = || {
            text.split(|c: char| !c.is_ascii_alphabetic())
                .filter(|word| !word.is_empty())
        };
        let count = typed().count();
        if count != COUNT {
            return Err(InvalidCode::Count(count));
        }
        let mut places = [0; COUNT];
        for (place, word) in places.iter_mut().zip(typed()) {
            *place = place_of(word).ok_or_else(|| InvalidCode::Word(word.into()))?;
        }
        Ok(Self(places))
    }
}

/// Where `typed` is in the list: the word itself, or the one word that
/// begins with it, if it is four letters or more.
fn place_of(typed: &str) -> Option<u16> {
    let typed = typed.to_ascii_lowercase();
    let at = WORDS.iter().position(|word| {
        *word == typed || (typed.len() >= 4 && word.starts_with(typed.as_str()))
    })?;
    u16::try_from(at).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;

    #[test]
    fn the_list_is_the_one_wallets_use() {
        assert_eq!(WORDS.len(), 1 << BITS);
        assert_eq!(
            (WORDS[0], WORDS[1], WORDS[2047]),
            ("abandon", "ability", "zoo")
        );
        // In the order of the alphabet, and known by their first four
        // letters.
        assert!(WORDS.windows(2).all(|pair| pair[0] < pair[1]));
        let starts = |word: &&str| word.chars().take(4).collect::<String>();
        assert!(
            WORDS
                .windows(2)
                .all(|pair| starts(&pair[0]) != starts(&pair[1]))
        );
        assert!(WORDS.iter().all(|word| (3..=8).contains(&word.len())));
        // The whole list, by its digest: the list of BIP-39 as it is
        // published, one word on each line.
        let mut published = String::new();
        for word in WORDS {
            published.push_str(word);
            published.push('\n');
        }
        assert_eq!(
            blake3::hash(published.as_bytes()).to_hex().as_str(),
            "43afe08c75f902534cfd2a694bef11db381f4804cd3ef76cd05a8ad0d7199122",
        );
    }

    #[test]
    fn a_code_is_four_words_with_dashes() {
        let code = Code::from_random([0; 8]);
        assert_eq!(code.to_string(), "abandon-abandon-abandon-abandon");
        assert_eq!(Code::from_random([0xff; 8]).to_string(), "zoo-zoo-zoo-zoo");
        // Eleven bits for each word, from the low end.
        let bits = 1 | (2 << 11) | (3 << 22) | (2047 << 33);
        let code = Code::from_random(u64::to_le_bytes(bits));
        assert_eq!(code.words(), ["ability", "able", "about", "zoo"]);
        // The twenty bits that are left over change nothing.
        assert_eq!(
            Code::from_random(u64::to_le_bytes(bits | (0xfffff << 44))),
            code
        );
        assert_eq!(format!("{code:?}"), "Code(..)");
    }

    #[test]
    fn a_code_is_read_as_a_person_types_it() {
        let code: Code = "harbor-velvet-piano-sunset".parse().unwrap();
        assert_eq!(code.to_string(), "harbor-velvet-piano-sunset");
        for typed in [
            "Harbor Velvet Piano Sunset",
            "  harbor  velvet\tpiano sunset ",
            "harbor.velvet.piano.sunset",
            "HARBOR-VELVET-PIANO-SUNSET",
            // The first four letters stand for a word.
            "harb-velv-pian-suns",
            "harbo-velve-pian-sunse",
            "1. harbor, 2. velvet, 3. piano, 4. sunset",
        ] {
            assert_eq!(typed.parse(), Ok(code), "{typed:?}");
        }
        // A word of three letters is itself and nothing shorter.
        assert_eq!(
            "zoo-act-add-age".parse::<Code>().unwrap().words(),
            ["zoo", "act", "add", "age"]
        );
    }

    #[test]
    fn what_is_not_a_code_says_why() {
        let read = |text: &str| text.parse::<Code>().unwrap_err();
        assert_eq!(read(""), InvalidCode::Count(0));
        assert_eq!(read("harbor-velvet-piano"), InvalidCode::Count(3));
        assert_eq!(
            read("harbor-velvet-piano-sunset-zoo"),
            InvalidCode::Count(5)
        );
        assert_eq!(
            read("harbor-velvet-piano-sunsets"),
            InvalidCode::Word("sunsets".into())
        );
        assert_eq!(
            read("harbor-velvet-piano-xyzzy"),
            InvalidCode::Word("xyzzy".into())
        );
        // Three letters are not enough to stand for a longer word.
        assert_eq!(
            read("har-velvet-piano-sunset"),
            InvalidCode::Word("har".into())
        );
        assert!(read("a-b-c-d").to_string().contains("not one of the words"));
    }

    /// What a code stands for is part of what two machines agree on, so it
    /// must not change from one version to the next.
    // Under Miri a million hashes would take hours.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_code_stands_for_a_key_and_a_secret_that_do_not_change() {
        let code: Code = "harbor-velvet-piano-sunset".parse().unwrap();
        let keys = code.keys();
        assert_eq!(format!("{keys:?}"), "Keys(..)");
        assert_eq!(
            crate::hex::encode(&keys.node),
            "c75b1d10c291e63ba22da0d7ffb3b31d936b5e56bdc09fa982e469252f43e7e4"
        );
        assert_eq!(keys.secret.to_hex(), "54cf1105726c6561fd6205b3e68df825");
        // One word apart, and nothing is the same.
        let other: Code = "harbor-velvet-piano-sunny".parse().unwrap();
        let others = other.keys();
        assert_ne!(others.node, keys.node);
        assert_ne!(others.secret, keys.secret);
    }
}

/// Bounded proof, run by `just kani`.
#[cfg(kani)]
mod proofs {
    use super::*;

    /// Whatever the eight bytes are, each of the four places is within the
    /// list, so looking a word up never fails, and each place is the
    /// eleven bits it should be: no code is likelier than another.
    #[kani::proof]
    #[kani::unwind(6)]
    fn every_eight_bytes_give_four_places_in_the_list() {
        let bytes: [u8; 8] = kani::any();
        let code = Code::from_random(bytes);
        let bits = u64::from_le_bytes(bytes);
        let mut word = 0;
        while word < COUNT {
            assert!(usize::from(code.0[word]) < WORDS.len());
            assert!(u64::from(code.0[word]) == (bits >> (11 * word)) & 0x7ff);
            word += 1;
        }
    }
}
