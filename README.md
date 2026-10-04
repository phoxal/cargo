# cargo-phoxal

`cargo-phoxal` is the canonical Phoxal source-development command.

This repository owns the project compiler and command implementation as private modules of its binary.
It consumes the published framework SDK, shared bundle records, and application contracts.
Linux and macOS are supported.
Windows and other operating systems are unsupported and unqualified.

## Installation

Install or update the released package from the Phoxal registry:

```sh
cargo install cargo-phoxal \
  --index sparse+https://phoxal.github.io/registry/ \
  --version '<exact released version>' \
  --locked
```

The source-install fallback uses `--locked` because Cargo install otherwise resolves dependencies afresh rather than using the package’s tested lockfile.
Ordinary build, check, run, and test commands reuse their lockfile without requiring that flag.
`--locked`, `--frozen`, and `--offline` remain available for deliberate reproducibility and network constraints.


Cargo requires an explicit version when installing a pre-release.
Select an exact release available in the registry.

Cargo exposes the installed binary as `cargo phoxal`.
Publishing a new tool version requires its exact `phoxal` dependency to be available in the registry first.
Owner contracts are published before consumers that need a changed interface; implementation-only releases remain independent.

For tool development, run this repository's binary explicitly:

```sh
cargo run -- phoxal --help
```

The tooling supports `cargo phoxal prepare`, `cargo phoxal check`, `cargo phoxal build`, `cargo phoxal run`, `cargo phoxal test`, managed simulation installation and execution, and reviewed package publication.

`cargo phoxal prepare` resolves each service and component from its required `source` selection in `robot.yaml`.
Choose exactly one source form:

```yaml
source: { path: ../../components/ddsm115 }
source: { package: { name: phoxal-component-ddsm115, version: "1.2.3" } }
source: { package: { name: phoxal-component-ddsm115, version: "1.2.3", registry: other } }
source:
  git:
    name: phoxal-component-ddsm115
    url: https://example.com/components.git
    rev: 0123456789abcdef0123456789abcdef01234567
    path: ddsm115
```

Local paths are relative to the robot root and use the selected Cargo package's own version.
Registry selections require an exact package name and semantic version; omitting `registry` selects the Phoxal registry.
Git selections require a package name and full commit revision, with an optional package path below the checkout; Cargo reads that package's version from the pinned checkout.
Registry and Git runnable packages are installed into the managed Phoxal home.
Passive components resolve through the same authored source forms without becoming robot Rust dependencies.
Their declared model resources are retained for offline preparation; local passive components need no Rust target.
It extracts compiled endpoint records and schemas into the project's ignored `.phoxal/` tree.
Managed installations retain the selected binary and any component model resources needed for composition.
Local path participants are built by Cargo from their own package manifest when a runtime bundle is needed.
Preparation preserves the authored selection and leaves the robot's Cargo manifest unchanged.
The check, build, run, test, bundle, and simulation entry points prepare these exact selections automatically.

Each command discovers the nearest robot project, validates explicit composition, and applies the requested Cargo lock and offline policy.
Registry and Git runnable participant packages install through Cargo into the managed Phoxal home outside the robot's dependency graph.
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

Change a registry participant's version or a Git participant's revision in `robot.yaml`, then run preparation or build to acquire it.
Local path packages use their current checked-out content.

All source-development commands accept `--cargo <path>` and preserve the selected executable across Cargo metadata and operation invocations.
Cargo package, workspace, target, and test selectors are forwarded using Cargo's native option names.
In JSON compiler-message mode, compiler JSON remains on stdout and Phoxal progress and structured project diagnostics remain on stderr.

The root project declares its ordinary `phoxal` SDK dependency; the supervisor is not a robot dependency.
The required `supervisor.source` in `robot.yaml` uses the same path, exact registry package, or pinned Git selection as participants, with an optional binary selector.
The tool builds local sources or acquires the selected release, caches it by the complete source/build selection, and validates its target and supported interface revisions from portable embedded metadata.
Package versions are provenance and release selections; compatible independently versioned supervisors work with the same tool.
Incompatible selections fail without fallback, preserving existing usable installations and bundles.
Foreign-target metadata is inspected without executing the foreign binary.
Preparation never adds per-participant Cargo dependencies or rewrites `Cargo.toml`.
Every selected runtime executable must expose its exact compiled contract metadata, and authored configuration is checked against that metadata before a bundle is published.

## Simulation installation

Install the supported MuJoCo distribution and the matching released simulator with:

```sh
cargo phoxal simulation install
cargo phoxal simulation status
```

The tool acquires the independently released `phoxal-simulator-install` application on first use or explicit upgrade.
Cargo resolves the current installer release, including prereleases; `--installer-version` selects an exact release.
An installed compatible installer and simulator are reused.
The simulator-owned installer verifies native archive checksums, uses ordinary Cargo source-package installation, packages native libraries and licenses, and selects a validated candidate.
Failed or incompatible upgrades preserve the previous application.
The tool checks embedded interface revisions and target before launching applications; package versions are provenance rather than compatibility gates.
Source development can select `PHOXAL_SIMULATOR_INSTALLER=/path/phoxal-simulator-install` with the same interface validation.
On macOS the result is a self-contained locally signed application bundle.
On Linux the executable is linked to the managed native distribution with an explicit runtime search path.
Robot manifests never depend on MuJoCo or the simulator.

Use `cargo phoxal simulation upgrade` to replace the managed installation and `cargo phoxal simulation uninstall` to remove only the directory marked as owned by `cargo-phoxal`.
Use `--mujoco-distribution <path>` when an official MuJoCo distribution is already available, or together with `--offline` for an installation that performs no download.
An explicit `--simulator <path>` remains available for simulator source development and deterministic test fixtures.

`cargo phoxal publish <role> <name> --dry-run` selects an exact local Cargo package and produces a `.crate` archive in isolated temporary staging.
The command prints the archive checksum and size without creating a review inventory, source provenance record, or checksum sidecar.
Supported roles are `component`, `service`, `library`, `proc-macro`, `simulator`, `application`, and `tool`.

The developer selects the publication role in the command instead of repeating it in package metadata.
`cargo-phoxal` verifies that selection from standard project structure: `component.yaml` identifies a component, and Cargo target shape distinguishes runnable services, libraries, procedural macros, applications, simulators, and tools.
Runtime composition derives service and component roles from `robot.yaml` source selections.
No Phoxal-specific package metadata table is required.

The optional `--path <source-directory>` selects a package explicitly, while omitting it selects the matching current package or a uniquely named member of the current Cargo workspace.

Passive components may contain only authored Cargo package metadata, a `component.yaml`, and declared assets.
The publication preparer adds `_cargo/lib.rs` and technical Cargo packaging fields only in staging, and never writes generated files into the authored source tree.

Omit `--dry-run` to submit those exact bytes to `phoxal/registry` for review.
Submission uses GitHub HTTPS APIs directly and invokes neither Git nor GitHub CLI.
Set `PHOXAL_GITHUB_TOKEN` for an explicit noninteractive credential, keeping it outside command arguments and submitted content.
Otherwise the released tool uses its embedded public OAuth client ID, obtains the `public_repo` scope through GitHub's bounded device flow, validates the authenticated account, and stores renewable credentials in the operating-system credential store.
Development builds may provide the public client ID through `PHOXAL_GITHUB_CLIENT_ID`.
The `public_repo` scope covers every public repository accessible to the account, not only the registry fork.
The command creates or reuses a contributor fork and immutable publication branch, uploads the archive and Cargo index bytes through Git objects, opens or reuses the upstream pull request, and returns `pending-review` without waiting for merge.
An already deployed version is reported as `available` only after its public archive checksum is verified.

## Testing

This is one root Cargo binary package, with private unit tests beside their implementation.
Integration assertions in `tests/` spawn Cargo's current `CARGO_BIN_EXE_cargo-phoxal` binary.
Shared test helpers live under `tests/support/`, and sample projects under `tests/fixtures/` are copied into isolated directories.
Acquisition tests retain real local Git and registry boundaries; the former acquisition-only helper package is gone.
Run `cargo test` for deterministic repository coverage.
Run `cargo test --features host-acceptance --test host_acquisition` for the separate cold-cache acquisition lane.
No integration suite imports private tool implementation modules.

## Publication

Review and merge package version changes normally before publication.
Dispatch the publication workflow on the approved revision, selecting one package and an independently released publication-tool version.
The workflow verifies its archive and submits it for registry review; a pending registry review is not a published release.
Packages retain independent versions, and compatibility follows the interfaces consumed by each operation.

## Registry dependencies

Framework SDK/build/macros `0.0.0-dev.8` are published in the Phoxal registry.
Committed application lockfiles record their registry sources and archive checksums.
Normal source builds use those dependencies without a sibling framework checkout or a local overlay.
Publishing this repository's application or participant packages remains a separate release operation.
