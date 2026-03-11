#![no_main]
use libfuzzer_sys::fuzz_target;
use stormblock::target::iscsi::pdu::Bhs;

fuzz_target!(|data: &[u8]| {
    if data.len() < 48 {
        return;
    }
    let bytes: &[u8; 48] = data[..48].try_into().unwrap();
    let bhs = Bhs::from_bytes(bytes);

    // Exercise all accessors — none should panic
    let _ = bhs.opcode();
    let _ = bhs.is_immediate();
    let _ = bhs.is_final();
    let _ = bhs.flags();
    let _ = bhs.total_ahs_length();
    let _ = bhs.data_segment_length();
    let _ = bhs.lun();
    let _ = bhs.initiator_task_tag();
    let _ = bhs.target_transfer_tag();
    let _ = bhs.cmd_sn();
    let _ = bhs.exp_stat_sn();
    let _ = bhs.max_cmd_sn();
    let _ = bhs.exp_cmd_sn();
    let _ = bhs.expected_data_transfer_length();
    let _ = bhs.cdb();
    let _ = bhs.isid();
    let _ = bhs.tsih();
    let _ = bhs.cid();
    let _ = bhs.csg();
    let _ = bhs.nsg();
    let _ = bhs.transit();
    let _ = bhs.cont();
    let _ = bhs.has_status();
    let _ = bhs.status();
    let _ = bhs.data_sn();
    let _ = bhs.buffer_offset();
    let _ = bhs.residual_count();
    let _ = bhs.r2t_sn();
    let _ = bhs.desired_data_transfer_length();
    let _ = bhs.reason_code();
    let _ = format!("{:?}", bhs);
});
