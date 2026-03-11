#![no_main]
use libfuzzer_sys::fuzz_target;
use stormblock::target::iscsi::pdu::{parse_text_params, encode_text_params};

fuzz_target!(|data: &[u8]| {
    // Fuzz parse_text_params — should never panic on arbitrary input
    let params = parse_text_params(data);

    // If we got params, verify encode/decode roundtrip doesn't panic
    if !params.is_empty() {
        let refs: Vec<(&str, &str)> = params.iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let encoded = encode_text_params(&refs);
        let _ = parse_text_params(&encoded);
    }
});
