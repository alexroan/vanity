# vanity

`vanity` is an interactive, GPU-accelerated CREATE2 vanity-address miner for
Foundry projects. It builds the project, lets you choose a deployable contract
artifact, and searches for a `bytes32` salt that gives the requested address
prefix and suffix. It uses Metal on macOS, Vulkan on Linux, and Direct3D 12 on
Windows, with automatic Rayon CPU fallback.

## Prerequisites

- Rust 1.87 or newer
- [Foundry](https://getfoundry.sh/) with `forge` on `PATH`
- A Foundry project that builds successfully

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
GPU selection is automatic. Use `vanity --help` or `vanity --version` for
metadata.

For local development, invoke the checkout without installing it:

```sh
cd /path/to/foundry-project
cargo run --release \
  --manifest-path /path/to/vanity/Cargo.toml \
  --bin vanity -- --backend auto
```

Use a release build for realistic mining performance.

## Backend selection

The optional `--backend` flag accepts:

| Value | Behavior |
| --- | --- |
| `auto` | Select the highest-ranked hardware GPU with compute and native shader `u64`; permanently fall back to Rayon CPU if GPU initialization or a search dispatch fails. This is the default. |
| `gpu` | Require a compatible GPU. Initialization, shader, device, readback, witness, and result-validation failures are returned as errors. |
| `cpu` | Use Rayon and skip GPU initialization entirely. |

For example:

```sh
vanity --backend gpu
vanity --backend cpu
```

The search summary prints the selected backend, GPU adapter, and graphics API.
When `auto` falls back, it also prints the reason. GPU results and no-match
batches are checked against a CPU digest witness before the search advances.

The default build supports the native graphics API for each desktop target:

- macOS: Metal
- Linux: Vulkan (a working Vulkan loader and driver are required)
- Windows: Direct3D 12; x86-64 builds statically bundle DXC

Adapters must expose `SHADER_INT64`. Machines without that capability work in
`auto` mode through the CPU fallback.

To build a smaller CPU-only binary without `wgpu` or graphics-driver
initialization:

```sh
cargo build --release --no-default-features --bin vanity
```

That binary supports `auto` (which selects CPU) and `cpu`; explicit `gpu`
returns a clear feature-disabled error.

## Interactive flow

`vanity`:

1. Finds the nearest enclosing `foundry.toml` and runs `forge build`. A
   zero-config project without that file is recognized by Solidity sources in
   its default `src` directory.
2. Reads the configured artifact directory and lists artifacts with non-empty
   creation bytecode as `source/path.sol:ContractName`.
3. Reads Foundry's configured `create2_deployer` and uses it automatically.
4. Prompts for deployed addresses of any unresolved external libraries.
5. If needed, lists the constructor arguments and asks for each typed value
   individually. Every value is validated against its ABI type before the next
   prompt appears, then `vanity` performs the ABI encoding.
6. Offers example address prefixes and suffixes (`00`, `dead`, `cafe`, `beef`,
   and `babe`), `None`, or a custom hexadecimal value.
7. Shows the contract, deployer, pattern, expected attempts, init-code hash,
   and selected backend, then searches. Patterns of seven or more constrained
   hex characters require confirmation.
8. Prints the matching address, full `bytes32` salt, init-code hash, contract,
   and CREATE2 deployer.

Custom patterns may include `0x`, are case-insensitive, and can be any number
of hex characters up to the address's 40. At least one prefix or suffix
character is required. Press Ctrl-C to stop a search. Mining uses one backend
and one GPU at a time; CPU and GPU mining are not run simultaneously.

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

For a constructor such as
`constructor(address owner,uint256 amount)`, the CLI shows the complete list:

```text
Constructor arguments:
  1. owner: address
  2. amount: uint256
```

It then asks for `owner` and `amount` separately. Enter values in these forms:

- integers: decimal, such as `42` or `-7`
- addresses: 20-byte hex with `0x`, such as
  `0x1111111111111111111111111111111111111111`
- booleans: `true` or `false`
- dynamic and fixed bytes: `0x`-prefixed hex, such as `0x1234`
- arrays: `[1,2,3]`
- tuples: `(0x1111111111111111111111111111111111111111,42)`
- strings: the text itself

Tuple and array types are shown in canonical ABI form. Invalid values, including
out-of-range narrow integers and incorrectly sized addresses or fixed bytes,
are rejected at the current prompt. `vanity` ABI-encodes the complete validated
set internally; do not pre-encode the values with `cast`.

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
addresses, and the same constructor values. Rebuilding after a source,
compiler, optimizer, metadata, library, or constructor-value change can change
the init-code hash and therefore the deployment address.

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
later. Throughput depends on the GPU or CPU, build profile, graphics driver, and
system load, so start with a short pattern and use the live
candidates-per-second display to estimate a longer search.

## Development

Run the complete local verification set with:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --no-default-features
cargo build --release
cargo run -- --help
```

There is also a real interactive smoke test using a minimal Foundry fixture.
It requires `forge` and `expect`:

```sh
cargo build --bin vanity
expect tests/interactive-smoke.exp
```

The smoke test explicitly selects `--backend cpu`, so it is deterministic on
headless machines. Shader tests parse and validate WGSL with Naga and translate
it to MSL, SPIR-V, and HLSL.

Optional hardware conformance tests cover EIP-1014-compatible hashing,
workgroup boundaries, odd-nibble masks, no-match ranges, and CPU validation:

```sh
VANITY_GPU_TESTS=1 cargo test gpu::tests::optional_hardware_conformance
```

Use `VANITY_REQUIRE_GPU=1` instead when GPU absence must fail the test:

```sh
VANITY_REQUIRE_GPU=1 cargo test gpu::tests::optional_hardware_conformance
```

The release benchmark searches a fixed `2^22` no-match range, reports GPU cold
initialization separately, and requires at least 2× warm GPU throughput:

```sh
VANITY_REQUIRE_GPU=1 cargo bench --bench throughput
```

On the reference M2 Max, the current implementation measured 134.09M
candidates/s on Metal versus 29.19M with Rayon (4.59×), with 21ms cold GPU
initialization. Results vary with load and toolchain.

The complete tests also cover EIP-1014 CREATE2 vectors, salt encoding,
prefix/suffix mask packing, batch partitioning, fallback without skipped
counters, cancellation and range limits, GPU offset and digest-witness
validation, Foundry artifact discovery, constructor type handling, and library
linking.

Licensed under either MIT or Apache-2.0.
