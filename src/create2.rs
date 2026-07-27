use crate::backend::{
    BackendError, BackendPreference, BackendSession, SearchEvent, search as backend_search,
};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use tiny_keccak::{Hasher, Keccak};

const ADDRESS_NIBBLES: usize = 40;
const CREATE2_PREIMAGE_LEN: usize = 1 + 20 + 32 + 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Address([u8; 20]);

impl Address {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl FromStr for Address {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let value = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .unwrap_or(value);

        if value.len() != ADDRESS_NIBBLES {
            return Err(format!(
                "expected 40 hexadecimal characters, got {}",
                value.len()
            ));
        }

        let bytes = hex::decode(value).map_err(|_| "address contains non-hex characters")?;
        let bytes: [u8; 20] = bytes
            .try_into()
            .map_err(|_| "address must contain exactly 20 bytes")?;
        Ok(Self(bytes))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "0x{}", hex::encode(self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Salt([u8; 32]);

impl Salt {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Salt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "0x{}", hex::encode(self.0))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VanityPattern {
    prefix: Vec<u8>,
    suffix: Vec<u8>,
    constraints: [Option<u8>; ADDRESS_NIBBLES],
    difficulty_nibbles: usize,
}

impl VanityPattern {
    pub fn new(prefix: &str, suffix: &str) -> Result<Self, String> {
        let prefix = parse_nibbles(prefix, "prefix")?;
        let suffix = parse_nibbles(suffix, "suffix")?;

        if prefix.is_empty() && suffix.is_empty() {
            return Err("choose at least one prefix or suffix character".to_owned());
        }

        let mut constraints = [None; ADDRESS_NIBBLES];
        for (index, nibble) in prefix.iter().copied().enumerate() {
            constraints[index] = Some(nibble);
        }

        let suffix_start = ADDRESS_NIBBLES - suffix.len();
        for (offset, nibble) in suffix.iter().copied().enumerate() {
            let index = suffix_start + offset;
            if let Some(prefix_nibble) = constraints[index] {
                if prefix_nibble != nibble {
                    return Err(format!(
                        "prefix and suffix conflict at address character {}",
                        index + 1
                    ));
                }
            }
            constraints[index] = Some(nibble);
        }

        let difficulty_nibbles = constraints.iter().flatten().count();
        Ok(Self {
            prefix,
            suffix,
            constraints,
            difficulty_nibbles,
        })
    }

    pub fn prefix(&self) -> String {
        format_nibbles(&self.prefix)
    }

    pub fn suffix(&self) -> String {
        format_nibbles(&self.suffix)
    }

    pub const fn difficulty_nibbles(&self) -> usize {
        self.difficulty_nibbles
    }

    pub fn expected_attempts(&self) -> String {
        let bits = self.difficulty_nibbles * 4;
        if bits < 128 {
            format_with_separators(1_u128 << bits)
        } else {
            format!("2^{bits}")
        }
    }

    pub fn matches(&self, address: &Address) -> bool {
        self.constraints
            .iter()
            .enumerate()
            .all(|(index, expected)| match expected {
                Some(expected) => address_nibble(address.as_bytes(), index) == *expected,
                None => true,
            })
    }

    pub(crate) fn packed_mask_value(&self) -> ([u32; 5], [u32; 5]) {
        let mut mask_bytes = [0_u8; 20];
        let mut value_bytes = [0_u8; 20];
        for (index, constraint) in self.constraints.iter().copied().enumerate() {
            let Some(nibble) = constraint else {
                continue;
            };
            let shift = if index.is_multiple_of(2) { 4 } else { 0 };
            mask_bytes[index / 2] |= 0x0f << shift;
            value_bytes[index / 2] |= nibble << shift;
        }

        let mut mask = [0_u32; 5];
        let mut value = [0_u32; 5];
        for index in 0..5 {
            let start = index * 4;
            mask[index] = u32::from_le_bytes(
                mask_bytes[start..start + 4]
                    .try_into()
                    .expect("four-byte mask word"),
            );
            value[index] = u32::from_le_bytes(
                value_bytes[start..start + 4]
                    .try_into()
                    .expect("four-byte value word"),
            );
        }
        (mask, value)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MiningOptions {
    pub start_counter: u64,
    pub max_attempts: Option<u64>,
    pub batch_size: u64,
}

impl Default for MiningOptions {
    fn default() -> Self {
        Self {
            start_counter: 0,
            max_attempts: None,
            batch_size: 262_144,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchProgress {
    pub candidates_checked: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub address: Address,
    pub salt: Salt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchOutcome {
    Found(SearchResult),
    Cancelled,
    NotFound { candidates_checked: u128 },
}

pub struct Create2Miner {
    deployer: Address,
    init_code_hash: [u8; 32],
    pattern: VanityPattern,
}

impl Create2Miner {
    pub fn new(deployer: Address, init_code: &[u8], pattern: VanityPattern) -> Self {
        Self {
            deployer,
            init_code_hash: keccak256(init_code),
            pattern,
        }
    }

    pub const fn init_code_hash(&self) -> &[u8; 32] {
        &self.init_code_hash
    }

    pub const fn deployer(&self) -> Address {
        self.deployer
    }

    pub const fn pattern(&self) -> &VanityPattern {
        &self.pattern
    }

    pub fn backend_session(
        &self,
        preference: BackendPreference,
    ) -> Result<BackendSession, BackendError> {
        BackendSession::new(self, preference)
    }

    pub fn search_with_backend(
        &self,
        session: &mut BackendSession,
        options: MiningOptions,
        cancelled: &AtomicBool,
        on_event: impl FnMut(SearchEvent),
    ) -> Result<SearchOutcome, BackendError> {
        backend_search(self, session, options, cancelled, on_event)
    }

    pub fn search(
        &self,
        options: MiningOptions,
        cancelled: &AtomicBool,
        mut on_progress: impl FnMut(SearchProgress),
    ) -> SearchOutcome {
        let mut session = BackendSession::new(self, BackendPreference::Cpu)
            .expect("the CPU backend is always available");
        backend_search(self, &mut session, options, cancelled, |event| {
            if let SearchEvent::Progress(progress) = event {
                on_progress(progress);
            }
        })
        .expect("the CPU backend cannot fail")
    }
}

pub fn salt_from_counter(counter: u64) -> Salt {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&counter.to_be_bytes());
    Salt::from_bytes(bytes)
}

pub fn create2_address(deployer: Address, salt: Salt, init_code: &[u8]) -> Address {
    create2_address_from_hash(deployer, salt, keccak256(init_code))
}

pub fn create2_address_from_hash(
    deployer: Address,
    salt: Salt,
    init_code_hash: [u8; 32],
) -> Address {
    let hash = create2_digest_from_hash(deployer, salt, init_code_hash);
    let mut address = [0_u8; 20];
    address.copy_from_slice(&hash[12..]);
    Address::from_bytes(address)
}

pub(crate) fn create2_digest_from_hash(
    deployer: Address,
    salt: Salt,
    init_code_hash: [u8; 32],
) -> [u8; 32] {
    let mut preimage = [0_u8; CREATE2_PREIMAGE_LEN];
    preimage[0] = 0xff;
    preimage[1..21].copy_from_slice(deployer.as_bytes());
    preimage[21..53].copy_from_slice(salt.as_bytes());
    preimage[53..].copy_from_slice(&init_code_hash);

    keccak256(&preimage)
}

pub fn keccak256(bytes: &[u8]) -> [u8; 32] {
    let mut output = [0_u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(bytes);
    hasher.finalize(&mut output);
    output
}

fn parse_nibbles(value: &str, label: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    if value.len() > ADDRESS_NIBBLES {
        return Err(format!(
            "{label} cannot exceed {ADDRESS_NIBBLES} hexadecimal characters"
        ));
    }

    value
        .bytes()
        .map(|byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(format!("{label} must contain only hexadecimal characters")),
        })
        .collect()
}

fn address_nibble(address: &[u8; 20], index: usize) -> u8 {
    let byte = address[index / 2];
    if index.is_multiple_of(2) {
        byte >> 4
    } else {
        byte & 0x0f
    }
}

fn format_nibbles(nibbles: &[u8]) -> String {
    nibbles
        .iter()
        .map(|nibble| char::from_digit(u32::from(*nibble), 16).expect("nibble is in range"))
        .collect()
}

fn format_with_separators(value: u128) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_address() -> Address {
        Address::from_bytes([0; 20])
    }

    fn zero_salt() -> Salt {
        Salt::from_bytes([0; 32])
    }

    #[test]
    fn eip_1014_example_0() {
        let address = create2_address(zero_address(), zero_salt(), &[0x00]);
        assert_eq!(
            address.to_string(),
            "0x4d1a2e2bb4f88f0250f26ffff098b0b30b26bf38"
        );
    }

    #[test]
    fn eip_1014_example_3() {
        let address = create2_address(
            zero_address(),
            zero_salt(),
            &hex::decode("deadbeef").unwrap(),
        );
        assert_eq!(
            address.to_string(),
            "0x70f2b2914a2a4b783faefb75f459a580616fcb5e"
        );
    }

    #[test]
    fn eip_1014_example_6() {
        let address = create2_address(zero_address(), zero_salt(), &[]);
        assert_eq!(
            address.to_string(),
            "0xe33c0c7f7df4809055c3eba6c09cfe4baf1bd9e0"
        );
    }

    #[test]
    fn counter_is_encoded_as_a_big_endian_bytes32() {
        let salt = salt_from_counter(1);
        assert_eq!(
            salt.to_string(),
            "0x0000000000000000000000000000000000000000000000000000000000000001"
        );
    }

    #[test]
    fn patterns_are_case_insensitive_and_ignore_optional_prefix_marker() {
        let pattern = VanityPattern::new("0x4D1A", "BF38").unwrap();
        let address: Address = "0x4d1a2e2bb4f88f0250f26ffff098b0b30b26bf38"
            .parse()
            .unwrap();
        assert!(pattern.matches(&address));
        assert_eq!(pattern.difficulty_nibbles(), 8);
    }

    #[test]
    fn overlapping_patterns_share_the_same_constraints() {
        let pattern = VanityPattern::new("1234567890abcdef1234567890abcdef123456", "5678").unwrap();
        assert_eq!(pattern.difficulty_nibbles(), 40);

        let error =
            VanityPattern::new("1234567890abcdef1234567890abcdef123456", "5778").unwrap_err();
        assert!(error.contains("conflict"));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(VanityPattern::new("", "").is_err());
        assert!(VanityPattern::new("xyz", "").is_err());
        assert!(VanityPattern::new(&"0".repeat(41), "").is_err());
    }

    #[test]
    fn search_returns_a_salt_that_recomputes_to_the_result() {
        let pattern = VanityPattern::new("4d1a", "bf38").unwrap();
        let miner = Create2Miner::new(zero_address(), &[0x00], pattern);
        let cancelled = AtomicBool::new(false);
        let outcome = miner.search(
            MiningOptions {
                start_counter: 0,
                max_attempts: Some(1),
                batch_size: 1,
            },
            &cancelled,
            |_| {},
        );

        let SearchOutcome::Found(result) = outcome else {
            panic!("counter zero should match the EIP-1014 vector");
        };
        assert_eq!(
            create2_address(zero_address(), result.salt, &[0x00]),
            result.address
        );
    }

    #[test]
    fn search_honors_range_limit_and_cancellation() {
        let impossible_in_one_try = VanityPattern::new("ffffffffff", "").unwrap();
        let miner = Create2Miner::new(zero_address(), &[0x00], impossible_in_one_try);
        let not_cancelled = AtomicBool::new(false);
        assert_eq!(
            miner.search(
                MiningOptions {
                    start_counter: 0,
                    max_attempts: Some(1),
                    batch_size: 64,
                },
                &not_cancelled,
                |_| {},
            ),
            SearchOutcome::NotFound {
                candidates_checked: 1
            }
        );

        let cancelled = AtomicBool::new(true);
        assert_eq!(
            miner.search(MiningOptions::default(), &cancelled, |_| {}),
            SearchOutcome::Cancelled
        );
    }

    #[test]
    fn search_covers_the_requested_counter_range_without_skipping() {
        let target_counter = 123;
        let target_salt = salt_from_counter(target_counter);
        let target_address = create2_address(zero_address(), target_salt, &[0x00]);
        let pattern = VanityPattern::new(&target_address.to_string(), "").unwrap();
        let miner = Create2Miner::new(zero_address(), &[0x00], pattern);
        let cancelled = AtomicBool::new(false);

        let found = miner.search(
            MiningOptions {
                start_counter: 120,
                max_attempts: Some(4),
                batch_size: 64,
            },
            &cancelled,
            |_| {},
        );
        let SearchOutcome::Found(result) = found else {
            panic!("counter 123 should be included in the range 120..=123");
        };
        assert_eq!(result.salt, target_salt);

        assert_eq!(
            miner.search(
                MiningOptions {
                    start_counter: 120,
                    max_attempts: Some(3),
                    batch_size: 64,
                },
                &cancelled,
                |_| {},
            ),
            SearchOutcome::NotFound {
                candidates_checked: 3
            }
        );
    }

    #[test]
    fn search_handles_the_final_u64_counter_without_overflow() {
        let zero_counter_address = create2_address(zero_address(), zero_salt(), &[0x00]);
        let pattern = VanityPattern::new(&zero_counter_address.to_string(), "").unwrap();
        let miner = Create2Miner::new(zero_address(), &[0x00], pattern);
        let cancelled = AtomicBool::new(false);

        assert_eq!(
            miner.search(
                MiningOptions {
                    start_counter: u64::MAX,
                    max_attempts: None,
                    batch_size: 64,
                },
                &cancelled,
                |_| {},
            ),
            SearchOutcome::NotFound {
                candidates_checked: 1
            }
        );
    }
}
