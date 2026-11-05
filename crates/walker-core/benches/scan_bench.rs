use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;
use walker_core::{scan, ScanOptions};

const GROUPS: usize = 40;
const SUBDIRS: usize = 20;
const FILES_PER_DIR: usize = 25;

fn synthetic_tree() -> &'static Path {
    static TREE: OnceLock<TempDir> = OnceLock::new();
    TREE.get_or_init(|| {
        let directory = TempDir::new().expect("create benchmark tree");
        let root = directory.path();
        for group in 0..GROUPS {
            let group_path = root.join(format!("group_{group:03}"));
            fs::create_dir_all(&group_path).expect("create group");
            for subdir in 0..SUBDIRS {
                let sub_path = group_path.join(format!("sub_{subdir:03}"));
                fs::create_dir_all(&sub_path).expect("create subdir");
                for file in 0..FILES_PER_DIR {
                    let name = if file % 5 == 0 {
                        format!("文档_中文_😀_{file:03}.txt")
                    } else {
                        format!("file_{file:03}.dat")
                    };
                    fs::write(sub_path.join(name), b"x").expect("create file");
                }
            }
        }
        directory
    })
    .path()
}

fn walk_throughput(criterion: &mut Criterion) {
    let root = synthetic_tree();
    let options = ScanOptions {
        roots: vec![root.to_path_buf()],
        ..ScanOptions::default()
    };
    let total_files = (GROUPS * SUBDIRS * FILES_PER_DIR) as u64;

    let mut group = criterion.benchmark_group("walk");
    group.sample_size(10);
    group.throughput(Throughput::Elements(total_files));
    group.bench_function("scan_throughput", |bench| {
        bench.iter(|| black_box(scan(black_box(&options))));
    });
    group.finish();
}

criterion_group!(benches, walk_throughput);
criterion_main!(benches);
