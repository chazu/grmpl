//! Ent content keys use the workspace's pinned durable SHA-256.

pub use grmpl_core::hash::{sha256, Sha256Digest as ContentKey};

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &ContentKey) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn distinct_content_gives_distinct_keys() {
        assert_ne!(sha256(b"lamp"), sha256(b"lamq"));
        assert_ne!(sha256(b"a"), sha256(b"aa"));
        assert_ne!(sha256(b"ab\x00"), sha256(b"ab"));
    }
}
