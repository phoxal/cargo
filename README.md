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

The tooling supports `cargo phoxal prepare`, `cargo phoxal check`, `cargo phoxal build`, `cargo phoxal run`, `cargo phoxal test`, `cargo phoxal simulation <scene-file>`, and `cargo phoxal scenario <file>`.

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
It extracts compiled endpoint records and schemas under the normal Cargo target root, in `phoxal/prepared/<project-identity>/`.
Preparation and ordinary Cargo/build-script/IDE reads share the same local Cargo configuration and environment.
One-off `--target-dir` or command-line `--config` overrides relocate compiler and runnable outputs only; they do not relocate prepared inputs.
After changing normal target configuration or environment, run `cargo phoxal prepare` again.
Existing project-side `.cargo` directories track config additions, edits, legacy filename precedence, and deletion without forcing unchanged warm builds.
When adding a previously absent `.cargo` directory and reusing an existing compiler-output cache, prepare the new input store and clean only the robot package:

```sh
cargo phoxal prepare
cargo clean -p <ROBOT_PACKAGE> --target-dir <AFFECTED_COMPILER_OUTPUT>
```

Retain matching `--target <TRIPLE>` if applicable; omit `--target-dir` when normal configuration already selects the affected output directory.
Then build normally; do not clean dependencies or every historical target directory.
Existing global Cargo config files and environment changes are tracked, but adding a previously absent global config file requires the same recovery because Cargo home also contains mutable caches.
Managed installations retain the selected binary and any component model resources needed for composition.
Local path participants are built by Cargo from their own package manifest when a runtime bundle is needed.
Preparation preserves the authored selection and leaves the robot's Cargo manifest unchanged.
The check, build, run, test, simulation, and scenario entry points prepare these exact selections automatically.

Each command discovers the nearest robot project, validates explicit composition, and applies the requested Cargo lock and offline policy.
Pinned Git runnable participant packages install through Cargo into the managed Phoxal home outside the robot's dependency graph.
The root Cargo graph contains the robot application and its genuine Rust library dependencies.

Author delivery edges as explicit records:

```yaml
connections:
  - from: motion.front_left_actuator
    to: front_left_drive.actuator
```

Endpoint spelling stays `instance.endpoint`.
Compiled contracts determine producer/consumer and request/reply roles; repeated destinations retain fan-in where that input contract supports it.

`cargo phoxal check` prepares exact selections, builds the brain, validates compiled contracts and connections, then checks the robot code.
For differently typed latest observations, the single generated API attaches normal brain endpoints and executes the robot's ordinary `From` or `TryFrom` conversion during that runtime's invocation.
Conversions preserve the producer's capture stamp and the consumer's freshness and byte bounds.

`cargo phoxal build` compiles in release mode, assembles the selected brain, service, and component-driver executables under Cargo's target directory, and writes `bundle/<robot-id>.zip`.
Use `--output <file.zip>` to select another archive destination.
The runnable directory is separated by target and profile; the archive contains relative paths and preserves executable permissions.
The bundle carries one typed resolved manifest (`manifest.json`, schema `phoxal/bundle/v0`) as the sole description of the composition.
Executables use bundle-local IDs derived from their complete source/build selections.
Every instance of a repeated driver shares one stored executable.
The manifest records the supervisor executable path, every instance with its role and resolved configuration, the typed connection graph between instance endpoints, and the compiled runtime contracts.
The common runnable build contains the model assets needed by the simulator and has no separate simulation manifest variant.
Native probe facts and execution state are command-owned temporary inputs outside that build.

`cargo phoxal run` independently prepares and validates the hardware bundle, then launches the selected supervisor with the isolated `local` scope and `local` supervisor identity and an explicit execution-state directory.
It does not launch simulation or claim domain readiness or physical safety.

Change a Git participant's revision in `robot.yaml`, then run preparation or build to acquire it.
Local path packages use their current checked-out content.

All source-development commands accept `--cargo <path>` and preserve the selected executable across Cargo metadata and operation invocations.
Cargo package, workspace, and test-target selectors apply only to `check` and `test`; whole-robot commands always use the authored graph.
Whole-robot commands do not accept an arbitrary Cargo argument tail.
`test -- <arguments>` passes ordinary Rust test-harness arguments.
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

From robot-rover, select the scene explicitly:

```sh
cargo phoxal simulation simulation/scene.xml
```

The tool prepares a development build and delegates native execution, controls, cleanup, and terminal evidence to the simulator.
The validated scene closure is stored in the runnable build and used for execution.
Simulation does not create or extract a ZIP.
Use `--release` for release compilation or `--paused` for paused desktop startup.
The desktop has no arbitrary step-count limit.
For a finite run without a window:

```sh
cargo phoxal simulation simulation/scene.xml --headless --duration 10s
```

An existing runnable directory bypasses robot discovery, Cargo, and source acquisition:

```sh
cargo phoxal simulation /path/to/scene.xml --build /path/to/build --headless --duration 250ms
```

`--build` cannot be combined with `--release`.
The explicitly selected external scene still undergoes simulator-owned native admission.
MuJoCo is user-managed; see the [simulator README](https://github.com/phoxal/simulator#readme) for discovery and controls.

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

## Command choices

| Command | Purpose | Defaults and key options |
| --- | --- | --- |
| `prepare` | Prepare selected contracts and generated APIs | Authored graph; advanced acquisition controls |
| `check` | Check selected Rust code and validate composition | Cargo package/target selectors |
| `test` | Run ordinary Rust tests | Cargo selectors; `--` for test-harness arguments |
| `build` | Assemble and archive the complete robot | Release; `-o, --output <ZIP_FILE>` |
| `run` | Execute the robot with real device drivers | Development; `--release` |
| `simulation <SCENE_FILE>` | Run native physics | Development and desktop; `--release`, `--paused`, or `--headless --duration 10s` |
| `scenario <SCENARIO_FILE>` | Execute typed behavior assertions | Headless; `--desktop`, `--release` |

Simulation `--build <BUILD_DIR>` consumes an existing build without Cargo or source acquisition and conflicts with build/acquisition controls.
Explicit command paths resolve relative to the invocation directory, including invocation from a project subdirectory.
Authored paths in `robot.yaml` and resources selected by scenario code remain relative to the robot project root.
Scenarios are ordinary Cargo-declared examples or binaries under `scenarios/`, with `test = false` and an explicit scene.
They do not masquerade as ordinary Rust tests requiring a command-scoped host.
