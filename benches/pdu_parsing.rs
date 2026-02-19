use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use stormblock::target::iscsi::pdu::{
    Bhs, IscsiPdu, Opcode, encode_text_params, parse_text_params, read_pdu, write_pdu,
};
use stormblock::target::nvmeof::pdu::{
    CommonHeader, ICResp, NvmeCqe, NvmeSqe, PduType,
    write_capsule_resp, write_c2h_data, write_ic_resp,
};

fn bench_iscsi_bhs_roundtrip(c: &mut Criterion) {
    c.bench_function("iscsi_bhs_roundtrip", |b| {
        b.iter(|| {
            let mut bhs = Bhs::new();
            bhs.set_opcode(Opcode::ScsiCommand);
            bhs.set_immediate(true);
            bhs.set_final(true);
            bhs.set_data_segment_length(4096);
            bhs.set_initiator_task_tag(0xDEADBEEF);
            bhs.set_cmd_sn(42);
            bhs.set_lun(0);
            bhs.set_expected_data_transfer_length(4096);

            let bhs2 = Bhs::from_bytes(&bhs.raw);
            let _ = bhs2.opcode();
            let _ = bhs2.data_segment_length();
            let _ = bhs2.initiator_task_tag();
            let _ = bhs2.cmd_sn();
            let _ = bhs2.expected_data_transfer_length();
        });
    });
}

fn bench_iscsi_text_params(c: &mut Criterion) {
    let mut group = c.benchmark_group("iscsi_text_params");

    for count in [2, 5, 10] {
        let params: Vec<(&str, &str)> = (0..count)
            .map(|_| ("InitiatorName", "iqn.2024.io.stormblock:test-initiator"))
            .collect();

        group.bench_with_input(BenchmarkId::new("encode", count), &params, |b, params| {
            b.iter(|| {
                encode_text_params(params)
            });
        });

        let encoded = encode_text_params(&params);
        group.bench_with_input(BenchmarkId::new("parse", count), &encoded, |b, encoded| {
            b.iter(|| {
                parse_text_params(encoded)
            });
        });
    }
    group.finish();
}

fn bench_iscsi_pdu_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("iscsi_pdu_write");

    for size in [0, 512, 4096, 65536] {
        group.throughput(Throughput::Bytes(48 + size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let mut bhs = Bhs::new();
            bhs.set_opcode(Opcode::DataIn);
            bhs.set_final(true);
            bhs.set_has_status(true);
            let data = vec![0xAA_u8; size];
            let pdu = IscsiPdu::with_data(bhs, data);

            b.iter(|| {
                let mut buf = Vec::with_capacity(48 + size + 4);
                rt.block_on(write_pdu(&mut buf, &pdu, false, false)).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_iscsi_pdu_read(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("iscsi_pdu_read");

    for size in [0, 512, 4096, 65536] {
        // Pre-serialize PDU
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::DataIn);
        bhs.set_final(true);
        let data = vec![0xBB_u8; size];
        let pdu = IscsiPdu::with_data(bhs, data);
        let mut wire = Vec::new();
        rt.block_on(write_pdu(&mut wire, &pdu, false, false)).unwrap();

        group.throughput(Throughput::Bytes(wire.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &wire, |b, wire| {
            b.iter(|| {
                let mut cursor = std::io::Cursor::new(wire.as_slice());
                rt.block_on(read_pdu(&mut cursor, false, false)).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_iscsi_pdu_roundtrip_digest(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("iscsi_pdu_roundtrip_digest_4k", |b| {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::ScsiCommand);
        bhs.set_immediate(true);
        let data = vec![0xCC_u8; 4096];
        let pdu = IscsiPdu::with_data(bhs, data);

        b.iter(|| {
            let mut buf = Vec::with_capacity(4200);
            rt.block_on(write_pdu(&mut buf, &pdu, true, true)).unwrap();
            let mut cursor = std::io::Cursor::new(buf.as_slice());
            rt.block_on(read_pdu(&mut cursor, true, true)).unwrap();
        });
    });
}

fn bench_nvmeof_common_header(c: &mut Criterion) {
    c.bench_function("nvmeof_common_header_roundtrip", |b| {
        b.iter(|| {
            let ch = CommonHeader {
                pdu_type: PduType::CapsuleCmd as u8,
                flags: 0x03,
                hlen: 72,
                pdo: 72,
                plen: 4168,
            };
            let bytes = ch.to_bytes();
            let ch2 = CommonHeader::from_bytes(&bytes);
            let _ = ch2.hdgst_enable();
            let _ = ch2.ddgst_enable();
        });
    });
}

fn bench_nvmeof_sqe_parse(c: &mut Criterion) {
    c.bench_function("nvmeof_sqe_parse", |b| {
        let mut raw = [0u8; 64];
        raw[0] = 0x01; // Write opcode
        raw[2..4].copy_from_slice(&1u16.to_le_bytes());
        raw[4..8].copy_from_slice(&1u32.to_le_bytes());
        raw[40..44].copy_from_slice(&100u32.to_le_bytes());
        raw[44..48].copy_from_slice(&0u32.to_le_bytes());
        raw[48..52].copy_from_slice(&7u32.to_le_bytes());

        b.iter(|| {
            let sqe = NvmeSqe::from_bytes(&raw);
            let _ = sqe.opcode();
            let _ = sqe.cid();
            let _ = sqe.nsid();
            let _ = sqe.cdw10();
            let _ = sqe.cdw11();
            let _ = sqe.cdw12();
        });
    });
}

fn bench_nvmeof_capsule_resp_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("nvmeof_capsule_resp_write", |b| {
        let cqe = NvmeCqe::success(1, 0, 0);
        b.iter(|| {
            let mut buf = Vec::with_capacity(32);
            rt.block_on(write_capsule_resp(&mut buf, &cqe, false)).unwrap();
        });
    });
}

fn bench_nvmeof_c2h_data_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("nvmeof_c2h_data_write");

    for size in [4096, 65536, 262144] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let data = vec![0xDD_u8; size];
            b.iter(|| {
                let mut buf = Vec::with_capacity(size + 32);
                rt.block_on(write_c2h_data(&mut buf, 1, 0, &data, true, true, false, false)).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_nvmeof_ic_resp_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("nvmeof_ic_resp_write", |b| {
        let resp = ICResp { pfv: 0, cpda: 0, dgst: 0, maxh2cdata: 131072 };
        b.iter(|| {
            let mut buf = Vec::with_capacity(128);
            rt.block_on(write_ic_resp(&mut buf, &resp)).unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_iscsi_bhs_roundtrip,
    bench_iscsi_text_params,
    bench_iscsi_pdu_write,
    bench_iscsi_pdu_read,
    bench_iscsi_pdu_roundtrip_digest,
    bench_nvmeof_common_header,
    bench_nvmeof_sqe_parse,
    bench_nvmeof_capsule_resp_write,
    bench_nvmeof_c2h_data_write,
    bench_nvmeof_ic_resp_write,
);
criterion_main!(benches);
