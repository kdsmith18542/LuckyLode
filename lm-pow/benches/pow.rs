use criterion::{criterion_group, criterion_main, Criterion};
use lm_pow::pow_hash;
use std::hint::black_box;

fn bench_pow(c: &mut Criterion) {
    // Same 80-byte header shape as the frozen vectors.
    let header: Vec<u8> = (0u8..80).collect();
    c.bench_function("emberhash_v1_single", |b| {
        b.iter(|| pow_hash(black_box(&header), black_box(0), black_box(1)))
    });
}

criterion_group!(benches, bench_pow);
criterion_main!(benches);
