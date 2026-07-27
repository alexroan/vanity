use crate::create2::{Create2Miner, MiningOptions, SearchOutcome, create2_address};
use crate::foundry::FoundryProject;
use crate::prompts;
use anyhow::{Context, Result, bail, ensure};
use indicatif::{ProgressBar, ProgressStyle};
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    if handle_meta_argument()? {
        return Ok(());
    }

    println!("vanity — CREATE2 address miner for Foundry\n");
    println!("Running forge build…");
    let current_dir = env::current_dir().context("could not determine the current directory")?;
    let project = FoundryProject::build(&current_dir)?;
    println!(
        "Found {} deployable contract artifact(s) in {}\n",
        project.contracts().len(),
        project.out_dir().display()
    );

    let selected = prompts::select_contract(project.contracts())?;
    let contract = &project.contracts()[selected];
    let deployer = project.create2_deployer();
    let required_libraries = contract.required_libraries()?;
    let libraries = prompts::prompt_libraries(&required_libraries)?;
    let constructor_arguments = prompts::prompt_constructor_arguments(contract)?;
    let init_code = contract.init_code(&libraries, &constructor_arguments)?;
    let pattern = prompts::prompt_vanity_pattern()?;
    let miner = Create2Miner::new(deployer, &init_code, pattern.clone());

    println!("\nSearch configuration");
    println!("  Contract: {}", contract.label());
    println!("  CREATE2 deployer: {deployer}");
    println!(
        "  Prefix: {}",
        display_pattern_part(&pattern.prefix(), "any")
    );
    println!(
        "  Suffix: {}",
        display_pattern_part(&pattern.suffix(), "any")
    );
    println!(
        "  Init code hash: 0x{}",
        hex::encode(miner.init_code_hash())
    );
    println!("  Expected attempts: about {}", pattern.expected_attempts());

    if !prompts::confirm_search(&pattern)? {
        println!("Search cancelled.");
        return Ok(());
    }

    println!();

    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
    })
    .context("could not install Ctrl-C handler")?;

    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .context("could not configure progress display")?
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    progress.enable_steady_tick(Duration::from_millis(80));
    progress.set_message("Mining… press Ctrl-C to stop");

    let started = Instant::now();
    let outcome = miner.search(MiningOptions::default(), &cancelled, |search_progress| {
        let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
        let checked = search_progress.candidates_checked;
        let rate = checked as f64 / elapsed;
        progress.set_message(format!(
            "Checked {} candidates ({}/s) — Ctrl-C to stop",
            format_count(checked),
            format_rate(rate)
        ));
    });
    progress.finish_and_clear();

    match outcome {
        SearchOutcome::Found(result) => {
            let recomputed = create2_address(deployer, result.salt, &init_code);
            ensure!(
                recomputed == result.address && pattern.matches(&result.address),
                "internal verification of the mined result failed"
            );

            println!("Found a matching CREATE2 deployment:\n");
            println!("Address: {}", result.address);
            println!("Salt:    {}", result.salt);
            println!("Init code hash: 0x{}", hex::encode(miner.init_code_hash()));
            println!("Contract: {}", contract.label());
            println!("CREATE2 deployer: {deployer}");
            Ok(())
        }
        SearchOutcome::Cancelled => bail!("search cancelled"),
        SearchOutcome::NotFound { candidates_checked } => {
            bail!(
                "no match found after checking {} candidates",
                format_count(candidates_checked)
            )
        }
    }
}

fn handle_meta_argument() -> Result<bool> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(false),
        [argument] if argument == "-h" || argument == "--help" => {
            print_help();
            Ok(true)
        }
        [argument] if argument == "-V" || argument == "--version" => {
            println!("vanity {}", env!("CARGO_PKG_VERSION"));
            Ok(true)
        }
        _ => {
            bail!("`vanity` is interactive and takes no arguments; try `vanity --help`")
        }
    }
}

fn print_help() {
    println!(
        "\
Interactive CREATE2 vanity address miner for Foundry projects

Usage:
  vanity

Run it anywhere inside a Foundry project. The tool runs `forge build`, lets you
select a deployable artifact, reads Foundry's configured CREATE2 deployer, asks
for the desired address prefix/suffix, then prints a matching address and
bytes32 salt.

The deployer comes from `forge config --json` and is the proxy Foundry uses for
salted script deployments."
    );
}

fn display_pattern_part(value: &str, empty: &str) -> String {
    if value.is_empty() {
        empty.to_owned()
    } else {
        format!("0x{value}")
    }
}

fn format_rate(rate: f64) -> String {
    if rate >= 1_000_000_000.0 {
        format!("{:.1}B", rate / 1_000_000_000.0)
    } else if rate >= 1_000_000.0 {
        format!("{:.1}M", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.1}K", rate / 1_000.0)
    } else {
        format!("{rate:.0}")
    }
}

fn format_count(value: u128) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rates_and_counts_for_progress_output() {
        assert_eq!(format_rate(999.0), "999");
        assert_eq!(format_rate(12_345.0), "12.3K");
        assert_eq!(format_rate(12_345_678.0), "12.3M");
        assert_eq!(format_count(123_456_789), "123,456,789");
    }
}
