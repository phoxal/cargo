//! Locates the SDK actually selected by this application's Cargo graph.
//! Tests can scaffold source consumers from a registry installation or a
//! development overlay without knowing any sibling checkout layout.

use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) fn sdk_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut command = cargo_metadata::MetadataCommand::new();
        command.manifest_path(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"));
        if let Some(cargo) = std::env::var_os("CARGO") {
            command.cargo_path(cargo);
        }
        let metadata = command
            .exec()
            .unwrap_or_else(|error| panic!("the tool dependency graph resolves: {error}"));
        let sdk = metadata
            .packages
            .iter()
            .find(|package| package.name == "phoxal")
            .unwrap_or_else(|| panic!("the tool selects the public SDK"));
        sdk.manifest_path
            .as_std_path()
            .parent()
            .unwrap_or_else(|| panic!("SDK package root"))
            .to_owned()
    })
    .clone()
}
