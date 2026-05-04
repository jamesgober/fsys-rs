use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fsys::builder::Builder;
use fsys::method::Method;

fn bench_method_comparison(c: &mut Criterion) {
    let dir = std::env::temp_dir();
    let data = vec![0u8; 4096];

    let sync_handle = Builder::new()
        .method(Method::Sync)
        .build()
        .expect("sync handle");
    let data_handle = Builder::new()
        .method(Method::Data)
        .build()
        .expect("data handle");

    let mut group = c.benchmark_group("method_comparison_4k");
    group.throughput(Throughput::Bytes(data.len() as u64));

    let sync_path = dir.join(format!("fsys_bench_cmp_sync_{}.dat", std::process::id()));
    group.bench_function("method_sync", |b| {
        b.iter(|| sync_handle.write(&sync_path, &data).expect("write"));
    });
    let _ = std::fs::remove_file(&sync_path);

    let data_path = dir.join(format!("fsys_bench_cmp_data_{}.dat", std::process::id()));
    group.bench_function("method_data", |b| {
        b.iter(|| data_handle.write(&data_path, &data).expect("write"));
    });
    let _ = std::fs::remove_file(&data_path);

    group.finish();
}

criterion_group!(benches, bench_method_comparison);
criterion_main!(benches);
