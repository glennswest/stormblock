#![no_main]
use libfuzzer_sys::fuzz_target;
use stormblock::target::nvmeof::fabric::ConnectData;
use stormblock::target::nvmeof::pdu::NvmeSqe;

fuzz_target!(|data: &[u8]| {
    // Fuzz ConnectData parsing — needs 1024 bytes minimum
    let _ = ConnectData::from_bytes(data);

    // Also fuzz NvmeSqe if we have enough bytes
    if data.len() >= 64 {
        let bytes: &[u8; 64] = data[..64].try_into().unwrap();
        let sqe = NvmeSqe::from_bytes(bytes);
        let _ = sqe.opcode();
        let _ = sqe.fuse();
        let _ = sqe.cid();
        let _ = sqe.nsid();
        let _ = sqe.cdw10();
        let _ = sqe.cdw11();
        let _ = sqe.cdw12();
        let _ = sqe.cdw13();
        let _ = sqe.cdw14();
        let _ = sqe.cdw15();
    }
});
