//! Small built-in word lists for `%radjective`, `%ranimal` and `%remoji`.
//!
//! ShareX ships larger lists; these are deliberately short, ASCII (except emoji) and free of
//! anything that could embarrass a user when it lands in a public URL.

pub(super) const ADJECTIVES: &[&str] = &[
    "amber", "ancient", "bold", "brave", "bright", "calm", "clever", "cosmic", "crisp", "curious",
    "dapper", "eager", "electric", "fancy", "fierce", "gentle", "golden", "happy", "humble", "icy",
    "jolly", "keen", "lively", "lucky", "mellow", "mighty", "nimble", "noble", "odd", "plucky",
    "proud", "quick", "quiet", "rapid", "royal", "shiny", "silent", "sleepy", "smooth", "snowy",
    "spicy", "sunny", "swift", "tidy", "vivid", "wild", "witty", "zesty",
];

pub(super) const ANIMALS: &[&str] = &[
    "badger", "beaver", "bison", "camel", "cat", "cheetah", "cobra", "condor", "coyote", "crane",
    "dolphin", "eagle", "falcon", "ferret", "fox", "gecko", "gopher", "heron", "hedgehog", "ibis",
    "jaguar", "koala", "lemur", "lynx", "llama", "marten", "narwhal", "newt", "ocelot", "otter",
    "owl", "panda", "penguin", "puffin", "quokka", "raven", "seal", "sparrow", "tiger", "turtle",
    "walrus", "weasel", "wombat", "yak", "zebra",
];

pub(super) const EMOJI: &[&str] = &[
    "😀", "😎", "🤖", "👻", "🐱", "🐶", "🦊", "🐼", "🐸", "🦄", "🌈", "🌟", "🔥", "🍕", "🍩", "🍉",
    "🚀", "🎈", "🎸", "🏆", "💎", "🌵", "🍀", "⚡",
];
