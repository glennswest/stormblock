#![no_main]
use libfuzzer_sys::fuzz_target;
use stormblock::target::iscsi::pdu;

fuzz_target!(|data: &[u8]| {
    // Fuzz the async read_pdu by wrapping data in a Cursor.
    // Try all combinations of digest flags.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    for header_digest in [false, true] {
        for data_digest in [false, true] {
            let mut cursor = std::io::Cursor::new(data.to_vec());
            let _ = rt.block_on(pdu::read_pdu(&mut cursor, header_digest, data_digest));
        }
    }
});
