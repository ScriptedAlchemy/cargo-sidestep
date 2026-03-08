# cargo-sidestep prior art

Date: 2026-03-08

## Core finding

The strongest prior art says the same thing in different ways: share immutable inputs, isolate mutable build state.

There is no obvious well-known Cargo subcommand whose whole job is "detect a Cargo lock wait and transparently reroute". The ecosystem mostly relies on tool-specific target-dir separation and manual workarounds.

## Direct evidence of the problem

- [`rust-lang/cargo#11566`](https://github.com/rust-lang/cargo/issues/11566): concrete report of `Blocking waiting for file lock on package cache`.
- [`rust-lang/cargo#11924`](https://github.com/rust-lang/cargo/issues/11924): Cargo can wait on the package cache lock even for a crate with zero dependencies.
- [`rust-lang/cargo#6747`](https://github.com/rust-lang/cargo/issues/6747): Cargo should release its jobserver token while waiting on a file lock.
- [`rust-lang/cargo#15094`](https://github.com/rust-lang/cargo/issues/15094): request for Cargo to say which lock it is waiting on more clearly.

## Existing workaround patterns

- [`rust-lang/rust-analyzer#6007`](https://github.com/rust-lang/rust-analyzer/issues/6007): users asked for a separate `cargo.targetDir` to avoid blocking other Cargo commands.
- [`rust-lang/rust-analyzer#6589`](https://github.com/rust-lang/rust-analyzer/issues/6589): background Cargo activity from the editor contends with terminal workflows.
- [`emacs-lsp/lsp-mode#4506`](https://github.com/emacs-lsp/lsp-mode/issues/4506): same contention pattern in another editor integration.
- [`oxidecomputer/omicron`](https://github.com/oxidecomputer/omicron): project guidance recommends a dedicated rust-analyzer target dir.
- [`r3bl-org/r3bl-open-core`](https://github.com/r3bl-org/r3bl-open-core): documents per-tool target dirs such as `target/vscode` and `target/check`.
- [`DataDog/libdatadog`](https://github.com/DataDog/libdatadog): container builds set a dedicated `CARGO_TARGET_DIR` to avoid host/container collisions.
- [`CharmsDev/charms`](https://github.com/CharmsDev/charms): uses `CARGO_TARGET_DIR=$(mktemp -d)/target` for one-shot isolation during installs.
- [`leptos-rs/cargo-leptos`](https://github.com/leptos-rs/cargo-leptos): exposes `separate-front-target-dir = true` to avoid frontend/backend build interference.
- [`taiki-e/cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov): uses a dedicated build area for coverage runs.

## Cargo's direction

- [`rust-lang/cargo#5931`](https://github.com/rust-lang/cargo/issues/5931): proposes a per-user compiled artifact cache for immutable/idempotent artifacts with finer-grained locking.
- [`rust-lang/cargo#9455`](https://github.com/rust-lang/cargo/issues/9455): reinforces the idea that registry source cache content should be reused more safely.
- [`rust-lang/cargo#12207`](https://github.com/rust-lang/cargo/issues/12207): adjacent discussion around hashed per-script target/build directories.
- [`rust-lang/cargo#16147`](https://github.com/rust-lang/cargo/issues/16147): points toward moving `build-dir` into a shared cache home with workspace hashing.
- Cargo build-cache reference: [Build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)

## Design takeaways for this repo

- Do not share a mutable build directory between unrelated Cargo actors.
- Reuse immutable-ish caches where possible.
- Prefer reusable lanes over one-off temp dirs so rerouted builds can stay warm.
- Treat Cargo home differently from the build directory; the fallback strategy should be more conservative there.
- Cargo 1.91's `build.build-dir` is the right primitive for preserving final outputs while moving lock-sensitive intermediates.

## Naming

`cargo-sidestep` fits the behavior: it does not try to replace Cargo, distribute builds, or schedule work globally. It just sidesteps lock contention when it appears.
