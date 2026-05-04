use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;

fn bench_single_read(c: &mut Criterion) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("fsys_bench_read_{}.dat", std::process::id()));
    let data = vec![0u8; 4096];

    let handle = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("build handle");

    // Prepare the file once before benchmarking.
    handle.write(&path, &data).expect("prepare file");

    let mut group = c.benchmark_group("single_read");
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("sync_4k", |b| {
        b.iter(|| {
            let _ = handle.read(&path).expect("read");
        });
    });

    group.finish();
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_single_read);
criterion_main!(benches);
