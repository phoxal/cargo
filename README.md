# cargo-phoxal

`cargo-phoxal` is the canonical Phoxal source-development command.

This repository owns the project compiler and command implementation as private modules of its binary.
It consumes the published framework SDK, shared bundle records, and application contracts.
Linux and macOS are supported.
Windows and other operating systems are unsupported and unqualified.

## Installation

Install the released binary from crates.io:

```sh
cargo install cargo-phoxal
```

Cargo exposes it as `cargo phoxal`.
Ordinary build, check, run, and test commands need no `--locked` flag.
Advanced `--locked`, `--frozen`, and `--offline` options remain available.
Managed source-package installs use `--locked` internally to request the package's tested lockfile; CI uses it to check committed locks.
Package versions are independent, and compatibility follows the actual interfaces consumed.

For tool development, run this repository's binary explicitly:

```sh
cargo run -- phoxal --help
```

The tooling supports `cargo phoxal prepare`, `cargo phoxal check`, `cargo phoxal build`, `cargo phoxal run`, `cargo phoxal test`, simulation preparation and delegation.

`cargo phoxal prepare` resolves each service and component from its required `source` selection in `robot.yaml`.
Choose exactly one source form:

```yaml
source: { path: ../../components/ddsm115 }
source:
  git:
    name: phoxal-component-ddsm115
    url: https://example.com/components.git
    rev: 0123456789abcdef0123456789abcdef01234567
    path: ddsm115
```

Local paths are relative to the robot root and use the selected Cargo package's own version.
Git selections require a package name and full commit revision, with an optional package path below the checkout; Cargo reads that package's version from the pinned checkout.
Pinned Git runnable packages are installed into the managed Phoxal home.
Passive components resolve through the same authored source forms without becoming robot Rust dependencies.
Their declared model resources are retained for offline preparation; local passive components need no Rust target.
It extracts compiled endpoint records and schemas into the project's ignored `.phoxal/` tree.
Managed installations retain the selected binary and any component model resources needed for composition.
Local path participants are built by Cargo from their own package manifest when a runtime bundle is needed.
Preparation preserves the authored selection and leaves the robot's Cargo manifest unchanged.
The check, build, run, test, bundle, and simulation entry points prepare these exact selections automatically.

Each command discovers the nearest robot project, validates explicit composition, and applies the requested Cargo lock and offline policy.
Pinned Git runnable participant packages install through Cargo into the managed Phoxal home outside the robot's dependency graph.
The root Cargo graph contains the robot application and its genuine Rust library dependencies.

`cargo phoxal check` prepares exact selections, builds the brain, validates compiled contracts and connections, then checks the robot code.
For differently typed latest observations, the single generated API attaches normal brain endpoints and executes the robot's ordinary `From` or `TryFrom` conversion during that runtime's invocation.
Conversions preserve the producer's capture stamp and the consumer's freshness and byte bounds.

`cargo phoxal build` assembles the selected brain, service, and component-driver executables into a deterministic bundle under Cargo's target directory by default, or at `--output <directory>`.
The bundle carries one typed resolved manifest (`manifest.json`, schema `phoxal/bundle/v0`) as the sole description of the composition.
Executables use bundle-local IDs derived from their complete source/build selections.
Every instance of a repeated driver shares one stored executable.
The manifest records the supervisor executable path, every instance with its role and resolved configuration, the typed connection graph between instance endpoints, the compiled runtime contracts, and the model and snapshot facts for simulation bundles.
Simulation bundles also contain the model assets needed by the simulator.

`cargo phoxal run` independently prepares and validates the hardware bundle, then launches the selected supervisor with the isolated `local` scope and `local` supervisor identity and an explicit execution-state directory.
It does not launch simulation or claim domain readiness or physical safety.

Change a Git participant's revision in `robot.yaml`, then run preparation or build to acquire it.
Local path packages use their current checked-out content.

All source-development commands accept `--cargo <path>` and preserve the selected executable across Cargo metadata and operation invocations.
Cargo package, workspace, target, and test selectors are forwarded using Cargo's native option names.
In JSON compiler-message mode, compiler JSON remains on stdout and Phoxal progress and structured project diagnostics remain on stderr.

The root project declares its ordinary `phoxal` SDK dependency; the supervisor is not a robot dependency.
The required `supervisor.source` in `robot.yaml` uses the same local path or pinned Git selection as participants, with an optional binary selector.
The tool builds local sources or acquires the selected release, caches it by the complete source/build selection, and validates its target and supported interface revisions from portable embedded metadata.
Package versions are provenance and release selections; compatible independently versioned supervisors work with the same tool.
Incompatible selections fail without fallback, preserving existing usable installations and bundles.
Foreign-target metadata is inspected without executing the foreign binary.
Preparation never adds per-participant Cargo dependencies or rewrites `Cargo.toml`.
Every selected runtime executable must expose its exact compiled contract metadata, and authored configuration is checked against that metadata before a bundle is published.

## Simulation

Install the independently versioned application:

```sh
cargo install phoxal-simulator
```

From robot-rover, one command prepares the source project and opens its visible desktop simulation:

```sh
cargo phoxal simulation project
```

The simulator owns this convenience command and invokes the public `cargo phoxal build --simulation-scene simulation/scene.xml` preparation boundary.
The tool remains a thin proxy for every `simulation` argument, standard stream, interruption, and exit status.
The simulator owns native prerequisites, scene execution, supervisor launch and cleanup, desktop controls, and terminal/scenario evidence.
MuJoCo is user-managed and dynamically loaded only for native operations; see the [simulator README](https://github.com/phoxal/simulator#readme) for discovery and controls.

For an explicitly prepared bundle:

```sh
cargo phoxal build --simulation-scene simulation/scene.xml --output /tmp/rover-simulation
cargo phoxal simulation run --bundle /tmp/rover-simulation --scene /tmp/rover-simulation/scene/scene.xml --desktop --steps 10000 --auto-run
```

Scenario tests retain robot preparation in the tool and delegate the native run and evidence to the simulator.

## Testing

This is one root Cargo binary package, with private unit tests beside their implementation.
Integration assertions in `tests/` spawn Cargo's current `CARGO_BIN_EXE_cargo-phoxal` binary.
Shared test helpers live under `tests/support/`, and sample projects under `tests/fixtures/` are copied into isolated directories.
Acquisition tests retain real isolated local Git boundaries; the former acquisition-only helper package is gone.
Run `cargo test` for deterministic repository coverage.
Run `cargo test --features host-acceptance --test host_acquisition` for the separate cold-cache acquisition lane.
No integration suite imports private tool implementation modules.

## Releases

Versions and generated changelogs are prepared by release-plz, and tested revisions publish to crates.io.
Owners are published before dependent consumers.
Application package versions are independent of SDK versions.
Normal builds and installations use public dependencies without sibling checkouts or local patches.
