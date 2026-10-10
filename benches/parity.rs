use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use stormblock::raid::parity::{ParityEngine, SimdLevel};

fn bench_xor_parity(c: &mut Criterion) {
    let engine = ParityEngine::detect();
    let mut group = c.benchmark_group("xor_parity");

    for size in [4096, 65536, 262144, 1048576] {
        group.throughput(Throughput::Bytes(size as u64 * 3)); // 3 data strips
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let strip0 = vec![0xAAu8; size];
            let strip1 = vec![0xBBu8; size];
            let strip2 = vec![0xCCu8; size];
            let strips: Vec<&[u8]> = vec![&strip0, &strip1, &strip2];
            let mut parity = vec![0u8; size];

            b.iter(|| {
                engine.compute_xor_parity(&strips, &mut parity);
            });
        });
    }
    group.finish();
}

fn bench_xor_in_place(c: &mut Criterion) {
    let engine = ParityEngine::detect();
    let mut group = c.benchmark_group("xor_in_place");

    for size in [4096, 65536, 262144, 1048576] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let src = vec![0xAAu8; size];
            let mut dst = vec![0xBBu8; size];

            b.iter(|| {
                engine.xor_in_place(&mut dst, &src);
            });
        });
    }
    group.finish();
}

fn bench_raid6_parity(c: &mut Criterion) {
    let engine = ParityEngine::detect();
    let mut group = c.benchmark_group("raid6_parity");

    for size in [4096, 65536, 262144] {
        group.throughput(Throughput::Bytes(size as u64 * 4)); // 4 data strips
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            let strips: Vec<Vec<u8>> = (0..4).map(|i| vec![(0x10 + i) as u8; size]).collect();
            let strip_refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
            let mut p = vec![0u8; size];
            let mut q = vec![0u8; size];

            b.iter(|| {
                engine.compute_raid6_parity(&strip_refs, &mut p, &mut q);
            });
        });
    }
    group.finish();
}

/// The portable code against the detected level on an 11-wide RAID-6 set
/// (9 data strips): a full stripe's P+Q, and Q's read-modify-write (#255).
fn bench_raid6_levels(c: &mut Criterion) {
    let detected = ParityEngine::detect().level;
    let mut group = c.benchmark_group("raid6_9+2");
    let size = 65536;
    let strips: Vec<Vec<u8>> = (0..9).map(|i| (0..size).map(|j| (j * 31 + i * 7) as u8).collect()).collect();
    let refs: Vec<&[u8]> = strips.iter().map(|s| s.as_slice()).collect();
    for level in [SimdLevel::Generic, detected] {
        let engine = ParityEngine::with_level(level);
        let (mut p, mut q) = (vec![0u8; size], vec![0u8; size]);
        group.throughput(Throughput::Bytes(size as u64 * 9));
        group.bench_function(BenchmarkId::new("pq", level.to_string()), |b| {
            b.iter(|| engine.compute_raid6_parity(&refs, &mut p, &mut q));
        });
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(BenchmarkId::new("q_update", level.to_string()), |b| {
            b.iter(|| engine.q_update(&mut q, &strips[0], 5));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_xor_parity, bench_xor_in_place, bench_raid6_parity, bench_raid6_levels);
criterion_main!(benches);
