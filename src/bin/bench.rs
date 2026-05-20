// src/bin/bench.rs
//
// Run with:
//   cargo run --bin bench --release
//
// Useful env vars:
//   ASDB_BENCH_INSERTS=10000000
//   ASDB_BENCH_LOOKUPS=1000000
//   ASDB_BENCH_RANGE_TOTAL=1000000
//   ASDB_BENCH_PRESSURE_N=1000000
//   ASDB_BENCH_PRESSURE_LOOKUPS=1000000

use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use asdb::btree::btree::BTree;
use asdb::document::{serialize_key, Collection, Document, Value};
use asdb::storage::buffer_pool::BufferPool;
use asdb::storage::disk::DiskManager;
use asdb::storage::heap::{DocId, HeapFile};

// ----------------------------------------------------------------
// Config
// ----------------------------------------------------------------

const DEFAULT_INSERT_COUNT: usize = 100_000;
const DEFAULT_LOOKUP_COUNT: usize = 100_000;
const DEFAULT_POOL_CAPACITY: usize = 1024;
const DEFAULT_CROSSOVER_SAMPLES: usize = 100;
const DEFAULT_PRESSURE_LOOKUPS: usize = 10_000;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// ----------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = env::temp_dir();
        path.push(format!("asdb-bench-{}-{counter}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn make_pool(capacity: usize) -> (BufferPool, TempDb) {
    let tmp = TempDb::new();
    let disk = DiskManager::open(&tmp.path).unwrap();
    (BufferPool::new(disk, capacity), tmp)
}

fn make_collection(capacity: usize) -> (Collection, TempDb) {
    let (pool, tmp) = make_pool(capacity);
    let heap = HeapFile::create(pool).unwrap();
    (Collection::create(heap), tmp)
}

fn make_btree(pool: &mut BufferPool) -> BTree<'_> {
    BTree::new(pool).unwrap()
}

fn doc_id(n: usize) -> DocId {
    (0, (n % u16::MAX as usize) as u16)
}

fn int_key(n: i64) -> Vec<u8> {
    serialize_key(&Value::Int(n))
}

fn bench_size(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&value| value > 0)
        .unwrap_or(default)
}

fn next_random(rng: &mut u64) -> u64 {
    *rng = rng
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *rng
}

fn user_doc(id: i64, age: i64) -> Document {
    let mut doc = Document::new();
    doc.insert("id".to_string(), Value::Int(id));
    doc.insert("age".to_string(), Value::Int(age));
    doc
}

/// Compute percentile from a sorted slice of durations.
fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    let idx = ((sorted.len() as f64) * pct / 100.0) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn print_percentiles(label: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    println!(
        "{label}: p50={:.2}µs  p95={:.2}µs  p99={:.2}µs",
        percentile(&samples, 50.0).as_secs_f64() * 1e6,
        percentile(&samples, 95.0).as_secs_f64() * 1e6,
        percentile(&samples, 99.0).as_secs_f64() * 1e6,
    );
}

fn avg_duration(total: Duration, count: usize) -> Duration {
    Duration::from_secs_f64(total.as_secs_f64() / count as f64)
}

// ----------------------------------------------------------------
// 1. Sequential vs random insert
// ----------------------------------------------------------------

fn bench_sequential_insert(insert_count: usize) {
    println!("\n=== 1. Sequential insert ({insert_count} keys) ===");
    let (mut pool, _tmp) = make_pool(DEFAULT_POOL_CAPACITY);
    let mut btree = make_btree(&mut pool);

    let start = Instant::now();
    for i in 0..insert_count as i64 {
        let key = int_key(i);
        btree.insert(&key, doc_id(i as usize)).unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "Sequential: {insert_count} inserts in {:.2}s  ({:.0} ops/sec)",
        elapsed.as_secs_f64(),
        insert_count as f64 / elapsed.as_secs_f64()
    );
}

fn bench_random_insert(insert_count: usize) {
    println!("\n=== 1b. Random insert ({insert_count} keys) ===");
    let (mut pool, _tmp) = make_pool(DEFAULT_POOL_CAPACITY);
    let mut btree = make_btree(&mut pool);

    // Simple LCG for reproducible pseudo-random order
    let mut rng: u64 = 0xdeadbeef_cafebabe;
    let start = Instant::now();
    for i in 0..insert_count as i64 {
        next_random(&mut rng);
        let key = int_key(rng as i64);
        btree.insert(&key, doc_id(i as usize)).unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "Random:     {insert_count} inserts in {:.2}s  ({:.0} ops/sec)",
        elapsed.as_secs_f64(),
        insert_count as f64 / elapsed.as_secs_f64()
    );
}

// ----------------------------------------------------------------
// 2. Point lookup p50/p95/p99
// ----------------------------------------------------------------

fn bench_point_lookup(lookup_count: usize) {
    println!("\n=== 2. Point lookup ({lookup_count} random lookups) ===");

    // Build a tree with LOOKUP_COUNT keys first
    let (mut pool, _tmp) = make_pool(DEFAULT_POOL_CAPACITY);
    let mut btree = make_btree(&mut pool);
    for i in 0..lookup_count as i64 {
        let key = int_key(i);
        btree.insert(&key, doc_id(i as usize)).unwrap();
    }

    // Now do random lookups and record each duration
    let mut rng: u64 = 0x1234567890abcdef;
    let mut samples = Vec::with_capacity(lookup_count);
    for _ in 0..lookup_count {
        next_random(&mut rng);
        let n = (rng % lookup_count as u64) as i64;
        let key = int_key(n);
        let t = Instant::now();
        let _ = btree.search(&key).unwrap();
        samples.push(t.elapsed());
    }

    print_percentiles("Point lookup", samples);
}

// ----------------------------------------------------------------
// 3. Range scan at varying selectivity
// ----------------------------------------------------------------

fn bench_range_scan(total: usize) {
    println!("\n=== 3. Range scan selectivity ===");

    let (mut pool, _tmp) = make_pool(DEFAULT_POOL_CAPACITY);
    let mut btree = make_btree(&mut pool);
    for i in 0..total as i64 {
        let key = int_key(i);
        btree.insert(&key, doc_id(i as usize)).unwrap();
    }

    for &pct in &[0.001f64, 0.01, 0.10] {
        let count = ((total as f64 * pct) as i64).max(1);
        let low = int_key(0);
        let high = int_key(count - 1);

        let t = Instant::now();
        let results = btree.range_scan(&low, &high).unwrap();
        let elapsed = t.elapsed();

        println!(
            "  {:.1}% selectivity: {} docs in {:.2}ms",
            pct * 100.0,
            results.len(),
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

// ----------------------------------------------------------------
// 4. Indexed lookup vs sequential scan crossover
// ----------------------------------------------------------------

fn bench_crossover(samples: usize) {
    println!("\n=== 4. Indexed lookup vs sequential scan crossover ===");

    let sizes: &[usize] = &[100, 500, 1_000, 5_000, 10_000, 50_000, 100_000];

    for &n in sizes {
        let (mut pool, _tmp) = make_pool(DEFAULT_POOL_CAPACITY);
        let mut btree = make_btree(&mut pool);
        let (mut collection, _collection_tmp) = make_collection(DEFAULT_POOL_CAPACITY);

        for i in 0..n as i64 {
            let doc = user_doc(i, i);
            let doc_id = collection.insert(&doc).unwrap();
            btree.insert(&int_key(i), doc_id).unwrap();
        }

        let mut rng: u64 = 0x9e3779b97f4a7c15 ^ n as u64;
        let mut index_total = Duration::ZERO;
        let mut scan_total = Duration::ZERO;

        for _ in 0..samples {
            let age = (next_random(&mut rng) % n as u64) as i64;
            let target = int_key(age);

            let t = Instant::now();
            let indexed = btree.search(&target).unwrap();
            index_total += t.elapsed();
            assert!(indexed.is_some());

            let t = Instant::now();
            let found = collection
                .scan()
                .unwrap()
                .into_iter()
                .find(|(_, doc)| doc.get("age") == Some(&Value::Int(age)));
            scan_total += t.elapsed();
            assert!(found.is_some());
        }

        let indexed_time = avg_duration(index_total, samples);
        let scan_time = avg_duration(scan_total, samples);
        let winner = if indexed_time < scan_time {
            "index"
        } else {
            "scan "
        };
        println!(
            "  n={n:>8}: index_avg={:.2}µs  scan_avg={:.2}µs  winner={winner}",
            indexed_time.as_secs_f64() * 1e6,
            scan_time.as_secs_f64() * 1e6,
        );
    }
}

// ----------------------------------------------------------------
// 5. Buffer pool pressure
// ----------------------------------------------------------------

fn estimate_working_set_pages(n: usize) -> usize {
    // Rough estimate: n keys, ~15 keys/leaf page worst case
    // Plus internal nodes (much fewer). Add 20% overhead.
    let leaf_pages = n.div_ceil(15);
    (leaf_pages as f64 * 1.2) as usize + 1
}

fn bench_buffer_pool_pressure(n: usize, lookups: usize) {
    println!("\n=== 5. Buffer pool pressure ===");

    let working_set = estimate_working_set_pages(n);
    println!("  Estimated working set: ~{working_set} pages");

    for &fraction in &[0.10f64, 0.50, 1.0] {
        let capacity = ((working_set as f64 * fraction) as usize).max(8);
        let (mut pool, _tmp) = make_pool(capacity);
        let mut btree = make_btree(&mut pool);

        // Insert N keys
        for i in 0..n as i64 {
            btree.insert(&int_key(i), doc_id(i as usize)).unwrap();
        }

        // Do random point lookups and measure throughput
        let mut rng: u64 = 0xfeedface12345678;
        let start = Instant::now();
        for _ in 0..lookups {
            next_random(&mut rng);
            let key_num = (rng % n as u64) as i64;
            let _ = btree.search(&int_key(key_num)).unwrap();
        }
        let elapsed = start.elapsed();

        println!(
            "  Pool={:.0}% ({capacity} frames): {lookups} lookups in {:.2}ms  ({:.0} ops/sec)",
            fraction * 100.0,
            elapsed.as_secs_f64() * 1000.0,
            lookups as f64 / elapsed.as_secs_f64()
        );
    }
}

// ----------------------------------------------------------------
// main
// ----------------------------------------------------------------

fn main() {
    println!("BTree Benchmarks");
    println!("================");
    let insert_count = bench_size("ASDB_BENCH_INSERTS", DEFAULT_INSERT_COUNT);
    let lookup_count = bench_size("ASDB_BENCH_LOOKUPS", DEFAULT_LOOKUP_COUNT);
    let range_count = bench_size("ASDB_BENCH_RANGE_TOTAL", DEFAULT_LOOKUP_COUNT);
    let pressure_count = bench_size("ASDB_BENCH_PRESSURE_N", DEFAULT_INSERT_COUNT);
    let crossover_samples = bench_size("ASDB_BENCH_CROSSOVER_SAMPLES", DEFAULT_CROSSOVER_SAMPLES);
    let pressure_lookups = bench_size("ASDB_BENCH_PRESSURE_LOOKUPS", DEFAULT_PRESSURE_LOOKUPS);

    bench_sequential_insert(insert_count);
    bench_random_insert(insert_count);
    bench_point_lookup(lookup_count);
    bench_range_scan(range_count);
    bench_crossover(crossover_samples);
    bench_buffer_pool_pressure(pressure_count, pressure_lookups);

    println!("\nDone.");
}
