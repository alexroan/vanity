# vanity-create2

`vanity` is an interactive, CPU-parallel CREATE2 vanity-address miner for
Foundry projects. It builds the project, lets you choose a deployable contract
artifact, and searches for a `bytes32` salt that gives the requested address
prefix and suffix.

## Prerequisites

- Rust 1.85 or newer
- [Foundry](https://getfoundry.sh/) with `forge` on `PATH`
- A Foundry project that builds successfully
- The address of the contract or factory that will execute `CREATE2`

`cast` is also useful for encoding constructor arguments; the CLI prints the
exact command when the selected constructor needs them.

## Install and run

From this repository:

```sh
cargo install --path . --bin vanity --locked
```

Make sure Cargo's binary directory (normally `~/.cargo/bin`) is on `PATH`.
Then, from inside the Foundry project whose contract you want to deploy:

```sh
vanity
```

The command is intentionally interactive and takes no positional arguments.
Use `vanity --help` or `vanity --version` for metadata.

For local development, invoke the checkout without installing it:

```sh
cd /path/to/foundry-project
cargo run --release \
  --manifest-path /path/to/vanity-create2/Cargo.toml \
  --bin vanity
```

Use a release build for realistic mining performance.

## Interactive flow

`vanity`:

1. Finds the nearest enclosing `foundry.toml` and runs `forge build`. A
   zero-config project without that file is recognized by Solidity sources in
   its default `src` directory.
2. Reads the configured artifact directory and lists artifacts with non-empty
   creation bytecode as `source/path.sol:ContractName`.
3. Prompts for the **CREATE2 deployer address**.
4. Prompts for deployed addresses of any unresolved external libraries.
5. If needed, shows the constructor signature and asks for its ABI-encoded
   arguments.
6. Offers example address prefixes and suffixes (`00`, `dead`, `cafe`, `beef`,
   and `babe`), `None`, or a custom hexadecimal value.
7. Shows the contract, deployer, pattern, expected attempts, and init-code hash,
   then searches all CPU cores. Patterns of seven or more constrained hex
   characters require confirmation.
8. Prints the matching address, full `bytes32` salt, init-code hash, contract,
   and CREATE2 deployer.

Custom patterns may include `0x`, are case-insensitive, and can be any number
of hex characters up to the address's 40. At least one prefix or suffix
character is required. Press Ctrl-C to stop a search.

## The deployer is not usually the EOA

CREATE2 derives an address as:

```text
last20(keccak256(0xff ++ deployer ++ salt ++ keccak256(init_code)))
```

Here, `deployer` is the address of the contract whose code executes the
`CREATE2` opcode (`address(this)` at that point). It is not the EOA that sends a
transaction to that contract. If an EOA calls a factory, enter the factory
address. A proxy, singleton deployer, or nested factory must likewise be
represented by the address that actually executes `CREATE2`.

Some factories transform or namespace a user-provided salt before calling
`CREATE2`. The value printed by `vanity` is the actual 32-byte opcode salt. Do
not pass it through a factory that hashes or otherwise changes it unless the
factory API lets the opcode receive that exact value.

## Constructor arguments and libraries

The init code being mined is:

```text
linked artifact creation bytecode ++ ABI-encoded constructor arguments
```

For a constructor such as `constructor(address,uint256)`, the prompt suggests:

```sh
cast abi-encode "args(address,uint256)" 0x1111111111111111111111111111111111111111 42
```

Paste the resulting hex bytes into `vanity`. Use `cast abi-encode`, not calldata
with a four-byte function selector. The bytes are checked against the
constructor's ABI before mining. Tuple and array types are shown in their
canonical ABI form by the CLI.

When an artifact contains unresolved link references, `vanity` asks for each
library as `source/path.sol:LibraryName` and patches that deployed address into
the creation bytecode. These must be the library addresses used by the real
deployment on the target chain.

## Deploying the result safely

The returned address is valid only for the exact tuple:

```text
(CREATE2 deployer, bytes32 salt, init code)
```

Use the printed salt byte-for-byte. The real deployment must use the same
contract artifact and compiler/build settings, the same linked library
addresses, and the same ABI-encoded constructor values. Rebuilding after a
source, compiler, optimizer, metadata, library, or constructor-value change can
change the init-code hash and therefore the deployment address.

Before broadcasting, recompute or compare `keccak256(init_code)` with the
`Init code hash` printed by `vanity`. A generic factory should receive those
exact init-code bytes and the printed salt. With Solidity's
`new Target{salt: salt}(...)`, ensure the factory executing that expression was
compiled with bytecode and library links identical to the artifact mined here.

## Search difficulty

Each independently constrained hexadecimal character multiplies the expected
work by 16:

| Constrained hex characters | Expected attempts |
| ---: | ---: |
| 2 | 256 |
| 4 | 65,536 |
| 6 | 16,777,216 |
| 7 | 268,435,456 |
| 8 | 4,294,967,296 |

Prefix and suffix characters both count; compatible overlap counts only once.
These are averages, not deadlines—a search can finish much sooner or much
later. Throughput depends on the CPU, build profile, and system load, so start
with a short pattern and use the live candidates-per-second display to estimate
a longer search.

## Development

Run the complete local verification set with:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
cargo run -- --help
```

There is also a real interactive smoke test using a minimal Foundry fixture.
It requires `forge` and `expect`:

```sh
cargo build --bin vanity
expect tests/interactive-smoke.exp
```

The tests cover EIP-1014 CREATE2 vectors, salt encoding, prefix/suffix matching,
search cancellation and limits, Foundry artifact discovery, constructor type
handling, and library linking.

Licensed under either MIT or Apache-2.0.
