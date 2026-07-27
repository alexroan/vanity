use crate::create2::{
    Address, Create2Miner, MiningOptions, SearchOutcome, SearchProgress, SearchResult,
    create2_address_from_hash, create2_digest_from_hash, salt_from_counter,
};
use rayon::prelude::*;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_SCANNER_BATCH: u64 = u32::MAX as u64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackendPreference {
    #[default]
    Auto,
    Gpu,
    Cpu,
}

impl fmt::Display for BackendPreference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Gpu => "gpu",
            Self::Cpu => "cpu",
        })
    }
}

impl FromStr for BackendPreference {
    type Err = BackendError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "gpu" => Ok(Self::Gpu),
            "cpu" => Ok(Self::Cpu),
            _ => Err(BackendError::new(format!(
                "unknown backend `{value}`; expected auto, gpu, or cpu"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendKind {
    Gpu,
    Cpu,
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Gpu => "GPU",
            Self::Cpu => "CPU (Rayon)",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub adapter: Option<String>,
    pub graphics_api: Option<String>,
    pub fallback_reason: Option<String>,
}

impl BackendInfo {
    fn cpu(fallback_reason: Option<String>) -> Self {
        Self {
            kind: BackendKind::Cpu,
            adapter: None,
            graphics_api: None,
            fallback_reason,
        }
    }

    pub fn summary(&self) -> String {
        match (&self.adapter, &self.graphics_api) {
            (Some(adapter), Some(api)) => format!("{} — {adapter} ({api})", self.kind),
            _ => self.kind.to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchEvent {
    Progress(SearchProgress),
    Fallback {
        reason: String,
        backend: BackendInfo,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError {
    message: String,
}

impl BackendError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MiningKey {
    pub deployer: Address,
    pub init_code_hash: [u8; 32],
    pub mask: [u32; 5],
    pub value: [u32; 5],
}

impl MiningKey {
    pub(crate) fn from_miner(miner: &Create2Miner) -> Self {
        let (mask, value) = miner.pattern().packed_mask_value();
        Self {
            deployer: miner.deployer(),
            init_code_hash: *miner.init_code_hash(),
            mask,
            value,
        }
    }
}

pub struct BackendSession {
    preference: BackendPreference,
    info: BackendInfo,
    key: MiningKey,
    scanner: Scanner,
}

impl BackendSession {
    pub fn new(miner: &Create2Miner, preference: BackendPreference) -> Result<Self, BackendError> {
        let key = MiningKey::from_miner(miner);

        match preference {
            BackendPreference::Cpu => Ok(Self::cpu(key, preference, None)),
            BackendPreference::Gpu => {
                #[cfg(feature = "gpu")]
                {
                    let scanner = crate::gpu::GpuScanner::new(key)?;
                    let info = scanner.backend_info();
                    Ok(Self {
                        preference,
                        info,
                        key,
                        scanner: Scanner::Gpu(Box::new(scanner)),
                    })
                }
                #[cfg(not(feature = "gpu"))]
                {
                    Err(BackendError::new(
                        "GPU backend requested, but this binary was built without the `gpu` feature",
                    ))
                }
            }
            BackendPreference::Auto => {
                #[cfg(feature = "gpu")]
                {
                    match crate::gpu::GpuScanner::new(key) {
                        Ok(scanner) => {
                            let info = scanner.backend_info();
                            Ok(Self {
                                preference,
                                info,
                                key,
                                scanner: Scanner::Gpu(Box::new(scanner)),
                            })
                        }
                        Err(error) => Ok(Self::cpu(key, preference, Some(error.to_string()))),
                    }
                }
                #[cfg(not(feature = "gpu"))]
                {
                    Ok(Self::cpu(
                        key,
                        preference,
                        Some("GPU support is not compiled into this binary".to_owned()),
                    ))
                }
            }
        }
    }

    fn cpu(key: MiningKey, preference: BackendPreference, fallback_reason: Option<String>) -> Self {
        Self {
            preference,
            info: BackendInfo::cpu(fallback_reason),
            key,
            scanner: Scanner::Cpu(CpuScanner::new(key)),
        }
    }

    pub fn info(&self) -> &BackendInfo {
        &self.info
    }

    fn is_gpu(&self) -> bool {
        match self.scanner {
            #[cfg(feature = "gpu")]
            Scanner::Gpu(_) => true,
            #[cfg(test)]
            Scanner::TestGpu(_) => true,
            Scanner::Cpu(_) => false,
        }
    }

    fn fallback_to_cpu(&mut self, reason: String) -> BackendInfo {
        self.scanner = Scanner::Cpu(CpuScanner::new(self.key));
        self.info = BackendInfo::cpu(Some(reason));
        self.info.clone()
    }
}

enum Scanner {
    Cpu(CpuScanner),
    #[cfg(feature = "gpu")]
    Gpu(Box<crate::gpu::GpuScanner>),
    #[cfg(test)]
    TestGpu(Box<dyn BatchScanner>),
}

impl BatchScanner for Scanner {
    fn scan(&mut self, batch: Batch, cancelled: &AtomicBool) -> Result<BatchScan, BackendError> {
        match self {
            Self::Cpu(scanner) => scanner.scan(batch, cancelled),
            #[cfg(feature = "gpu")]
            Self::Gpu(scanner) => scanner.scan(batch, cancelled),
            #[cfg(test)]
            Self::TestGpu(scanner) => scanner.scan(batch, cancelled),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Batch {
    pub start_counter: u64,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BatchScan {
    Complete {
        match_offset: Option<u32>,
        witness: [u8; 32],
    },
    Cancelled,
}

pub(crate) trait BatchScanner {
    fn scan(&mut self, batch: Batch, cancelled: &AtomicBool) -> Result<BatchScan, BackendError>;
}

struct CpuScanner {
    key: MiningKey,
}

impl CpuScanner {
    const fn new(key: MiningKey) -> Self {
        Self { key }
    }
}

impl BatchScanner for CpuScanner {
    fn scan(&mut self, batch: Batch, cancelled: &AtomicBool) -> Result<BatchScan, BackendError> {
        let witness = create2_digest_from_hash(
            self.key.deployer,
            salt_from_counter(batch.start_counter),
            self.key.init_code_hash,
        );
        let found = (0..batch.count).into_par_iter().find_first(|offset| {
            if cancelled.load(Ordering::Relaxed) {
                return false;
            }
            let counter = batch.start_counter + u64::from(*offset);
            let salt = salt_from_counter(counter);
            let address =
                create2_address_from_hash(self.key.deployer, salt, self.key.init_code_hash);
            packed_address_matches(address, self.key.mask, self.key.value)
        });

        if let Some(match_offset) = found {
            return Ok(BatchScan::Complete {
                match_offset: Some(match_offset),
                witness,
            });
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(BatchScan::Cancelled);
        }
        Ok(BatchScan::Complete {
            match_offset: None,
            witness,
        })
    }
}

pub(crate) fn search(
    miner: &Create2Miner,
    session: &mut BackendSession,
    options: MiningOptions,
    cancelled: &AtomicBool,
    mut on_event: impl FnMut(SearchEvent),
) -> Result<SearchOutcome, BackendError> {
    let key = MiningKey::from_miner(miner);
    if session.key != key {
        return Err(BackendError::new(
            "backend session belongs to a different CREATE2 search configuration",
        ));
    }

    let batch_size = options.batch_size.clamp(1, MAX_SCANNER_BATCH);
    let mut cursor = options.start_counter;
    let mut candidates_checked = 0_u128;
    let mut attempts_remaining = options.max_attempts.map(u128::from);

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(SearchOutcome::Cancelled);
        }
        if attempts_remaining == Some(0) {
            return Ok(SearchOutcome::NotFound { candidates_checked });
        }

        let available = u128::from(u64::MAX) - u128::from(cursor) + 1;
        let mut count = available.min(u128::from(batch_size));
        if let Some(remaining) = attempts_remaining {
            count = count.min(remaining);
        }
        let batch = Batch {
            start_counter: cursor,
            count: u32::try_from(count).expect("batch count is limited to u32"),
        };

        let scan = match session.scanner.scan(batch, cancelled) {
            Ok(scan) => scan,
            Err(error) => {
                if session.preference == BackendPreference::Auto && session.is_gpu() {
                    let reason = error.to_string();
                    let backend = session.fallback_to_cpu(reason.clone());
                    on_event(SearchEvent::Fallback { reason, backend });
                    continue;
                }
                return Err(error);
            }
        };

        let BatchScan::Complete {
            match_offset,
            witness,
        } = scan
        else {
            return Ok(SearchOutcome::Cancelled);
        };

        if let Err(error) = validate_batch(miner, batch, match_offset, witness) {
            if session.preference == BackendPreference::Auto && session.is_gpu() {
                let reason = error.to_string();
                let backend = session.fallback_to_cpu(reason.clone());
                on_event(SearchEvent::Fallback { reason, backend });
                continue;
            }
            return Err(error);
        }

        if let Some(offset) = match_offset {
            let counter = batch
                .start_counter
                .checked_add(u64::from(offset))
                .ok_or_else(|| BackendError::new("backend returned an overflowing match offset"))?;
            let salt = salt_from_counter(counter);
            let address =
                create2_address_from_hash(miner.deployer(), salt, *miner.init_code_hash());
            return Ok(SearchOutcome::Found(SearchResult { address, salt }));
        }

        let committed = u128::from(batch.count);
        candidates_checked += committed;
        on_event(SearchEvent::Progress(SearchProgress { candidates_checked }));
        if let Some(remaining) = &mut attempts_remaining {
            *remaining -= committed;
        }

        if committed == available {
            return Ok(SearchOutcome::NotFound { candidates_checked });
        }
        cursor += u64::from(batch.count);
    }
}

fn validate_batch(
    miner: &Create2Miner,
    batch: Batch,
    match_offset: Option<u32>,
    witness: [u8; 32],
) -> Result<(), BackendError> {
    let expected_witness = create2_digest_from_hash(
        miner.deployer(),
        salt_from_counter(batch.start_counter),
        *miner.init_code_hash(),
    );
    if witness != expected_witness {
        return Err(BackendError::new(format!(
            "backend digest witness failed for counter {}",
            batch.start_counter
        )));
    }

    let Some(offset) = match_offset else {
        return Ok(());
    };
    if offset >= batch.count {
        return Err(BackendError::new(format!(
            "backend returned invalid match offset {offset} for a batch of {}",
            batch.count
        )));
    }

    let counter = batch
        .start_counter
        .checked_add(u64::from(offset))
        .ok_or_else(|| BackendError::new("backend match offset overflowed the counter range"))?;
    let salt = salt_from_counter(counter);
    let address = create2_address_from_hash(miner.deployer(), salt, *miner.init_code_hash());
    if !miner.pattern().matches(&address) {
        return Err(BackendError::new(format!(
            "backend reported a false match at counter {counter}"
        )));
    }
    Ok(())
}

fn packed_address_matches(address: Address, mask: [u32; 5], value: [u32; 5]) -> bool {
    address
        .as_bytes()
        .chunks_exact(4)
        .enumerate()
        .all(|(index, bytes)| {
            let word = u32::from_le_bytes(bytes.try_into().expect("chunk has four bytes"));
            word & mask[index] == value[index]
        })
}

#[cfg(any(feature = "gpu", test))]
pub(crate) fn padded_rate_block(key: MiningKey) -> [u8; 136] {
    let mut block = [0_u8; 136];
    block[0] = 0xff;
    block[1..21].copy_from_slice(key.deployer.as_bytes());
    // Salt bytes 21..53 are zero here. The shader injects the counter into
    // the final eight bytes at preimage offsets 45..53.
    block[53..85].copy_from_slice(&key.init_code_hash);
    block[85] = 0x01;
    block[135] = 0x80;
    block
}

#[cfg(feature = "gpu")]
pub(crate) fn block_lanes(block: [u8; 136]) -> [u64; 17] {
    let mut lanes = [0_u64; 17];
    for (lane, bytes) in lanes.iter_mut().zip(block.chunks_exact(8)) {
        *lane = u64::from_le_bytes(bytes.try_into().expect("chunk has eight bytes"));
    }
    lanes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create2::{VanityPattern, create2_address, keccak256};

    fn miner_for(pattern: VanityPattern) -> Create2Miner {
        Create2Miner::new(Address::from_bytes([0; 20]), &[0], pattern)
    }

    #[test]
    fn backend_preferences_parse_strict_cli_values() {
        assert_eq!("auto".parse(), Ok(BackendPreference::Auto));
        assert_eq!("gpu".parse(), Ok(BackendPreference::Gpu));
        assert_eq!("cpu".parse(), Ok(BackendPreference::Cpu));
        assert!("GPU".parse::<BackendPreference>().is_err());
    }

    #[cfg(not(feature = "gpu"))]
    #[test]
    fn cpu_only_builds_auto_select_cpu_and_reject_explicit_gpu() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let auto = BackendSession::new(&miner, BackendPreference::Auto).unwrap();
        assert_eq!(auto.info().kind, BackendKind::Cpu);
        assert!(
            auto.info()
                .fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not compiled"))
        );
        assert!(BackendSession::new(&miner, BackendPreference::Gpu).is_err());
    }

    #[test]
    fn pattern_mask_packing_handles_odd_nibbles() {
        let pattern = VanityPattern::new("a", "b").unwrap();
        let (mask, value) = pattern.packed_mask_value();
        assert_eq!(mask[0] & 0xff, 0xf0);
        assert_eq!(value[0] & 0xff, 0xa0);
        assert_eq!(mask[4] >> 24, 0x0f);
        assert_eq!(value[4] >> 24, 0x0b);
    }

    #[test]
    fn padded_block_places_preimage_padding_and_counter_gap() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let key = MiningKey::from_miner(&miner);
        let block = padded_rate_block(key);
        assert_eq!(block[0], 0xff);
        assert_eq!(&block[1..21], key.deployer.as_bytes());
        assert_eq!(&block[21..53], &[0; 32]);
        assert_eq!(&block[53..85], &key.init_code_hash);
        assert_eq!(block[85], 0x01);
        assert!(block[86..135].iter().all(|byte| *byte == 0));
        assert_eq!(block[135], 0x80);
    }

    #[test]
    fn host_counter_injection_matches_cpu_preimage_at_boundaries() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let key = MiningKey::from_miner(&miner);
        for counter in [u64::from(u32::MAX), u64::from(u32::MAX) + 1, u64::MAX] {
            let mut block = padded_rate_block(key);
            block[45..53].copy_from_slice(&counter.to_be_bytes());
            let digest_from_block = {
                // Keccak padding is already present; compare the source bytes
                // before padding with the ordinary host implementation.
                keccak256(&block[..85])
            };
            let expected = create2_digest_from_hash(
                key.deployer,
                salt_from_counter(counter),
                key.init_code_hash,
            );
            assert_eq!(digest_from_block, expected);
        }
    }

    #[test]
    fn zero_attempts_do_not_scan() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let mut session = BackendSession::new(&miner, BackendPreference::Cpu).unwrap();
        let outcome = search(
            &miner,
            &mut session,
            MiningOptions {
                max_attempts: Some(0),
                ..MiningOptions::default()
            },
            &AtomicBool::new(false),
            |_| panic!("zero attempts must not emit progress"),
        )
        .unwrap();
        assert_eq!(
            outcome,
            SearchOutcome::NotFound {
                candidates_checked: 0
            }
        );
    }

    #[test]
    fn progress_commits_only_complete_partitioned_batches() {
        let miner = miner_for(VanityPattern::new("ffffffffff", "").unwrap());
        let mut session = BackendSession::new(&miner, BackendPreference::Cpu).unwrap();
        let mut progress = Vec::new();
        let outcome = search(
            &miner,
            &mut session,
            MiningOptions {
                start_counter: 0,
                max_attempts: Some(5),
                batch_size: 2,
            },
            &AtomicBool::new(false),
            |event| {
                if let SearchEvent::Progress(update) = event {
                    progress.push(update.candidates_checked);
                }
            },
        )
        .unwrap();
        assert_eq!(
            outcome,
            SearchOutcome::NotFound {
                candidates_checked: 5
            }
        );
        assert_eq!(progress, [2, 4, 5]);
    }

    #[test]
    fn validation_rejects_out_of_range_offsets_and_bad_witnesses() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let batch = Batch {
            start_counter: 7,
            count: 3,
        };
        let witness = create2_digest_from_hash(
            miner.deployer(),
            salt_from_counter(batch.start_counter),
            *miner.init_code_hash(),
        );
        let offset_error = validate_batch(&miner, batch, Some(3), witness).unwrap_err();
        assert!(offset_error.to_string().contains("invalid match offset"));

        let mut bad_witness = witness;
        bad_witness[0] ^= 1;
        let witness_error = validate_batch(&miner, batch, None, bad_witness).unwrap_err();
        assert!(witness_error.to_string().contains("digest witness failed"));
    }

    struct FailingScanner;

    impl BatchScanner for FailingScanner {
        fn scan(
            &mut self,
            _batch: Batch,
            _cancelled: &AtomicBool,
        ) -> Result<BatchScan, BackendError> {
            Err(BackendError::new("injected GPU readback failure"))
        }
    }

    struct BadWitnessScanner;

    impl BatchScanner for BadWitnessScanner {
        fn scan(
            &mut self,
            _batch: Batch,
            _cancelled: &AtomicBool,
        ) -> Result<BatchScan, BackendError> {
            Ok(BatchScan::Complete {
                match_offset: None,
                witness: [0; 32],
            })
        }
    }

    #[test]
    fn automatic_fallback_retries_the_uncommitted_batch_without_skipping() {
        let target_counter = 42;
        let deployer = Address::from_bytes([0; 20]);
        let target = create2_address(deployer, salt_from_counter(target_counter), &[0]);
        let miner = Create2Miner::new(
            deployer,
            &[0],
            VanityPattern::new(&target.to_string(), "").unwrap(),
        );
        let key = MiningKey::from_miner(&miner);
        let mut session = BackendSession {
            preference: BackendPreference::Auto,
            info: BackendInfo {
                kind: BackendKind::Gpu,
                adapter: Some("fault injector".to_owned()),
                graphics_api: Some("test".to_owned()),
                fallback_reason: None,
            },
            key,
            scanner: Scanner::TestGpu(Box::new(FailingScanner)),
        };
        let mut saw_fallback = false;
        let outcome = search(
            &miner,
            &mut session,
            MiningOptions {
                start_counter: target_counter,
                max_attempts: Some(1),
                batch_size: 1,
            },
            &AtomicBool::new(false),
            |event| saw_fallback |= matches!(event, SearchEvent::Fallback { .. }),
        )
        .unwrap();

        let SearchOutcome::Found(result) = outcome else {
            panic!("CPU fallback should retry counter 42");
        };
        assert_eq!(result.salt, salt_from_counter(target_counter));
        assert!(saw_fallback);
        assert_eq!(session.info().kind, BackendKind::Cpu);
    }

    #[test]
    fn explicit_gpu_mode_surfaces_backend_failures() {
        let miner = miner_for(VanityPattern::new("0", "").unwrap());
        let key = MiningKey::from_miner(&miner);
        let mut session = BackendSession {
            preference: BackendPreference::Gpu,
            info: BackendInfo {
                kind: BackendKind::Gpu,
                adapter: Some("fault injector".to_owned()),
                graphics_api: Some("test".to_owned()),
                fallback_reason: None,
            },
            key,
            scanner: Scanner::TestGpu(Box::new(FailingScanner)),
        };
        let error = search(
            &miner,
            &mut session,
            MiningOptions {
                max_attempts: Some(1),
                ..MiningOptions::default()
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected GPU readback failure"));
    }

    #[test]
    fn automatic_witness_failure_falls_back_and_retries_the_batch() {
        let target_counter = 9;
        let deployer = Address::from_bytes([0; 20]);
        let target = create2_address(deployer, salt_from_counter(target_counter), &[0]);
        let miner = Create2Miner::new(
            deployer,
            &[0],
            VanityPattern::new(&target.to_string(), "").unwrap(),
        );
        let key = MiningKey::from_miner(&miner);
        let mut session = BackendSession {
            preference: BackendPreference::Auto,
            info: BackendInfo {
                kind: BackendKind::Gpu,
                adapter: Some("bad witness injector".to_owned()),
                graphics_api: Some("test".to_owned()),
                fallback_reason: None,
            },
            key,
            scanner: Scanner::TestGpu(Box::new(BadWitnessScanner)),
        };
        let mut fallback_reason = None;
        let outcome = search(
            &miner,
            &mut session,
            MiningOptions {
                start_counter: target_counter,
                max_attempts: Some(1),
                batch_size: 1,
            },
            &AtomicBool::new(false),
            |event| {
                if let SearchEvent::Fallback { reason, .. } = event {
                    fallback_reason = Some(reason);
                }
            },
        )
        .unwrap();

        let SearchOutcome::Found(result) = outcome else {
            panic!("CPU fallback should retry the witness-failed batch");
        };
        assert_eq!(result.salt, salt_from_counter(target_counter));
        assert!(
            fallback_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("digest witness failed"))
        );
    }
}
