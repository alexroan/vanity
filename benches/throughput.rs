use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use vanity::backend::BackendPreference;
use vanity::create2::{Address, Create2Miner, MiningOptions, SearchOutcome, VanityPattern};

const SEARCH_LEN: u64 = 1 << 22;
const OUTSIDE_TARGET: &str = "18162225c723faacff3b021232a717bdbf62605d";

fn main() {
    let miner = Create2Miner::new(
        Address::from_bytes([0; 20]),
        &[0],
        VanityPattern::new(OUTSIDE_TARGET, "").expect("preverified target is valid"),
    );
    let cancelled = AtomicBool::new(false);
    let options = MiningOptions {
        start_counter: 0,
        max_attempts: Some(SEARCH_LEN),
        ..MiningOptions::default()
    };

    let mut cpu = miner
        .backend_session(BackendPreference::Cpu)
        .expect("CPU backend should initialize");
    let cpu_elapsed = timed_search(&miner, &mut cpu, options, &cancelled);

    let cold_started = Instant::now();
    let mut gpu = match miner.backend_session(BackendPreference::Gpu) {
        Ok(session) => session,
        Err(error) if std::env::var_os("VANITY_REQUIRE_GPU").is_none() => {
            println!("GPU benchmark skipped: {error}");
            return;
        }
        Err(error) => panic!("VANITY_REQUIRE_GPU is set but GPU initialization failed: {error}"),
    };
    let cold_elapsed = cold_started.elapsed();

    // Warm the pipeline and allocations separately from the steady-state run.
    let warmup = MiningOptions {
        max_attempts: Some(262_144),
        ..options
    };
    let _ = timed_search(&miner, &mut gpu, warmup, &cancelled);
    let gpu_elapsed = timed_search(&miner, &mut gpu, options, &cancelled);

    let cpu_rate = rate(SEARCH_LEN, cpu_elapsed);
    let gpu_rate = rate(SEARCH_LEN, gpu_elapsed);
    let speedup = gpu_rate / cpu_rate;
    println!("Backend: {}", gpu.info().summary());
    println!(
        "Cold GPU initialization: {:.3}s",
        cold_elapsed.as_secs_f64()
    );
    println!(
        "CPU steady state: {:.2} M candidates/s",
        cpu_rate / 1_000_000.0
    );
    println!(
        "GPU steady state: {:.2} M candidates/s",
        gpu_rate / 1_000_000.0
    );
    println!("GPU speedup: {speedup:.2}x");
    assert!(
        speedup >= 2.0,
        "warm GPU throughput must be at least 2x Rayon CPU throughput (measured {speedup:.2}x)"
    );
}

fn timed_search(
    miner: &Create2Miner,
    session: &mut vanity::BackendSession,
    options: MiningOptions,
    cancelled: &AtomicBool,
) -> Duration {
    let started = Instant::now();
    let outcome = miner
        .search_with_backend(session, options, cancelled, |_| {})
        .expect("benchmark search should complete");
    assert_eq!(
        outcome,
        SearchOutcome::NotFound {
            candidates_checked: u128::from(options.max_attempts.unwrap())
        },
        "the preverified full-address target must be outside the benchmark range"
    );
    started.elapsed()
}

fn rate(candidates: u64, elapsed: Duration) -> f64 {
    candidates as f64 / elapsed.as_secs_f64()
}
