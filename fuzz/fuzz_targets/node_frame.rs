//! A granfilade node frame read as a Fact tree node: decodes or is refused,
//! and what decodes frames to a fixed point of decoding.
#![no_main]

use std::sync::{Arc, OnceLock};

use grmpl_ent::{Durability, Granfilade};
use libfuzzer_sys::fuzz_target;

/// One throwaway store for the whole run: decoding pages nothing in, so the
/// store is only the pager its children hang from.
fn gran() -> &'static Arc<Granfilade> {
    static GRAN: OnceLock<(tempfile::TempDir, Arc<Granfilade>)> = OnceLock::new();
    &GRAN
        .get_or_init(|| {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let gran = Granfilade::open_with(dir.path(), Durability::Os).expect("a fresh granfilade");
            (dir, gran)
        })
        .1
}

fuzz_target!(|data: &[u8]| {
    if let Ok(once) = gran().reframe_for_fuzz(data) {
        assert_eq!(gran().reframe_for_fuzz(&once).expect("a reframed node decodes"), once);
    }
});
