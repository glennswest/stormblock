use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use stormblock::raid::RaidArrayId;
use stormblock::volume::extent::ExtentAllocator;
use uuid::Uuid;

fn bench_sequential_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("extent_alloc_sequential");

    for count in [10, 100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let array_id = RaidArrayId(Uuid::new_v4());
                let mut alloc = ExtentAllocator::new(4096);
                alloc.add_array(array_id, count as u64 * 4096 * 2); // room for 2x
                alloc.allocate(array_id, count).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_alloc_free_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("extent_alloc_free_cycle");

    for count in [10, 100, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let array_id = RaidArrayId(Uuid::new_v4());
            let mut alloc = ExtentAllocator::new(4096);
            alloc.add_array(array_id, count as u64 * 4096 * 4);

            b.iter(|| {
                let extents = alloc.allocate(array_id, count).unwrap();
                for ext in &extents {
                    alloc.free(ext);
                }
            });
        });
    }
    group.finish();
}

fn bench_fragmented_allocation(c: &mut Criterion) {
    c.bench_function("extent_alloc_fragmented", |b| {
        let array_id = RaidArrayId(Uuid::new_v4());
        let mut alloc = ExtentAllocator::new(4096);
        alloc.add_array(array_id, 10000 * 4096);

        // Pre-fragment: allocate all, then free odd-indexed
        let extents = alloc.allocate(array_id, 10000).unwrap();
        for (i, ext) in extents.iter().enumerate() {
            if i % 2 == 1 {
                alloc.free(ext);
            }
        }

        b.iter(|| {
            if let Some(exts) = alloc.allocate(array_id, 100) {
                for ext in &exts {
                    alloc.free(ext);
                }
            }
        });
    });
}

criterion_group!(benches, bench_sequential_allocation, bench_alloc_free_cycle, bench_fragmented_allocation);
criterion_main!(benches);
