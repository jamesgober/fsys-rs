use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;

fn bench_single_write(c: &mut Criterion) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("fsys_bench_write_{}.dat", std::process::id()));
    let data = vec![0u8; 4096];

    let handle = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("build handle");

    let mut group = c.benchmark_group("single_write");
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("sync_4k", |b| {
        b.iter(|| {
            handle.write(&path, &data).expect("write");
        });
    });

    group.finish();
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_single_write);
criterion_main!(benches);
