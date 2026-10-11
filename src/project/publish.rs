//! Publication slots, immutable generations, and the stable root symlink.
//!
//! One publication is a complete validated staging tree. It becomes an
//! immutable generation directory beside the slot's stable root, and the
//! stable root is one atomically replaced symlink to the selected
//! generation. Readers resolve the stable root once and read only that
//! generation; previous generations stay available, and every failure path
//! leaves the previous pointer and its contents in place.
//!
//! The slot never deletes or overwrites artifacts it does not manage: an
//! unmanaged destination is refused with a concrete diagnostic.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs4::{FileExt, TryLockError};

use crate::project::Error;

/// The stable-root entry name every payload executable lives under.
const BIN_DIR: &str = "bin";

/// One typed publication outcome.
///
/// Compiler completion or an existing output alone is not success; the
/// outcome states whether this invocation published a new generation or
/// verified the current one as complete and identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PublicationOutcome {
    /// A new generation became the selected publication.
    Published {
        /// The stable publication root readers observe.
        stable: PathBuf,
        /// The immutable generation the stable root selects.
        generation: PathBuf,
    },
    /// The current generation already carries exactly this publication,
    /// payload permissions included.
    Unchanged {
        /// The stable publication root readers observe.
        stable: PathBuf,
        /// The verified immutable generation.
        generation: PathBuf,
    },
}

/// The deterministic slot key for one complete invocation selection.
///
/// The key covers the ordered canonical robot files, the effective build
/// settings (target, profile, features, feature policy), and the effective
/// toolchain identity resolved through the Cargo owner, never the changing
/// source bytes: ordinary edits publish to the same slot.
pub(crate) fn selection_key(
    robot_files: &[PathBuf],
    options: &crate::project::CargoOptions,
    toolchain: &str,
) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"phoxal-publication-slot-v1\x1f");
    for file in robot_files {
        hasher.update(file.as_os_str().as_encoded_bytes());
        hasher.update(b"\x1e");
    }
    hasher.update(b"\x1f");
    let selection = serde_json::to_string(&crate::project::cargo::BuildSelection::root(options))
        .unwrap_or_default();
    hasher.update(selection.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(toolchain.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_owned()
}

/// The stable publication root for one selection under a resolved target
/// directory.
///
/// `<target>/phoxal/<robot-id>/<target>/<profile>/<selection-key>/build`
/// is the one resolver shared by the path query and the real build.
pub(crate) fn slot_path(
    target_directory: &Path,
    robot_id: &str,
    robot_files: &[PathBuf],
    options: &crate::project::CargoOptions,
    toolchain: &str,
) -> PathBuf {
    target_directory
        .join("phoxal")
        .join(robot_id)
        .join(crate::project::cargo::effective_target(options))
        .join(crate::project::cargo::effective_profile(options))
        .join(selection_key(robot_files, options, toolchain))
        .join("build")
}

/// One publication destination: the stable root and its generations.
struct Slot {
    /// The stable root readers observe, a symlink to the selected generation.
    stable: PathBuf,
    /// The immutable generation directories, beside the stable root.
    generations: PathBuf,
}

impl Slot {
    fn new(stable: &Path) -> Result<Self, Error> {
        let parent = stable.parent().ok_or_else(|| Error::BundlePublish {
            path: stable.to_owned(),
            source: io::Error::other("publication slot has no parent directory"),
        })?;
        let name = stable.file_name().ok_or_else(|| Error::BundlePublish {
            path: stable.to_owned(),
            source: io::Error::other("publication slot has no directory name"),
        })?;
        Ok(Self {
            stable: stable.to_owned(),
            generations: parent.join(format!("{}.generations", name.to_string_lossy())),
        })
    }

    /// The generation directory one content digest selects.
    fn generation(&self, digest: &str) -> PathBuf {
        self.generations.join(digest)
    }
}

/// A held per-output publication lock; only one managed writer at a time.
pub(crate) struct PublicationLock {
    file: File,
    key: PathBuf,
}

static ACTIVE_PUBLICATION_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeSet<PathBuf>>,
> = std::sync::OnceLock::new();

impl Drop for PublicationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        if let Some(active) = ACTIVE_PUBLICATION_LOCKS.get()
            && let Ok(mut active) = active.lock()
        {
            active.remove(&self.key);
        }
    }
}

/// Admits the one managed writer for one output path.
///
/// The lock directory sits beside the output like the generations do, so an
/// explicit destination keeps all publication machinery under its parent.
pub(crate) fn acquire_publication_lock(stable: &Path) -> Result<PublicationLock, Error> {
    let parent = stable.parent().ok_or_else(|| Error::BundlePublish {
        path: stable.to_owned(),
        source: io::Error::other("publication slot has no parent directory"),
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|source| Error::BundleDirectory {
            path: parent.to_owned(),
            source,
        })?;
    let name = stable.file_name().ok_or_else(|| Error::BundlePublish {
        path: stable.to_owned(),
        source: io::Error::other("publication slot has no directory name"),
    })?;
    let lock_directory = canonical_parent.join(".phoxal-bundle-locks");
    fs::create_dir_all(&lock_directory).map_err(|source| Error::BundleDirectory {
        path: lock_directory.clone(),
        source,
    })?;
    let lock_path = lock_directory.join(format!("{}.lock", name.to_string_lossy()));
    let active = ACTIVE_PUBLICATION_LOCKS.get_or_init(|| std::sync::Mutex::new(Default::default()));
    let mut active = active.lock().map_err(|_| Error::BundleLock {
        path: lock_path.clone(),
        source: io::Error::other("publication lock registry is poisoned"),
    })?;
    if active.contains(&lock_path) {
        return Err(Error::BundleBusy {
            path: stable.to_owned(),
        });
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::BundleLock {
            path: lock_path.clone(),
            source,
        })?;
    match FileExt::try_lock(&lock) {
        Ok(()) => {
            active.insert(lock_path.clone());
            Ok(PublicationLock {
                file: lock,
                key: lock_path,
            })
        }
        Err(TryLockError::WouldBlock) => Err(Error::BundleBusy {
            path: stable.to_owned(),
        }),
        Err(TryLockError::Error(source)) => Err(Error::BundleLock {
            path: lock_path,
            source,
        }),
    }
}

/// One complete payload entry as the publication compares it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PayloadEntry {
    /// Repository-relative path with `/` separators.
    path: String,
    /// Executable-class payloads keep their permission identity: a brain
    /// chmod 644 is a different, invalid generation, not an unchanged one.
    executable: bool,
    /// Content digest of the file bytes.
    digest: String,
}

/// Digests one complete payload tree for identity and comparisons.
///
/// The comparison covers every entry's kind, executable class, and content,
/// so manifest text, payload bytes, and permissions all participate.
fn payload_digest(root: &Path) -> Result<(String, Vec<PayloadEntry>), Error> {
    use sha2::Digest as _;
    let mut entries = BTreeMap::new();
    collect_payload_entries(root, root, &mut entries)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"phoxal-generation-v1\x1f");
    for (path, entry) in &entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\x1e");
        hasher.update(if entry.executable { b"x" } else { b"-" });
        hasher.update(b"\x1e");
        hasher.update(entry.digest.as_bytes());
        hasher.update(b"\x1f");
    }
    Ok((
        format!("{:x}", hasher.finalize())[..24].to_owned(),
        entries.into_values().collect(),
    ))
}

fn collect_payload_entries(
    root: &Path,
    directory: &Path,
    entries: &mut BTreeMap<String, PayloadEntry>,
) -> Result<(), Error> {
    for entry in fs::read_dir(directory).map_err(|source| Error::BundleDirectory {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::BundleDirectory {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(Error::ArtifactInvalid {
                path,
                message: "bundle payloads must not contain symlinks".to_owned(),
            });
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::ArtifactInvalid {
                path: path.clone(),
                message: "bundle payload escaped its root".to_owned(),
            })?
            .to_string_lossy()
            .replace('\\', "/");
        if metadata.is_dir() {
            collect_payload_entries(root, &path, entries)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(Error::ArtifactInvalid {
                path,
                message: "bundle payloads must contain regular files".to_owned(),
            });
        }
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let bytes = fs::read(&path).map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
        use sha2::Digest as _;
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        entries.insert(
            relative.clone(),
            PayloadEntry {
                path: relative,
                executable,
                digest,
            },
        );
    }
    Ok(())
}

/// Validates one complete payload tree for publication or adoption.
///
/// Every entry must be a regular file or directory with no symlinks, and
/// every executable under `bin/` must actually carry an executable bit.
fn validate_payload(payload_root: &Path) -> Result<(), Error> {
    validate_payload_directory(payload_root, payload_root)
}

fn validate_payload_directory(payload_root: &Path, directory: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(directory).map_err(|source| Error::BundleDirectory {
        path: directory.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::BundleDirectory {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(Error::ArtifactInvalid {
                path,
                message: "bundle payloads must not contain symlinks".to_owned(),
            });
        }
        if metadata.is_dir() {
            validate_payload_directory(payload_root, &path)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(Error::ArtifactInvalid {
                path,
                message: "bundle payloads must contain regular files".to_owned(),
            });
        }
        let relative = path
            .strip_prefix(payload_root)
            .map_err(|_| Error::ArtifactInvalid {
                path: path.clone(),
                message: "bundle payload escaped its root".to_owned(),
            })?;
        let mut components = relative.components();
        let under_bin = components
            .next()
            .is_some_and(|component| component.as_os_str() == BIN_DIR)
            && components.next().is_some();
        if under_bin && metadata.permissions().mode() & 0o111 == 0 {
            return Err(Error::ArtifactInvalid {
                path: path.clone(),
                message: format!(
                    "bundled executable {} is missing its executable permission",
                    relative.display()
                ),
            });
        }
    }
    Ok(())
}

/// Publishes one validated staging tree through the slot.
///
/// The staging tree must live on the slot's filesystem (its parent). On
/// success the staging directory is consumed. The previous pointer survives
/// every failure, and unmanaged destinations are refused without deletion.
pub(crate) fn publish(
    staging: &Path,
    stable: &Path,
    phases: &dyn super::PhaseReporter,
) -> Result<PublicationOutcome, Error> {
    phases.phase("publishing");
    let slot = Slot::new(stable)?;
    validate_payload(staging)?;
    let (digest, _entries) = payload_digest(staging)?;
    ensure_managed_storage(&slot)?;
    fs::create_dir_all(&slot.generations).map_err(|source| Error::BundleDirectory {
        path: slot.generations.clone(),
        source,
    })?;

    // The controlled root must already be this slot's indirection: one
    // symlink to one normal generation entry inside this slot's generations
    // storage, verified by real containment. Everything else at the root is
    // unmanaged and refused, never replaced or moved; ownership is never
    // inferred from a directory's shape or a pathname.
    if let Some(current) = controlled_root(&slot)? {
        // Unchanged depends only on complete verified equality: the
        // selected generation's actual bytes digest to the staged identity
        // and the whole payload validates, whatever its filename is.
        if generation_identity_matches(&current, &digest)? && validate_payload(&current).is_ok() {
            phases.before_publication()?;
            remove_staging(staging)?;
            return Ok(PublicationOutcome::Unchanged {
                stable: slot.stable.clone(),
                generation: current,
            });
        }
    }

    // A previously installed generation may already carry exactly this
    // verified content (a repair disambiguation, or content returned to an
    // earlier state): repoint to it instead of installing a duplicate, so
    // valid disambiguated generations never grow repeated copies.
    if let Some(existing) = find_verified_generation(&slot, &digest)? {
        phases.before_publication()?;
        remove_staging(staging)?;
        swap_pointer(&slot, &existing)?;
        return Ok(PublicationOutcome::Published {
            stable: slot.stable.clone(),
            generation: existing,
        });
    }

    // Every installation is a fresh immutable generation: existing paths
    // are never rewritten, deleted, or reused, so repairs and changed
    // payloads retain every prior reader path and its contents.
    let generation = fresh_generation(&slot, &digest)?;
    phases.before_publication()?;
    fs::rename(staging, &generation).map_err(|source| Error::BundlePublish {
        path: generation.clone(),
        source,
    })?;
    // A failed pointer swap leaves the previous pointer and every
    // generation's contents untouched; the freshly installed generation
    // simply becomes the next candidate.
    swap_pointer(&slot, &generation)?;
    Ok(PublicationOutcome::Published {
        stable: slot.stable.clone(),
        generation,
    })
}

/// Rejects unmanaged startup destinations using ordinary publication ownership.
pub(crate) fn validate_output(stable: &Path) -> Result<(), Error> {
    ensure_managed_storage(&Slot::new(stable)?)
}

/// Certifies a complete existing publication before watch preservation claims.
pub(crate) fn verified_current(stable: &Path) -> Result<bool, Error> {
    let slot = Slot::new(stable)?;
    let Some(current) = controlled_root(&slot)? else {
        return Ok(false);
    };
    if validate_payload(&current).is_err() {
        return Ok(false);
    }
    let Ok((digest, _)) = payload_digest(&current) else {
        return Ok(false);
    };
    let name = current
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !(name == digest
        || name
            .strip_prefix(&format!("{digest}-"))
            .is_some_and(|suffix| {
                suffix
                    .parse::<u64>()
                    .is_ok_and(|i| i >= 2 && i.to_string() == suffix)
            }))
    {
        return Ok(false);
    }
    let Ok(bytes) = fs::read(current.join("manifest.json")) else {
        return Ok(false);
    };
    let Ok(manifest) = serde_json::from_slice(&bytes) else {
        return Ok(false);
    };
    Ok(phoxal::artifact::bundle::AdmittedBundle::validate(manifest).is_ok())
}

/// One installed generation whose actual bytes digest to the identity and
/// whose payload validates, if any exists in the slot's storage.
fn find_verified_generation(slot: &Slot, digest: &str) -> Result<Option<PathBuf>, Error> {
    let Ok(entries) = fs::read_dir(&slot.generations) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Names locate only this identity and the numeric disambiguations
        // installed by fresh_generation. Actual payload verification below
        // still decides whether a candidate can be reused.
        if !entry.file_name().to_str().is_some_and(|name| {
            name == digest
                || name.strip_prefix(digest).is_some_and(|suffix| {
                    suffix.strip_prefix('-').is_some_and(|index| {
                        index
                            .parse::<u64>()
                            .is_ok_and(|number| number >= 2 && number.to_string() == index)
                    })
                })
        }) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if generation_identity_matches(&path, digest)? && validate_payload(&path).is_ok() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Removes one consumed staging tree.
fn remove_staging(staging: &Path) -> Result<(), Error> {
    fs::remove_dir_all(staging).map_err(|source| Error::BundleCleanup {
        path: staging.to_owned(),
        source,
    })
}

/// Validates that the slot's storage is managed by this tool.
///
/// The generations directory must be a real directory (or absent), never an
/// unmanaged indirection, and the stable root must be absent, a regular
/// file refused as unmanaged, a directory refused as unmanaged, or this
/// slot's controlled symlink into its generations storage.
fn ensure_managed_storage(slot: &Slot) -> Result<(), Error> {
    if let Ok(metadata) = fs::symlink_metadata(&slot.generations)
        && !metadata.is_dir()
    {
        return Err(unmanaged(
            slot,
            "the generations storage is not a directory",
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(&slot.stable) {
        if metadata.is_file() {
            return Err(unmanaged(
                slot,
                "it is a regular file, not a managed publication",
            ));
        }
        if metadata.is_dir() {
            return Err(unmanaged(
                slot,
                "it is an existing directory, not a managed publication",
            ));
        }
        if metadata.file_type().is_symlink() && controlled_root(slot)?.is_none() {
            return Err(unmanaged(
                slot,
                "the existing symlink is not this slot's controlled publication root",
            ));
        }
    }
    Ok(())
}

/// One unmanaged-output refusal with a concrete reason.
fn unmanaged(slot: &Slot, reason: &str) -> Error {
    Error::BundlePublish {
        path: slot.stable.clone(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing unmanaged output {}: {reason}",
                slot.stable.display()
            ),
        ),
    }
}

/// The generation directory this slot's controlled root selects.
///
/// The root qualifies as controlled only when it is a symlink whose target
/// is one directory entry inside this slot's generations storage and that
/// directory exists. Any other indirection is unmanaged.
fn controlled_root(slot: &Slot) -> Result<Option<PathBuf>, Error> {
    let target = match fs::read_link(&slot.stable) {
        Ok(target) => target,
        Err(_) => return Ok(None),
    };
    let generations_name = slot.generations.file_name().unwrap_or_default();
    // Exactly `<generations>/<one normal entry>`: a parent or root
    // component never qualifies, and the resolved directory must really be
    // contained in the slot's own storage.
    let mut components = target.components();
    let in_storage = components
        .next()
        .is_some_and(|c| c.as_os_str() == generations_name)
        && components
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::Normal(_)))
        && components.next().is_none();
    if !in_storage {
        return Ok(None);
    }
    let resolved = slot.stable.parent().unwrap_or(Path::new(".")).join(&target);
    let Ok(metadata) = fs::symlink_metadata(&resolved) else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let storage = fs::canonicalize(&slot.generations).ok();
    let generation = fs::canonicalize(&resolved).ok();
    match (storage, generation) {
        (Some(storage), Some(generation)) if generation.starts_with(&storage) => Ok(Some(resolved)),
        _ => Ok(None),
    }
}

/// Whether one generation's actual contents digest to the claimed identity.
fn generation_identity_matches(generation: &Path, digest: &str) -> Result<bool, Error> {
    match payload_digest(generation) {
        Ok((actual, _)) => Ok(actual == digest),
        Err(_) => Ok(false),
    }
}

/// One previously unused generation path for this identity.
///
/// Identical content re-selects the digest path only when that path is
/// absent; otherwise a fresh disambiguated path keeps every installed
/// generation immutable and distinct.
fn fresh_generation(slot: &Slot, digest: &str) -> Result<PathBuf, Error> {
    let candidate = slot.generation(digest);
    if !candidate.exists() {
        return Ok(candidate);
    }
    for index in 2.. {
        let candidate = slot.generations.join(format!("{digest}-{index}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::BundlePublish {
        path: slot.generations.clone(),
        source: io::Error::other("no unused generation path is available"),
    })
}

/// Atomically replaces the stable symlink with one to the generation.
fn swap_pointer(slot: &Slot, generation: &Path) -> Result<(), Error> {
    if crate::owned_process::cancelled() {
        return Err(Error::WatchCancelled);
    }
    let temporary = slot
        .generations
        .join(format!(".pointer-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let target = generation
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    std::os::unix::fs::symlink(
        format!(
            "{}",
            slot.generations
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ) + "/"
            + &target,
        &temporary,
    )
    .map_err(|source| Error::BundlePublish {
        path: slot.stable.clone(),
        source,
    })?;
    if crate::owned_process::cancelled() {
        let _ = fs::remove_file(&temporary);
        return Err(Error::WatchCancelled);
    }
    fs::rename(&temporary, &slot.stable).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        Error::BundlePublish {
            path: slot.stable.clone(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regular(path: &Path, bytes: &[u8], executable: bool) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, bytes).expect("write");
        if executable {
            let mut permissions = fs::metadata(path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("mode");
        }
    }

    fn staged(directory: &tempfile::TempDir) -> PathBuf {
        let staging = directory.path().join("build.generations").join(".staging");
        regular(&staging.join("bin/brain"), b"brain", true);
        regular(&staging.join("manifest.json"), b"{}", false);
        staging
    }

    fn controlled(slot_stable: &Path) -> PathBuf {
        fs::read_link(slot_stable)
            .map(|target| slot_stable.parent().unwrap().join(target))
            .expect("controlled root")
    }

    #[test]
    fn identical_publications_report_unchanged_with_verified_bytes() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("build");
        let staging = staged(&directory);
        let first =
            publish(&staging, &stable, &crate::project::NoPhases).expect("first publication");
        assert!(matches!(first, PublicationOutcome::Published { .. }));
        let staging = staged(&directory);
        let second =
            publish(&staging, &stable, &crate::project::NoPhases).expect("second publication");
        assert!(matches!(second, PublicationOutcome::Unchanged { .. }));
        assert!(fs::read_link(&stable).is_ok());
    }

    #[test]
    fn corrupted_generation_content_is_never_certified_unchanged() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("build");
        let staging = staged(&directory);
        publish(&staging, &stable, &crate::project::NoPhases).expect("publication");
        let selected = controlled(&stable);
        // Corrupt the owned manifest like the review reproduction: the
        // pathname still claims the identity, but the bytes no longer do.
        fs::write(selected.join("manifest.json"), b"corrupted").expect("corrupt");
        let staging = staged(&directory);
        let outcome =
            publish(&staging, &stable, &crate::project::NoPhases).expect("repaired publication");
        assert!(matches!(outcome, PublicationOutcome::Published { .. }));
        let repaired = controlled(&stable);
        assert_eq!(
            fs::read(repaired.join("manifest.json")).expect("manifest"),
            b"{}",
            "the published manifest carries the real content"
        );
        assert_eq!(
            fs::read(selected.join("manifest.json")).expect("retained"),
            b"corrupted",
            "prior generation content is retained untouched"
        );
    }

    #[test]
    fn a_lost_executable_bit_installs_a_fresh_generation_and_retains_the_old() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("build");
        let staging = staged(&directory);
        publish(&staging, &stable, &crate::project::NoPhases).expect("publication");
        let degraded = controlled(&stable);
        let brain = degraded.join("bin/brain");
        let mut permissions = fs::metadata(&brain).expect("brain").permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&brain, permissions).expect("strip the executable bit");
        let staging = staged(&directory);
        let outcome =
            publish(&staging, &stable, &crate::project::NoPhases).expect("repaired publication");
        assert!(matches!(outcome, PublicationOutcome::Published { .. }));
        let repaired = match &outcome {
            PublicationOutcome::Published { generation, .. } => generation.clone(),
            _ => unreachable!(),
        };
        assert_ne!(repaired, degraded, "repairs never rewrite a prior path");
        let mode = fs::metadata(repaired.join("bin/brain"))
            .expect("brain")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "the executable permission is restored");
        // The degraded generation keeps its exact prior path and contents.
        assert_eq!(
            fs::metadata(&brain).expect("retained").permissions().mode() & 0o111,
            0
        );
    }

    #[test]
    fn arbitrary_symlinks_and_shaped_directories_are_refused() {
        let directory = tempfile::tempdir().expect("directory");
        for (name, prepare) in [("link", 1u8), ("shaped", 2u8), ("file", 3u8)] {
            let stable = directory.path().join(name);
            match prepare {
                1 => {
                    let target = directory.path().join("user-target");
                    fs::write(&target, b"user").expect("target");
                    std::os::unix::fs::symlink(&target, &stable).expect("user link");
                }
                2 => {
                    regular(&stable.join("manifest.json"), b"user text", false);
                    regular(&stable.join("bin/tool"), b"user tool", true);
                }
                _ => {
                    fs::write(&stable, b"user file").expect("user file");
                }
            }
            let staging = staged(&directory);
            let error = publish(&staging, &stable, &crate::project::NoPhases)
                .expect_err("unmanaged output refused");
            assert!(
                format!("{error:#}").contains("refusing unmanaged output"),
                "{name}: {error:#}"
            );
            match prepare {
                1 => assert_eq!(
                    fs::read(directory.path().join("user-target")).expect("target intact"),
                    b"user"
                ),
                2 => assert_eq!(
                    fs::read(stable.join("manifest.json")).expect("shaped intact"),
                    b"user text"
                ),
                _ => assert_eq!(fs::read(&stable).expect("file intact"), b"user file"),
            }
        }
    }

    #[test]
    fn an_identical_rebuild_after_repair_is_unchanged() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("build");
        let staging = staged(&directory);
        publish(&staging, &stable, &crate::project::NoPhases).expect("publication");
        // Degrade and repair once, installing a disambiguated generation.
        let degraded = controlled(&stable);
        let brain = degraded.join("bin/brain");
        let mut permissions = fs::metadata(&brain).expect("brain").permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&brain, permissions).expect("degrade");
        let staging = staged(&directory);
        publish(&staging, &stable, &crate::project::NoPhases).expect("repair");
        // The next byte-identical build must not reload again: the current
        // generation's verified content equals the staged identity.
        let staging = staged(&directory);
        let outcome = publish(&staging, &stable, &crate::project::NoPhases).expect("no-op");
        assert!(matches!(outcome, PublicationOutcome::Unchanged { .. }));
        let selected = fs::read_link(&stable).expect("pointer");
        let selected = selected.file_name().and_then(|name| name.to_str());
        assert!(
            selected.is_some_and(|name| name.ends_with("-2")),
            "the pointer keeps the disambiguated generation: {selected:?}"
        );
    }

    #[test]
    fn historical_lookup_reads_only_applicable_verified_payloads() {
        use std::time::{Duration, Instant, SystemTime};

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(946_684_800);
        for count in [0, 8, 32] {
            let directory = tempfile::tempdir().expect("directory");
            let stable = directory.path().join("build");
            let slot = Slot::new(&stable).expect("slot");
            let staging = staged(&directory);
            let (digest, _) = payload_digest(&staging).expect("identity");
            let mut unrelated = Vec::new();
            for index in 0..count {
                let path = slot.generations.join(format!("unrelated-{index}"));
                regular(
                    &path.join("assets/model.xml"),
                    &vec![b'x'; 1024 * 1024],
                    false,
                );
                unrelated.push(path.join("assets/model.xml"));
            }
            // A similarly prefixed name is not a tool-installed repair.
            let misleading = slot.generations.join(format!("{digest}-02"));
            regular(&misleading.join("manifest.json"), b"{}", false);
            unrelated.push(misleading.join("manifest.json"));
            for path in &unrelated {
                File::open(path)
                    .expect("asset")
                    .set_times(fs::FileTimes::new().set_accessed(old))
                    .expect("reset access time");
            }
            let start = Instant::now();
            let outcome = publish(&staging, &stable, &crate::project::NoPhases)
                .expect("new identity publication");
            let elapsed = start.elapsed();
            assert!(matches!(outcome, PublicationOutcome::Published { .. }));
            for path in &unrelated {
                assert_eq!(
                    fs::metadata(path).unwrap().accessed().unwrap(),
                    old,
                    "unrelated payload must remain unread: {}",
                    path.display()
                );
            }
            // Matching names cannot certify corrupted bytes or permissions.
            let original = controlled(&stable);
            fs::write(original.join("manifest.json"), b"corrupted").unwrap();
            let staging = staged(&directory);
            publish(&staging, &stable, &crate::project::NoPhases).expect("repair");
            let repaired = controlled(&stable);
            assert_ne!(repaired, original);
            let mut permissions = fs::metadata(repaired.join("bin/brain"))
                .unwrap()
                .permissions();
            permissions.set_mode(0o644);
            fs::set_permissions(repaired.join("bin/brain"), permissions).unwrap();
            let staging = staged(&directory);
            publish(&staging, &stable, &crate::project::NoPhases).expect("permission repair");
            let valid = controlled(&stable);
            assert_ne!(valid, repaired);
            let valid_manifest = valid.join("manifest.json");
            File::open(&valid_manifest)
                .unwrap()
                .set_times(fs::FileTimes::new().set_accessed(old))
                .unwrap();
            let found = find_verified_generation(&slot, &digest).expect("lookup");
            assert_eq!(found, Some(valid));
            assert!(
                fs::metadata(valid_manifest).unwrap().accessed().unwrap() > old,
                "a real matching candidate must be read and verified"
            );
            println!(
                "isolated_publish history={count} unrelated_model_bytes={} elapsed={elapsed:?}",
                count * 1024 * 1024
            );
        }
    }

    #[test]
    fn returning_to_previously_published_content_reuses_its_generation() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("build");
        let a = {
            let staging = staged(&directory);
            publish(&staging, &stable, &crate::project::NoPhases).expect("publish A");
            fs::write(controlled(&stable).join("manifest.json"), b"corrupt A").unwrap();
            let staging = staged(&directory);
            publish(&staging, &stable, &crate::project::NoPhases).expect("repair A");
            controlled(&stable)
        };
        // Publish changed content B.
        let staging = directory
            .path()
            .join("build.generations")
            .join(".staging-b");
        regular(&staging.join("bin/brain"), b"brain B", true);
        regular(&staging.join("manifest.json"), b"{}", false);
        publish(&staging, &stable, &crate::project::NoPhases).expect("publish B");
        assert_ne!(controlled(&stable), a);
        // Return to content A: the verified A generation is repointed, not
        // duplicated, and the pointer keeps it.
        let staging = staged(&directory);
        let outcome = publish(&staging, &stable, &crate::project::NoPhases).expect("return to A");
        assert!(matches!(outcome, PublicationOutcome::Published { .. }));
        assert_eq!(controlled(&stable), a);
        let generations = fs::read_dir(a.parent().expect("generations"))
            .expect("generations")
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| !name.starts_with('.'))
            })
            .count();
        assert_eq!(generations, 3, "degraded A, repaired A and B are retained");
    }

    #[test]
    fn a_parent_component_in_the_controlled_root_is_unmanaged() {
        let directory = tempfile::tempdir().expect("directory");
        let stable = directory.path().join("escape");
        let generations = directory.path().join("escape.generations");
        fs::create_dir_all(&generations).expect("storage");
        std::os::unix::fs::symlink("escape.generations/..", &stable).expect("escape link");
        let staging = staged(&directory);
        let error = publish(&staging, &stable, &crate::project::NoPhases)
            .expect_err("parent escape refused");
        assert!(
            format!("{error:#}").contains("refusing unmanaged output"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_link(&stable).expect("user link intact"),
            std::path::PathBuf::from("escape.generations/..")
        );
    }

    #[test]
    fn the_slot_key_separates_effective_toolchains_and_file_orders() {
        let base = crate::project::CargoOptions::default();
        let files_a = vec![PathBuf::from("/robot/robot.yaml")];
        let files_b = vec![
            PathBuf::from("/robot/robot.yaml"),
            PathBuf::from("/robot/b.yaml"),
        ];
        assert_ne!(
            selection_key(&files_a, &base, "cargo 1.98.0"),
            selection_key(&files_a, &base, "cargo 1.99.0")
        );
        assert_ne!(
            selection_key(&files_a, &base, "cargo 1.99.0"),
            selection_key(&files_b, &base, "cargo 1.99.0")
        );
        assert_eq!(
            selection_key(&files_a, &base, "cargo 1.99.0"),
            selection_key(&files_a, &base, "cargo 1.99.0")
        );
    }
}
