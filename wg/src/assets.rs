//! Prompts and pipelines compiled into the binary, so a run uses exactly what was reviewed.

use sha2::{Digest, Sha256};

pub const PREAMBLE: &str = include_str!("../../prompts/_preamble.md");
pub const DEV_PIPELINE: &str = include_str!("../../pipelines/dev.toml");

/// The prompt set: a template name the factory uses must be one of these.
pub const PROMPTS: &[(&str, &str)] = &[
    ("read", include_str!("../../prompts/read.md")),
    ("challenge", include_str!("../../prompts/challenge.md")),
    ("work", include_str!("../../prompts/work.md")),
    ("fix", include_str!("../../prompts/fix.md")),
    ("review", include_str!("../../prompts/review.md")),
    ("learn", include_str!("../../prompts/learn.md")),
];

pub fn prompt(name: &str) -> Option<&'static str> {
    PROMPTS.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// sha256 of every asset in use, recorded in the journal so outcomes map to exact versions.
pub fn hash() -> String {
    let mut h = Sha256::new();
    h.update(PREAMBLE);
    h.update(DEV_PIPELINE);
    for (n, t) in PROMPTS {
        h.update(n);
        h.update(t);
    }
    format!("{:x}", h.finalize())[..12].to_string()
}
