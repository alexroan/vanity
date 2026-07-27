use crate::create2::{Address, VanityPattern};
use crate::foundry::{ContractArtifact, LibraryId};
use anyhow::{Context, Result};
use dialoguer::{Confirm, Input, Select};
use std::collections::BTreeMap;

pub fn select_contract(contracts: &[ContractArtifact]) -> Result<usize> {
    let labels = contracts
        .iter()
        .map(|contract| contract.label())
        .collect::<Vec<_>>();
    Select::new()
        .with_prompt("Select a contract to deploy")
        .items(&labels)
        .default(0)
        .interact()
        .context("contract selection was interrupted")
}

pub fn prompt_libraries(required: &[LibraryId]) -> Result<BTreeMap<LibraryId, Address>> {
    let mut libraries = BTreeMap::new();
    for library in required {
        let value: String = Input::new()
            .with_prompt(format!("Deployed address for library {library}"))
            .validate_with(|input: &String| input.parse::<Address>().map(|_| ()))
            .interact_text()
            .with_context(|| format!("library address input for {library} was interrupted"))?;
        let address = value
            .parse()
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("invalid address for library {library}"))?;
        libraries.insert(library.clone(), address);
    }
    Ok(libraries)
}

pub fn prompt_constructor_arguments(contract: &ContractArtifact) -> Result<Vec<u8>> {
    let constructor = contract.constructor();
    if !constructor.requires_arguments() {
        return Ok(Vec::new());
    }

    println!("\nThis contract requires {}.", constructor.signature());
    println!(
        "Encode the values with: cast abi-encode \"{}\" <values...>",
        constructor.cast_signature()
    );
    let value: String = Input::new()
        .with_prompt("Paste ABI-encoded constructor arguments")
        .validate_with(|input: &String| {
            let encoded = parse_hex_bytes(input, false)?;
            constructor
                .validate_arguments(&encoded)
                .map_err(|error| error.to_string())
        })
        .interact_text()
        .context("constructor argument input was interrupted")?;
    let encoded = parse_hex_bytes(&value, false).map_err(anyhow::Error::msg)?;
    constructor.validate_arguments(&encoded)?;
    Ok(encoded)
}

pub fn prompt_vanity_pattern() -> Result<VanityPattern> {
    loop {
        let prefix = prompt_pattern_part(PatternPart::Prefix)?;
        let suffix = prompt_pattern_part(PatternPart::Suffix)?;
        match VanityPattern::new(&prefix, &suffix) {
            Ok(pattern) => return Ok(pattern),
            Err(error) => eprintln!("\nThat combination cannot match: {error}\n"),
        }
    }
}

pub fn confirm_search(pattern: &VanityPattern) -> Result<bool> {
    if pattern.difficulty_nibbles() < 7 {
        return Ok(true);
    }

    Confirm::new()
        .with_prompt(format!(
            "This pattern needs about {} attempts on average. Start mining",
            pattern.expected_attempts()
        ))
        .default(true)
        .interact()
        .context("search confirmation was interrupted")
}

#[derive(Clone, Copy)]
enum PatternPart {
    Prefix,
    Suffix,
}

impl PatternPart {
    const fn label(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::Suffix => "suffix",
        }
    }

    const fn choices(self) -> [(&'static str, &'static str); 5] {
        match self {
            Self::Prefix => [
                ("00", "00 — example: two leading zeroes"),
                ("dead", "dead — example"),
                ("cafe", "cafe — example"),
                ("", "None"),
                ("__custom__", "Custom…"),
            ],
            Self::Suffix => [
                ("00", "00 — example: two trailing zeroes"),
                ("beef", "beef — example"),
                ("babe", "babe — example"),
                ("", "None"),
                ("__custom__", "Custom…"),
            ],
        }
    }
}

fn prompt_pattern_part(part: PatternPart) -> Result<String> {
    let choices = part.choices();
    let labels = choices.map(|(_, label)| label);
    let selection = Select::new()
        .with_prompt(format!("Choose an address {}", part.label()))
        .items(&labels)
        .default(0)
        .interact()
        .with_context(|| format!("{} selection was interrupted", part.label()))?;
    let value = choices[selection].0;

    if value != "__custom__" {
        return Ok(value.to_owned());
    }

    let custom: String = Input::new()
        .with_prompt(format!(
            "Enter a custom {} (hex after 0x, e.g. {})",
            part.label(),
            if matches!(part, PatternPart::Prefix) {
                "abc"
            } else {
                "123"
            }
        ))
        .validate_with(|input: &String| {
            let result = match part {
                PatternPart::Prefix => VanityPattern::new(input, ""),
                PatternPart::Suffix => VanityPattern::new("", input),
            };
            result.map(|_| ())
        })
        .interact_text()
        .with_context(|| format!("custom {} input was interrupted", part.label()))?;
    Ok(normalize_pattern(&custom))
}

fn normalize_pattern(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn parse_hex_bytes(value: &str, allow_empty: bool) -> std::result::Result<Vec<u8>, String> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    if !allow_empty && value.is_empty() {
        return Err("encoded constructor arguments cannot be empty".to_owned());
    }
    if !value.len().is_multiple_of(2) {
        return Err("hex input must contain a whole number of bytes".to_owned());
    }
    hex::decode(value).map_err(|_| "input contains non-hex characters".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_bytes_accept_optional_marker_and_reject_partial_bytes() {
        assert_eq!(parse_hex_bytes("0x1234", false).unwrap(), [0x12, 0x34]);
        assert!(parse_hex_bytes("123", false).is_err());
        assert!(parse_hex_bytes("0x", false).is_err());
    }

    #[test]
    fn custom_patterns_are_normalized_for_display() {
        assert_eq!(normalize_pattern(" 0xAbC "), "abc");
    }
}
