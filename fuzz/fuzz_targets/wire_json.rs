#![no_main]
//! The wire types are decoded from whatever a peer sends: decoding must never
//! panic, and anything decoded must re-encode.

use landrop::protocol::{AcceptInfo, Offer};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    if let Ok(offer) = serde_json::from_str::<Offer>(data) {
        let encoded = serde_json::to_string(&offer).expect("an Offer always encodes");
        let again: Offer = serde_json::from_str(&encoded).expect("our own encoding decodes");
        assert_eq!(again.filename, offer.filename);
        assert_eq!(again.size, offer.size);
    }
    let _ = serde_json::from_str::<AcceptInfo>(data);
    let _ = serde_json::from_str::<serde_json::Value>(data);
});
