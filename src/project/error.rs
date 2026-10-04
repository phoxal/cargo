use std::path::PathBuf;

/// A failure while discovering a project from an invocation directory.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The invocation path could not be canonicalized.
    #[error("cannot resolve {path}: {source}")]
    Resolve {
        /// The path supplied by the caller.
        path: PathBuf,
        /// The filesystem failure.
        source: std::io::Error,
    },
    /// No authored robot document was found in the ancestor chain.
    #[error("no robot.yaml found at or above {start}")]
    MissingRobot { start: PathBuf },
    /// The discovered robot root does not carry its required Cargo package.
    #[error(
        "robot root {root} has no Cargo.toml; the root-local brain must be an ordinary Cargo package"
    )]
    MissingManifest {
        /// Directory containing robot.yaml.
        root: PathBuf,
    },
}

/// A malformed authored robot document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// The robot identity is empty or whitespace.
    #[error("robot.id must not be empty")]
    EmptyRobotId,
    /// An identity used for an instance or service is malformed.
    #[error(
        "{field} '{value}' is not a valid project identifier; use lowercase letters, digits, '-' or '_'"
    )]
    InvalidIdentifier {
        /// Authored field containing the identity.
        field: String,
        /// Invalid value.
        value: String,
    },
    /// A component instance used the native namespace separator reserved by composition.
    #[error("{field} '{value}' contains reserved native namespace separator '__'")]
    ReservedNamespaceSeparator {
        /// Authored field containing the identity.
        field: String,
        /// Invalid value.
        value: String,
    },
    /// An authored map tried to claim the reserved brain identity.
    #[error("{field} uses reserved instance id 'brain'; the root Cargo package owns the brain")]
    ReservedBrainId {
        /// Authored map containing the identity.
        field: String,
    },
    /// A service and component tried to use the same runtime instance id.
    #[error(
        "service and component composition both use instance id '{instance}'; runtime identities must be unique"
    )]
    InstanceCollision {
        /// Conflicting instance identity.
        instance: String,
    },
    /// A service or component source key is empty.
    #[error("{field} must not be empty")]
    EmptySourceKey {
        /// Authored field containing the key.
        field: String,
    },
    /// A service configuration explicitly used null.
    #[error("{field} must be a mapping when present; omit config for an empty configuration")]
    NullConfiguration {
        /// Authored configuration field.
        field: String,
    },
    /// A service configuration had a non-object root.
    #[error("{field} must be a mapping when present")]
    NonMappingConfiguration {
        /// Authored configuration field.
        field: String,
    },
    /// A connection key or endpoint did not have exactly one instance separator.
    #[error("{field} '{value}' must use instance.port syntax")]
    InvalidPortReference {
        /// Authored connection field.
        field: String,
        /// Invalid endpoint text.
        value: String,
    },
    /// A connection list was present but empty.
    #[error("{field} must contain at least one producer")]
    EmptyConnectionSources {
        /// Authored connection field.
        field: String,
    },
    /// A connection listed the same producer more than once.
    #[error("{field} lists producer '{producer}' more than once")]
    DuplicateConnectionSource {
        /// Authored connection field.
        field: String,
        /// Repeated producer endpoint.
        producer: String,
    },
    /// A connection named an instance that is absent from the composition.
    #[error("{field} references unknown instance '{instance}'")]
    UnknownConnectionInstance {
        /// Authored connection field.
        field: String,
        /// Missing instance.
        instance: String,
    },
    /// A connection key used an instance that cannot consume graph inputs.
    #[error(
        "{field} uses '{instance}' as a consumer, but only services, selected drivers and brain may consume connections"
    )]
    InvalidConnectionConsumer {
        /// Authored connection field.
        field: String,
        /// Consumer instance.
        instance: String,
    },
    /// A service entry used an unsupported field.
    #[error("services.{service}: {message}")]
    InvalidService {
        /// Service instance id.
        service: String,
        /// Specific service error.
        message: String,
    },
    /// A component driver declaration is not a mapping or has an invalid
    /// authored configuration value.
    #[error("robot.components.{component}.driver: {message}")]
    InvalidDriver {
        /// Component instance id.
        component: String,
        /// Specific driver error.
        message: String,
    },
}

/// A source-selection failure after Cargo has resolved the project graph.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// A selected target is gated by features that the package does not define.
    #[error("brain binary '{binary}' requires undefined Cargo features: {features}")]
    UndefinedRequiredFeatures {
        /// Selected target name.
        binary: String,
        /// Undefined feature names.
        features: String,
    },
    /// A root-local brain could not be identified from the root package.
    #[error(
        "the robot root package has no eligible brain binary; add one binary target or set brain.binary"
    )]
    MissingBrain,
    /// A requested brain binary is not an ordinary root-package binary.
    #[error("brain binary '{binary}' is not an eligible binary target of the root Cargo package")]
    InvalidBrainBinary {
        /// Requested target name.
        binary: String,
    },
    /// More than one root-package binary could serve as the brain.
    #[error(
        "the robot root package has multiple eligible brain binaries ({candidates}); set brain.binary"
    )]
    AmbiguousBrain {
        /// Candidate target names.
        candidates: String,
    },
    /// The selected brain binary requires features that are not enabled by the root graph.
    #[error("brain binary '{binary}' requires Cargo features that are not enabled: {features}")]
    BrainFeatures {
        /// Selected target name.
        binary: String,
        /// Required feature names.
        features: String,
    },
}

/// The top-level error returned by project discovery, preparation, and Cargo commands.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Project discovery failed.
    #[error("project discovery failed: {0}")]
    Discovery(#[from] DiscoveryError),
    /// Reading robot.yaml failed.
    #[error("cannot read robot.yaml at {path}: {source}")]
    ReadRobot {
        /// Authored manifest path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Parsing robot.yaml failed.
    #[error("cannot parse robot.yaml at {path}: {source}")]
    ParseRobot {
        /// Authored manifest path.
        path: PathBuf,
        /// YAML parser failure.
        source: serde_yaml::Error,
    },
    /// The parsed robot document violated one or more authoring rules.
    #[error("robot.yaml at {path} is invalid: {errors}")]
    InvalidRobot {
        /// Authored manifest path.
        path: PathBuf,
        /// All discovered validation failures.
        errors: ValidationErrors,
    },
    /// Reading Cargo.toml failed.
    #[error("cannot read Cargo.toml at {path}: {source}")]
    ReadManifest {
        /// Root package manifest path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Parsing Cargo.toml failed.
    #[error("cannot parse Cargo.toml at {path}: {source}")]
    ParseManifest {
        /// Root package manifest path.
        path: PathBuf,
        /// TOML parser failure.
        source: toml::de::Error,
    },
    /// A declaration-level composition check failed before Cargo ran.
    #[error("declaration check failed: {message}")]
    DeclarationCheck {
        /// Specific connection or requirement diagnostic.
        message: String,
    },
    /// The root Cargo.toml is a virtual manifest rather than the robot package.
    #[error(
        "Cargo.toml at {path} is a virtual manifest; the robot root must own an ordinary package for its brain"
    )]
    VirtualManifest {
        /// Root package manifest path.
        path: PathBuf,
    },
    /// Cargo metadata failed during preparation.
    #[error("cargo metadata failed for {manifest}: {source}")]
    CargoMetadata {
        /// Manifest passed to Cargo.
        manifest: PathBuf,
        /// Cargo metadata failure.
        source: cargo_metadata::Error,
    },
    /// An ordinary preparation would add a known required dependency, but the
    /// Updating the authored manifest for automatic preparation failed.
    #[error("cannot prepare Cargo manifest {path}: {message}")]
    ManifestPreparation {
        /// Root Cargo manifest.
        path: PathBuf,
        /// Manifest edit diagnostic.
        message: String,
    },
    /// A selected source could not be resolved.
    #[error("source selection failed: {0}")]
    Source(#[from] SourceError),
    /// Rust-contract preparation from a compiled artifact failed.
    #[error("contract preparation failed: {message}")]
    ContractPreparation {
        /// Specific failure.
        message: String,
    },
    /// Cargo command execution failed.
    #[error("cargo {operation} failed ({status}):\n{stdout}\n{stderr}")]
    CargoCommand {
        /// Cargo subcommand.
        operation: String,
        /// Exit status rendered for diagnostics.
        status: String,
        /// Captured standard output, including Rust test assertion failures.
        stdout: String,
        /// Captured standard error.
        stderr: String,
    },
    /// A command could not be started.
    #[error("cannot start cargo {operation}: {source}")]
    CargoSpawn {
        /// Cargo subcommand.
        operation: String,
        /// Process-spawn failure.
        source: std::io::Error,
    },
    /// Command options are contradictory.
    #[error("invalid Cargo command options: {message}")]
    InvalidOptions {
        /// Option diagnostic.
        message: String,
    },
    /// A simulation bundle could not carry a complete truthful native contract.
    #[error("simulation preparation failed: {message}")]
    SimulationInvalid {
        /// Simulation contract diagnostic.
        message: String,
    },
    /// Cargo did not report the selected executable in its machine-readable
    /// artifact stream.
    #[error(
        "cargo build produced no executable for package '{package}' target '{target}': {message}"
    )]
    ArtifactCapture {
        /// Selected package name.
        package: String,
        /// Selected Cargo target.
        target: String,
        /// Artifact-stream diagnostic.
        message: String,
    },
    /// A selected execution target did not expose its compiled Runtime
    /// contract metadata.
    #[error(
        "selected {role} '{instance}' package '{package}' binary '{target}' has no embedded Phoxal Runtime contract metadata"
    )]
    MissingArtifactContract {
        /// Selected graph role.
        role: String,
        /// Runtime instance identity.
        instance: String,
        /// Cargo package name.
        package: String,
        /// Cargo binary target.
        target: String,
    },
    /// A selected service configuration did not satisfy its exact compiled
    /// Runtime::Config schema.
    #[error(
        "configuration for {role} '{instance}' in {field} is invalid for package '{package}': {message}"
    )]
    ConfigurationInvalid {
        /// Selected graph role.
        role: String,
        /// Runtime instance identity.
        instance: String,
        /// Authored configuration path.
        field: String,
        /// Selected package name.
        package: String,
        /// Schema or instance diagnostic.
        message: String,
    },
    /// Building or launching the selected supervisor failed.
    #[error("supervisor launch failed: {message}")]
    SupervisorLaunch {
        /// Launch diagnostic.
        message: String,
    },
    /// A reported or authored artifact was not a safe regular file.
    #[error("invalid artifact {path}: {message}")]
    ArtifactInvalid {
        /// Artifact path.
        path: PathBuf,
        /// Validation diagnostic.
        message: String,
    },
    /// Reading an artifact input failed.
    #[error("cannot read artifact {path}: {source}")]
    ArtifactFile {
        /// Artifact path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Creating a compiled bundle directory failed.
    #[error("cannot create bundle directory {path}: {source}")]
    BundleDirectory {
        /// Bundle directory path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Copying one executable into a bundle failed.
    #[error("cannot copy executable {from} to {to}: {source}")]
    BundleCopy {
        /// Cargo-produced executable.
        from: PathBuf,
        /// Bundle destination.
        to: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Serializing a bundle record failed.
    #[error("cannot serialize bundle record {path}: {source}")]
    BundleJson {
        /// Record path.
        path: PathBuf,
        /// Serialization failure.
        source: serde_json::Error,
    },
    /// The assembled bundle failed the shared admission.
    #[error("{message}")]
    BundleInvalid {
        /// Why the assembled bundle cannot be admitted.
        message: String,
    },
    /// Writing a bundle record failed.
    #[error("cannot write bundle record {path}: {source}")]
    BundleWrite {
        /// Record path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Another process is already constructing the same bundle output.
    #[error("another cargo phoxal command is publishing compiled bundle {path}")]
    BundleBusy {
        /// Contended complete bundle output.
        path: PathBuf,
    },
    /// Opening or locking the bundle publication guard failed.
    #[error("cannot lock compiled bundle publication at {path}: {source}")]
    BundleLock {
        /// Stable lock file for one output path.
        path: PathBuf,
        /// Filesystem or locking failure.
        source: std::io::Error,
    },
    /// Publishing the complete bundle failed.
    #[error("cannot publish compiled bundle {path}: {source}")]
    BundlePublish {
        /// Bundle output path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
    /// Cleaning a replaced bundle failed.
    #[error("cannot clean replaced bundle {path}: {source}")]
    BundleCleanup {
        /// Replaced bundle path.
        path: PathBuf,
        /// Filesystem failure.
        source: std::io::Error,
    },
}

/// A stable display wrapper for all validation failures in one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            error.fmt(formatter)?;
        }
        Ok(())
    }
}
