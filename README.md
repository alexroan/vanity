# vanity-create2

`vanity` is an interactive, CPU-parallel CREATE2 vanity-address miner for
Foundry projects. It builds the project, lets you choose a deployable contract
artifact, and searches for a `bytes32` salt that gives the requested address
prefix and suffix.

## Prerequisites

- Rust 1.85 or newer
- [Foundry](https://getfoundry.sh/) with `forge` on `PATH`
- A Foundry project that builds successfully

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
3. Reads Foundry's configured `create2_deployer` and uses it automatically.
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

## Foundry's CREATE2 deployer

CREATE2 derives an address as:

```text
last20(keccak256(0xff ++ deployer ++ salt ++ keccak256(init_code)))
```

For salted contract creation in a Forge script broadcast, Foundry routes the
deployment through its configured deterministic deployment proxy. `vanity`
reads that address from the same resolved configuration returned by
`forge config --json`; it does not ask the user to enter an address. Foundry's
default is:

```text
0x4e59b44847b379578588920ca78fbf26c0b4956c
```

Projects and environments can override that setting. Inspect the effective
value with:

```sh
forge config --json | jq -r '.create2_deployer'
```

The deployer in the formula is the contract whose code executes the `CREATE2`
opcode, not the broadcasting EOA. This automatic selection is intended for
Forge script deployments routed through Foundry's configured proxy, including
tests that execute the deployment script when that proxy is available.

It is not the right deployer for an ordinary Solidity test that directly runs
`new Target{salt: salt}()`, or for an application-specific factory that executes
`CREATE2` itself. Custom factories may also transform or namespace the supplied
salt. The value printed by `vanity` is the raw 32-byte salt expected by
Foundry's deterministic deployment proxy. Ordinary tests use the test or
calling contract as the CREATE2 deployer unless
`always_use_create_2_factory = true`.

Foundry's two-argument `vm.computeCreate2Address(salt, initCodeHash)` helper
always computes with the canonical `0x4e59...` proxy. If a project overrides
`create2_deployer`, tests should use the three-argument overload and pass the
configured address explicitly.

Mining is offline and does not verify chain state. Before broadcasting to a
public chain, ensure the configured proxy and expected proxy bytecode exist at
that address. If the target chain does not support Foundry's default proxy,
deploy a compatible proxy or configure the correct supported deployer before
mining. Foundry can install the canonical proxy in a fresh local test
environment, but a custom deployer or fork must already contain the proxy code.
For the canonical proxy, check the target RPC with:

```sh
cast codehash 0x4e59b44847b379578588920ca78fbf26c0b4956c \
  --rpc-url "$RPC_URL"
```

The expected runtime code hash is
`0x2fa86add0aed31f33a762c9d88e807c475bd51d0f52bd0955754b2608f7e4989`.

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
`Init code hash` printed by `vanity`. Foundry's proxy must receive those exact
init-code bytes and the printed salt. With Solidity's
`new Target{salt: salt}(...)` in a Forge script broadcast, confirm the deployment
is routed through the configured CREATE2 proxy printed by `vanity`.

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
