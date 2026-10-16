#![no_main]
//! Address parsing must never panic, and anything it accepts must be a real,
//! displayable target (round trips through `Display`).

use landrop::peers::{parse_peer_addr, parse_peer_addr_with};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = parse_peer_addr(data, 8420);
    if let Ok(addr) = parse_peer_addr_with(data, 8420, true) {
        let text = addr.to_string();
        let again = parse_peer_addr_with(&text, 8420, true)
            .unwrap_or_else(|e| panic!("display form {text:?} must parse: {e}"));
        assert_eq!(again, addr);

        // Port 0 is never a valid target.
        assert_ne!(addr.port(), 0, "port 0 must be refused: {data:?}");
    }
});
