# cargo-sidestep

`cargo-sidestep` is a Cargo wrapper that detects Cargo lock contention and reroutes the command into an isolated lane instead of waiting.

It keeps the fast path shared, but moves lock-sensitive state when Cargo prints `Blocking waiting for file lock ...`.

## What it does

- Uses a managed per-workspace state root instead of writing straight into the repository `target/`.
- On Cargo 1.91+, separates `CARGO_BUILD_BUILD_DIR` from `CARGO_TARGET_DIR` so lock-sensitive intermediates can move without throwing away final outputs.
- Watches Cargo stderr for lock-wait messages.
- If the lock is the build directory, retries in a reusable build lane.
- If the lock is in Cargo home (`package cache`, `registry index`, `git db`), retries with an overlay `CARGO_HOME`:
  - first a readonly/offline overlay that reuses cached crate archives
  - then a fully isolated online overlay if the readonly pass does not have enough cached state

## Why this exists

The prior art is consistent: the ecosystem already works around Cargo lock contention by moving target/build state for specific tools, but there is not an obvious general-purpose wrapper that does the reroute automatically.

`cargo-sidestep` packages that workaround into one command.

## Install

```bash
cargo install --path .
```

Or run the built binary directly:

```bash
./target/debug/cargo-sidestep check
```

## Usage

Standalone:

```bash
cargo-sidestep build --workspace
```

As a Cargo subcommand:

```bash
cargo sidestep test -p my_crate
```

With an explicit manifest:

```bash
cargo sidestep check --manifest-path ./crates/api/Cargo.toml
```

## Environment

- `CARGO_SIDESTEP_STATE_DIR`: override the cache root used for managed targets, build dirs, and lanes.
- `CARGO_SIDESTEP_FALLBACK_AFTER_MS`: milliseconds to wait after Cargo prints a lock-wait message before rerouting. Default: `1500`.
- `CARGO_SIDESTEP_LANES`: number of reusable fallback lanes per workspace. Default: `4`.
- `CARGO_SIDESTEP_CARGO_BIN`: override the underlying Cargo executable. Mainly useful for tests.

## Current behavior

- Final build outputs live under the sidestep-managed target root, not the repository `./target`.
- Fallback lane directories are reused across invocations, so repeated reroutes can stay warm.
- The readonly overlay intentionally shares only conservative cache material from `CARGO_HOME`.

## Limitations

- This is an initial implementation and is deliberately conservative about what it shares from `CARGO_HOME`.
- Some tools assume outputs always live in `./target`; those workflows need to respect `CARGO_TARGET_DIR`.
- Lock detection is stderr-driven because Cargo does not expose a better stable signal for this yet.
