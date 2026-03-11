#![no_main]
use libfuzzer_sys::fuzz_target;
use stormblock::target::nvmeof::pdu::{CommonHeader, PduType};

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let bytes: &[u8; 8] = data[..8].try_into().unwrap();
    let ch = CommonHeader::from_bytes(bytes);

    // Exercise all accessors
    let _ = PduType::from_byte(ch.pdu_type);
    let _ = ch.hdgst_enable();
    let _ = ch.ddgst_enable();
    let _ = format!("{:?}", ch);

    // Roundtrip
    let out = ch.to_bytes();
    let ch2 = CommonHeader::from_bytes(&out);
    assert_eq!(ch.pdu_type, ch2.pdu_type);
    assert_eq!(ch.flags, ch2.flags);
    assert_eq!(ch.hlen, ch2.hlen);
    assert_eq!(ch.pdo, ch2.pdo);
    assert_eq!(ch.plen, ch2.plen);
});
