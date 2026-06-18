use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::io::{Read, Seek, SeekFrom, Write};
use tempfile::TempDir;

fn make_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

fn bench_write_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_stdfs");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            b.iter(|| {
                let path = dir.path().join("block");
                let mut f = std::fs::File::create(&path).unwrap();
                f.write_all(data).unwrap();
                f.sync_all().unwrap();
            });
        });
    }
    group.finish();
}

fn bench_read_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_stdfs");
    for size in [4 * 1024usize, 64 * 1024, 1024 * 1024] {
        let data = make_data(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("block");
            std::fs::write(&path, data).unwrap();
            b.iter(|| {
                let mut f = std::fs::File::open(&path).unwrap();
                let mut buf = vec![0u8; data.len()];
                f.read_exact(&mut buf).unwrap();
                buf
            });
        });
    }
    group.finish();
}

fn bench_partial_read_stdfs(c: &mut Criterion) {
    let mut group = c.benchmark_group("partial_read_stdfs");
    let size = 64 * 1024usize;
    let data = make_data(size);
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("64KB_read_4KB_at_32KB", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("block");
        std::fs::write(&path, &data).unwrap();
        b.iter(|| {
            let mut f = std::fs::File::open(&path).unwrap();
            let mut buf = vec![0u8; 4096];
            f.seek(SeekFrom::Start(32 * 1024)).unwrap();
            f.read_exact(&mut buf).unwrap();
            buf
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_write_stdfs,
    bench_read_stdfs,
    bench_partial_read_stdfs
);
criterion_main!(benches);
