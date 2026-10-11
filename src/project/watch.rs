//! One editing loop around the ordinary project builder and publication.
use super::{CargoOptions, Error, PhaseReporter, Project, document::Source};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const QUIET: Duration = Duration::from_millis(500);
const RESCAN: Duration = Duration::from_millis(200);
type Snapshot = BTreeMap<PathBuf, Vec<u8>>;
thread_local! { static OUTPUT: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }

pub(crate) fn owns_output(path: &Path) -> bool {
    OUTPUT.with(|held| held.borrow().as_deref() == Some(path))
}
struct OutputOwner {
    _lock: super::publish::PublicationLock,
}
impl Drop for OutputOwner {
    fn drop(&mut self) {
        OUTPUT.with(|held| *held.borrow_mut() = None);
    }
}
fn error(message: impl Into<String>) -> Error {
    Error::Watch {
        message: message.into(),
    }
}
fn cancelled() -> Result<(), Error> {
    if crate::owned_process::cancelled() {
        Err(Error::WatchCancelled)
    } else if !crate::owned_process::clean() {
        Err(error(
            "owned subprocess cleanup failed; cannot certify stopped state or resume watching",
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    output: PathBuf,
    id: String,
    target: String,
    toolchain: String,
    settings: String,
    toolchain_files: Snapshot,
    selections: Vec<PathBuf>,
}
impl Pin {
    fn resolve(
        project: &Project,
        options: &CargoOptions,
        output: Option<&Path>,
    ) -> Result<Self, Error> {
        let toolchain_files = Self::toolchain_files(project.layout.root());
        let target_directory = project.resolve_target_directory(options)?;
        let toolchain = project.resolve_toolchain(options)?;
        let config = cargo_config2::de::Config::load_with_cwd(project.layout.root())
            .map_err(|e| error(format!("cannot resolve watch Cargo settings: {e}")))?;
        let configuration = serde_json::to_vec(&config).map_err(|e| error(e.to_string()))?;
        // Detect changed invocation configuration without resolving it again
        // or echoing configured secrets. Cargo remains its compiler owner.
        let settings = format!("{:x}", Sha256::digest(configuration));
        let output = output.map(Path::to_owned).unwrap_or_else(|| {
            super::slot_path(
                &target_directory,
                &project.robot_id(),
                project.robot_files(),
                options,
                &toolchain,
            )
        });
        Ok(Self {
            output,
            id: project.robot_id(),
            target: super::cargo::effective_target(options),
            toolchain,
            settings,
            toolchain_files,
            selections: project.robot_files().to_vec(),
        })
    }
    fn toolchain_files(root: &Path) -> Snapshot {
        root.ancestors()
            .flat_map(|parent| {
                [
                    parent.join("rust-toolchain"),
                    parent.join("rust-toolchain.toml"),
                ]
            })
            .map(|path| {
                let bytes = fs::read(&path).unwrap_or_default();
                (path, bytes)
            })
            .collect()
    }
    fn check_declaration(&self, project: &Project) -> Result<(), Error> {
        let toolchain_files = Self::toolchain_files(project.layout.root());
        if toolchain_files != self.toolchain_files
            || project.robot_id() != self.id
            || project.robot_files() != self.selections
        {
            return Err(error(format!(
                "watch identity/toolchain selection changed; old robot id {:?}, old toolchain {:?}, old toolchain selection files {:?}, old robot files {:?}; new robot id {:?}, new toolchain selection files {:?}, new robot files {:?}; stop and restart cargo phoxal build --watch with the intended selection",
                self.id,
                self.toolchain,
                selection_text(&self.toolchain_files),
                self.selections,
                project.robot_id(),
                selection_text(&toolchain_files),
                project.robot_files()
            )));
        }
        Ok(())
    }
    fn check(&self, next: &Self) -> Result<(), Error> {
        if self == next {
            return Ok(());
        }
        Err(error(format!(
            "watch invocation settings changed; previous settings: {self:?}; new settings: {next:?}; stop this command and restart cargo phoxal build --watch with the intended robot files, target, profile, Cargo settings and output"
        )))
    }
}

fn selection_text(files: &Snapshot) -> Vec<(String, String)> {
    files
        .iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .map(|(path, bytes)| {
            (
                path.display().to_string(),
                String::from_utf8_lossy(bytes).into_owned(),
            )
        })
        .collect()
}

fn resolved_alias(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            return path
                .strip_prefix(ancestor)
                .ok()
                .map(|rest| canonical.join(rest));
        }
    }
    None
}

#[derive(Default, Clone)]
struct Inputs {
    packages: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    resources: BTreeSet<PathBuf>,
    excluded: BTreeSet<PathBuf>,
}
impl Inputs {
    fn declared(project: &Project) -> Self {
        let root = project.layout.root();
        let mut result = Self::default();
        result.packages.insert(root.to_owned());
        result.files.extend(project.robot_files().iter().cloned());
        // Retain the logical names as well as canonical selected files so
        // replacement of a symlink/selected directory remains observable.

        let super::RobotDocument::V0 {
            robot, supervisor, ..
        } = project.document();
        for source in robot
            .services
            .values()
            .map(|s| &s.source)
            .chain(robot.components.values().map(|c| &c.source))
            .chain(std::iter::once(&supervisor.source))
        {
            if let Source::Path(path) = source {
                result.packages.insert(root.join(path));
            }
        }
        if let Some(model) = &robot.model {
            let path = root.join(model);
            result.files.insert(path);
            match model.parent().filter(|p| !p.as_os_str().is_empty()) {
                Some(parent) => {
                    result.resources.insert(root.join(parent));
                }
                None => {
                    result.resources.insert(root.join("assets"));
                }
            }
        }
        result
    }
    fn extend(&mut self, other: Self) {
        self.packages.extend(other.packages);
        self.files.extend(other.files);
        self.resources.extend(other.resources);
        self.excluded.extend(other.excluded);
    }
    fn resolve(&mut self, project: &Project, options: &CargoOptions) -> Result<(), Error> {
        self.extend(Self::declared(project));
        let local_roots = self.packages.clone();
        for root in local_roots {
            cancelled()?;
            let manifest = root.join("Cargo.toml");
            self.files.insert(manifest.clone());
            self.files.insert(root.join("component.yaml"));
            if root.join("component.yaml").is_file() {
                for file in super::component_assets::read(&root)? {
                    self.files.insert(root.join(file));
                }
            }
            if !manifest.is_file() {
                continue;
            }
            let mut selection = options.clone();
            if root != project.layout.root() {
                selection.features.clear();
                selection.all_features = false;
                selection.no_default_features = false;
                selection.cargo_args.clear();
            }
            let metadata = super::cargo::load_metadata_at(&manifest, &root, None, &selection)?;
            self.files
                .insert(metadata.workspace_root.as_std_path().join("Cargo.toml"));
            self.files
                .insert(metadata.workspace_root.as_std_path().join("Cargo.lock"));
            self.excluded
                .insert(metadata.target_directory.into_std_path_buf());
            if let Some(build) = metadata.build_directory {
                self.excluded.insert(build.into_std_path_buf());
            }
            // Traverse the selected package's resolved graph rather than
            // every package in a workspace's unrelated member list.
            let selected = metadata.packages.iter().find(|p| {
                p.manifest_path.as_std_path().canonicalize().ok() == manifest.canonicalize().ok()
            });
            let mut reachable = BTreeSet::new();
            if let Some(selected) = selected {
                reachable.insert(selected.id.clone());
            }
            if let Some(graph) = &metadata.resolve {
                loop {
                    let before = reachable.len();
                    for node in &graph.nodes {
                        if reachable.contains(&node.id) {
                            reachable.extend(node.deps.iter().map(|d| d.pkg.clone()));
                        }
                    }
                    if reachable.len() == before {
                        break;
                    }
                }
            }
            for package in &metadata.packages {
                if package.source.is_none()
                    && reachable.contains(&package.id)
                    && let Some(parent) = package.manifest_path.parent()
                {
                    self.packages.insert(parent.as_std_path().to_owned());
                    for target in &package.targets {
                        let source = target.src_path.as_std_path();
                        self.files.insert(source.to_owned());
                        if !source.starts_with(parent.as_std_path())
                            && let Some(root) = source.parent()
                        {
                            self.packages.insert(root.to_owned());
                        }
                    }
                }
            }
            // Local compilers resolve settings in the robot caller context.
            let compiler = super::cargo::compiler_directory_metadata(
                &manifest,
                project.layout.root(),
                &selection,
            )?;
            self.excluded
                .insert(compiler.target_directory.into_std_path_buf());
            if let Some(build) = compiler.build_directory {
                self.excluded.insert(build.into_std_path_buf());
            }
        }
        if let Ok(home) = super::phoxal_home() {
            self.excluded.insert(home);
        }
        let roots = self.packages.clone();
        for root in roots {
            for ancestor in root.ancestors() {
                for name in [
                    ".cargo/config",
                    ".cargo/config.toml",
                    "rust-toolchain",
                    "rust-toolchain.toml",
                ] {
                    self.files.insert(ancestor.join(name));
                }
            }
        }
        if let Some(home) = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        {
            self.files.insert(home.join("config"));
            self.files.insert(home.join("config.toml"));
        }
        let aliases = self
            .excluded
            .iter()
            .filter_map(|path| resolved_alias(path))
            .collect::<Vec<_>>();
        self.excluded.extend(aliases);
        Ok(())
    }
    fn ignored(&self, path: &Path) -> bool {
        self.excluded.iter().any(|root| path.starts_with(root))
            || path.components().any(|c| {
                matches!(
                    c.as_os_str().to_str(),
                    Some(
                        ".git"
                            | ".idea"
                            | ".vscode"
                            | "target"
                            | ".phoxal"
                            | ".phoxal-home"
                            | "generated"
                    )
                )
            })
            || path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".phoxal-"))
            || path
                .components()
                .any(|c| c.as_os_str().to_string_lossy().ends_with(".generations"))
    }
    fn scan(&self) -> Result<(Snapshot, BTreeSet<PathBuf>), Error> {
        let mut snapshot = Snapshot::new();
        let mut directories = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for root in &self.packages {
            self.tree(root, false, &mut snapshot, &mut directories, &mut visited)?;
        }
        for root in &self.resources {
            self.tree(root, true, &mut snapshot, &mut directories, &mut visited)?;
        }
        for file in &self.files {
            self.file(file, &mut snapshot, &mut directories)?;
        }
        Ok((snapshot, directories))
    }
    fn file(
        &self,
        path: &Path,
        snapshot: &mut Snapshot,
        directories: &mut BTreeSet<PathBuf>,
    ) -> Result<(), Error> {
        if self.ignored(path) {
            return Ok(());
        }
        if let Some(parent) = path.parent()
            && let Some(existing) = parent.ancestors().find(|p| p.is_dir())
        {
            directories.insert(existing.to_owned());
        }
        let bytes = match fs::File::open(path) {
            Ok(mut file) => {
                use std::io::Read;
                let mut hasher = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    cancelled()?;
                    let count = file
                        .read(&mut buffer)
                        .map_err(|e| error(format!("cannot observe {}: {e}", path.display())))?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                }
                hasher.finalize().to_vec()
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
                ) =>
            {
                Vec::new()
            }
            Err(e) => return Err(error(format!("cannot observe {}: {e}", path.display()))),
        };
        snapshot.insert(path.to_owned(), bytes);
        if let Ok(target) = path.canonicalize() {
            if self.ignored(&target) {
                snapshot.remove(path);
                return Ok(());
            }
            if let Some(bytes) = snapshot.get(path).cloned() {
                snapshot.insert(target.clone(), bytes);
            }
            if let Some(parent) = target.parent() {
                directories.insert(parent.to_owned());
            }
            if target != path {
                snapshot.insert(
                    path.join(".symlink-target"),
                    target.as_os_str().as_encoded_bytes().to_vec(),
                );
            }
        }
        Ok(())
    }
    fn tree(
        &self,
        root: &Path,
        resources: bool,
        snapshot: &mut Snapshot,
        directories: &mut BTreeSet<PathBuf>,
        visited: &mut BTreeSet<(PathBuf, bool)>,
    ) -> Result<(), Error> {
        if self.ignored(root) {
            return Ok(());
        }
        let Ok(canonical) = root.canonicalize() else {
            if let Some(parent) = root.ancestors().skip(1).find(|p| p.is_dir()) {
                directories.insert(parent.to_owned());
            }
            return Ok(());
        };
        if !visited.insert((canonical.clone(), resources)) {
            return Ok(());
        }
        if self.ignored(&canonical) {
            return Ok(());
        }
        directories.insert(canonical.clone());
        let entries = match fs::read_dir(&canonical) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(error(format!("cannot observe {}: {e}", root.display()))),
        };
        for entry in entries {
            cancelled()?;
            let entry = entry.map_err(|e| error(e.to_string()))?;
            let path = entry.path();
            if self.ignored(&path) {
                continue;
            }
            if path.is_dir() {
                self.tree(&path, resources, snapshot, directories, visited)?;
            } else if resources
                || path.extension().is_some_and(|ext| ext == "rs")
                || matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("Cargo.toml" | "Cargo.lock")
                )
            {
                self.file(&path, snapshot, directories)?;
            }
        }
        if root != canonical {
            snapshot.insert(
                root.join(".symlink-target"),
                canonical.as_os_str().as_encoded_bytes().to_vec(),
            );
        }
        Ok(())
    }
}

struct Observation {
    inputs: Inputs,
    watcher: RecommendedWatcher,
    events: Receiver<notify::Result<notify::Event>>,
    directories: BTreeMap<PathBuf, (u64, u64)>,
    selected: BTreeSet<PathBuf>,
    revision: u64,
    missed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    backend_error: std::sync::Arc<std::sync::Mutex<Option<notify::Error>>>,
    last_scan: std::time::SystemTime,
}
impl Observation {
    fn new(inputs: Inputs, selected: BTreeSet<PathBuf>) -> Result<Self, Error> {
        let (tx, events) = mpsc::sync_channel(256);
        let missed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend_error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let callback_missed = std::sync::Arc::clone(&missed);
        let callback_error = std::sync::Arc::clone(&backend_error);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Err(e) = event {
                *callback_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
            } else if tx.try_send(event).is_err() {
                callback_missed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        })
        .map_err(|e| error(format!("cannot start source observation: {e}")))?;
        let mut result = Self {
            inputs,
            watcher,
            events,
            directories: BTreeMap::new(),
            selected,
            revision: 0,
            missed,
            backend_error,
            last_scan: std::time::SystemTime::UNIX_EPOCH,
        };
        result.snapshot()?;
        Ok(result)
    }
    fn resolve(&mut self, project: &Project, options: &CargoOptions) -> Result<(), Error> {
        let mut next = Inputs::declared(project);
        next.files.extend(self.selected.iter().cloned());
        let result = next.resolve(project, options);
        if result.is_ok() {
            next.excluded.extend(self.inputs.excluded.iter().cloned());
            self.inputs = next;
        } else {
            self.inputs.extend(next);
        }
        result
    }

    fn snapshot(&mut self) -> Result<Snapshot, Error> {
        cancelled()?;
        if self.missed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.revision = self.revision.wrapping_add(1);
        }
        if let Some(e) = self
            .backend_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && !matches!(e.kind, notify::ErrorKind::PathNotFound)
        {
            return Err(error(format!("source watcher failed: {e}")));
        }
        let (snapshot, directories) = self.inputs.scan()?;
        // Always maintain watches on real directories and selected-file
        // parents; replacement/missed events also get a bounded rescan.
        let identities: BTreeMap<_, _> = directories
            .into_iter()
            .filter_map(|path| fs::metadata(&path).ok().map(|m| (path, (m.dev(), m.ino()))))
            .collect();
        for (path, identity) in &self.directories {
            if identities.get(path) != Some(identity) {
                let _ = self.watcher.unwatch(path);
            }
        }
        let mut registered = BTreeMap::new();
        for (path, identity) in identities {
            if self.directories.get(&path) != Some(&identity) {
                if self.directories.contains_key(&path) {
                    self.revision = self.revision.wrapping_add(1);
                }
                if let Err(e) = self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                    if matches!(e.kind, notify::ErrorKind::PathNotFound) {
                        continue;
                    }
                    return Err(error(format!("cannot observe {}: {e}", path.display())));
                }
            }
            registered.insert(path, identity);
        }
        self.directories = registered;
        for event in self.events.try_iter() {
            if let Ok(event) = &event
                && matches!(
                    event.kind,
                    notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_)
                )
                && event.paths.iter().any(|path| {
                    snapshot.contains_key(path)
                        && !self.inputs.ignored(path)
                        && fs::symlink_metadata(path)
                            .and_then(|m| m.modified())
                            .map_or(true, |modified| modified > self.last_scan)
                })
            {
                self.revision = self.revision.wrapping_add(1);
            }
            if let Err(e) = event {
                // A missing watched directory is repaired by the rescan;
                // other backend failures are fatal rather than healthy idle.
                if !matches!(e.kind, notify::ErrorKind::PathNotFound) {
                    return Err(error(format!("source watcher failed: {e}")));
                }
            }
        }
        self.last_scan = std::time::SystemTime::now();
        Ok(snapshot)
    }
}

struct Attempt<'a, 'p> {
    progress: &'a crate::feedback::Progress<'p>,
    observation: &'a RefCell<Observation>,
    project: &'a Project,
    options: &'a CargoOptions,
    pin: &'a Pin,
    explicit_output: Option<&'a Path>,
    before: Snapshot,
    revision: u64,
}
impl PhaseReporter for Attempt<'_, '_> {
    fn phase(&self, phase: &str) {
        if !crate::owned_process::cancelled() {
            self.progress.phase(phase);
        }
    }
    fn before_publication(&self) -> Result<(), Error> {
        cancelled()?;
        self.phase("verifying build inputs");
        {
            let mut observer = self.observation.borrow_mut();
            if observer.snapshot()? != self.before || observer.revision != self.revision {
                return Err(Error::StaleBuild);
            }
        }
        let project =
            Project::discover_files(self.project.layout.root(), self.project.robot_files())?;
        self.pin.check_declaration(&project)?;
        self.pin
            .check(&Pin::resolve(&project, self.options, self.explicit_output)?)?;
        let mut observation = self.observation.borrow_mut();
        let old_roots = observation.inputs.packages.clone();
        observation.resolve(&project, self.options)?;
        let after = observation.snapshot()?;
        if self.before != after
            || old_roots != observation.inputs.packages
            || observation.revision != self.revision
        {
            return Err(Error::StaleBuild);
        }
        cancelled()?;
        self.phase("publishing verified inputs");
        Ok(())
    }
}

/// Runs immediate build and one coalesced, source-verified attempt at a time.
pub(crate) fn run(
    directory: &Path,
    files: &[PathBuf],
    options: &CargoOptions,
    output: Option<PathBuf>,
) -> Result<(), Error> {
    let _signals = crate::owned_process::WatchSignals::install()
        .map_err(|e| error(format!("cannot own watch signals: {e}")))?;
    let published = std::cell::Cell::new(false);
    let result = run_inner(directory, files, options, output, &published);
    if crate::owned_process::cancelled() {
        crate::owned_process::flush_cancelled();
        crate::owned_process::announce_stop();
        if !crate::owned_process::clean() {
            return Err(Error::WatchFailure {
                source: Box::new(error("watch cleanup failed; cannot certify stopped state")),
                preserved: None,
            });
        }
        crate::feedback::line(if published.get() {
            "Build watcher stopped. Previous build preserved."
        } else {
            "Build watcher stopped. No build published yet."
        });
        return Err(Error::WatchCancelled);
    }
    result.map_err(|source| Error::WatchFailure {
        source: Box::new(source),
        preserved: crate::owned_process::clean().then_some(published.get()),
    })
}
fn run_inner(
    directory: &Path,
    files: &[PathBuf],
    options: &CargoOptions,
    output: Option<PathBuf>,
    published: &std::cell::Cell<bool>,
) -> Result<(), Error> {
    let (project, mut inputs, mut observation, pin) =
        crate::feedback::run("Build", !options.diagnostics_json(), |progress| {
            progress.phase("resolving watch inputs");
            let result = (|| {
                let project = Project::discover_files(directory, files)?;
                let mut inputs = Inputs::declared(&project);
                // Observation exists before metadata, contract preparation or compilation.
                let selected = if files.is_empty() {
                    BTreeSet::from([project.layout.root().join("robot.yaml")])
                } else {
                    files.iter().map(|path| directory.join(path)).collect()
                };
                inputs.files.extend(selected.iter().cloned());
                let observation = Observation::new(inputs.clone(), selected)?;
                inputs.resolve(&project, options)?;
                let pin = Pin::resolve(&project, options, output.as_deref())?;
                Ok::<_, Error>((project, inputs, observation, pin))
            })();
            progress.suspend();
            result
        })?;
    if let Some(parent) = pin.output.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| error(format!("cannot create watch output parent: {e}")))?;
    }
    inputs.excluded.insert(pin.output.clone());
    if let Some(name) = pin.output.file_name() {
        inputs.excluded.insert(
            pin.output
                .with_file_name(format!("{}.generations", name.to_string_lossy())),
        );
    }
    observation.inputs.extend(inputs);
    let _owner = OutputOwner {
        _lock: super::publish::acquire_publication_lock(&pin.output)?,
    };
    OUTPUT.with(|held| *held.borrow_mut() = Some(pin.output.clone()));
    super::publish::validate_output(&pin.output)?;
    published.set(super::publish::verified_current(&pin.output)?);
    let observation = RefCell::new(observation);
    let mut seen = observation.borrow_mut().snapshot()?;
    let mut graph_retry = None;
    let mut immediate = true;
    let mut pending = None;
    loop {
        cancelled()?;
        if graph_retry.is_some_and(|time: Instant| time.elapsed() >= Duration::from_secs(2)) {
            graph_retry = Some(Instant::now());
            if let Ok(next) = Project::discover_files(directory, files) {
                crate::feedback::run("Build", !options.diagnostics_json(), |progress| {
                    progress.phase("resolving watch inputs");
                    let result = (|| {
                        let mut observer = observation.borrow_mut();
                        if observer.resolve(&next, options).is_ok() {
                            graph_retry = None;
                            if observer.snapshot()? != seen {
                                pending = Some(Instant::now());
                            }
                        }
                        cancelled()
                    })();
                    progress.suspend();
                    result
                })?;
                crate::feedback::line("Watching for changes.");
            }
        }
        let current = observation.borrow_mut().snapshot()?;
        if current != seen {
            seen = current;
            pending = Some(Instant::now());
        }
        if immediate || pending.is_some_and(|time| time.elapsed() >= QUIET) {
            immediate = false;
            pending = None;
            let started_inputs = observation.borrow_mut().snapshot()?;
            let attempt = crate::feedback::run("Build", !options.diagnostics_json(), |progress| {
                let result = (|| {
                    cancelled()?;
                    let _operation = super::participant::operation_lock(project.layout.root())?;
                    let next = Project::discover_files(directory, files)?;
                    pin.check_declaration(&next)?;
                    pin.check(&Pin::resolve(&next, options, output.as_deref())?)?;
                    let mut observer = observation.borrow_mut();
                    observer.resolve(&next, options)?;
                    let before = observer.snapshot()?;
                    let revision = observer.revision;
                    // The composed declaration must be exactly the bytes
                    // covered by this attempt, including edits during resolve.
                    if next
                        .inputs
                        .iter()
                        .any(|(path, bytes)| fs::read(path).ok().as_ref() != Some(bytes))
                    {
                        return Err(Error::StaleBuild);
                    }
                    drop(observer);
                    let reporter = Attempt {
                        progress,
                        observation: &observation,
                        project: &next,
                        options,
                        pin: &pin,
                        explicit_output: output.as_deref(),
                        before,
                        revision,
                    };
                    let prepared = next.prepare(options, &reporter)?;
                    prepared.build_bundle(options, &pin.output, &reporter)
                })();
                progress.suspend();
                result
            });
            if !crate::owned_process::clean() {
                return Err(error(
                    "owned subprocess cleanup failed; cannot certify stopped state or resume watching",
                ));
            }
            if attempt.is_ok() {
                published.set(true);
            }
            if crate::owned_process::cancelled() {
                continue;
            }
            match attempt {
                Ok(bundle) => {
                    published.set(true);
                    graph_retry = None;
                    let verb = match bundle.publication() {
                        super::publish::PublicationOutcome::Published { .. } => "published",
                        super::publish::PublicationOutcome::Unchanged { .. } => "unchanged",
                    };
                    crate::output::status(
                        options.diagnostics_json(),
                        &format!("Build {verb}: {}", pin.output.display()),
                    );
                }
                Err(Error::Watch { message }) => return Err(error(message)),
                Err(Error::StaleBuild) => {
                    crate::feedback::line("Inputs changed during build. Rebuilding.");
                    pending = Some(Instant::now());
                }
                Err(e) => {
                    graph_retry = Some(Instant::now());
                    crate::output::error(&e, options.diagnostics_json(), files);
                    crate::feedback::line(if published.get() {
                        "Previous build preserved."
                    } else {
                        "No build published yet."
                    });
                }
            }
            seen = observation.borrow_mut().snapshot()?;
            if seen != started_inputs {
                pending = Some(Instant::now());
            }
            crate::feedback::line("Watching for changes.");
            crate::feedback::line("Watching robot sources. Press Ctrl-C to stop.");
        }
        std::thread::sleep(RESCAN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_follow_replacement_symlinks_and_skip_generated_unselected_inputs()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::create_dir(root.join("src"))?;
        fs::create_dir(root.join("external"))?;
        fs::write(root.join("external/actual.rs"), "old")?;
        std::os::unix::fs::symlink(root.join("external/actual.rs"), root.join("src/main.rs"))?;
        let mut inputs = Inputs::default();
        inputs.packages.insert(root.join("src"));
        inputs.files.insert(root.join("selected.yaml"));
        fs::write(root.join("selected.yaml"), "selected")?;
        let before = inputs.scan()?.0;
        fs::write(root.join("external/actual.rs"), "new")?;
        assert_ne!(before, inputs.scan()?.0);
        let before = inputs.scan()?.0;
        fs::write(root.join("unselected.yaml"), "bad")?;
        fs::create_dir(root.join("src/target"))?;
        fs::write(root.join("src/target/generated.rs"), "generated")?;
        assert_eq!(before, inputs.scan()?.0);
        fs::rename(root.join("src"), root.join("old-src"))?;
        fs::create_dir(root.join("src"))?;
        fs::write(root.join("src/main.rs"), "replacement")?;
        assert_ne!(before, inputs.scan()?.0);
        fs::remove_file(root.join("selected.yaml"))?;
        let deleted = inputs.scan()?.0;
        fs::write(root.join("selected.yaml"), "restored")?;
        assert_ne!(deleted, inputs.scan()?.0);
        Ok(())
    }
    #[test]
    fn independent_graphs_and_both_external_compiler_roots_are_observed_correctly()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("robot");
        for (name, dependency) in [
            ("robot", None),
            ("provider", Some("leaf")),
            ("leaf", None),
            ("supervisor", Some("supervisor-leaf")),
            ("supervisor-leaf", None),
        ] {
            let package = directory.path().join(name);
            fs::create_dir_all(package.join("src"))?;
            let mut manifest =
                format!("[package]\nname={name:?}\nversion='0.1.0'\nedition='2024'\n");
            if let Some(dep) = dependency {
                manifest += &format!("[dependencies]\n{dep}={{path='../{dep}'}}\n");
            }
            fs::write(package.join("Cargo.toml"), manifest)?;
            fs::write(
                package.join(if name.contains("leaf") {
                    "src/lib.rs"
                } else {
                    "src/main.rs"
                }),
                "fn main(){}\n",
            )?;
        }
        fs::write(
            root.join("robot.yaml"),
            "schema: phoxal/robot/v0\nrobot:\n  id: watch-graphs\n  services:\n    independent: {source: {path: '../provider'}}\nsupervisor: {source: {path: '../supervisor'}}\n",
        )?;
        let target = directory.path().join("external-final");
        let build = directory.path().join("external-intermediates");
        fs::create_dir(root.join(".cargo"))?;
        fs::write(
            root.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir={target:?}\nbuild-dir={build:?}\n"),
        )?;
        let project = Project::discover(&root)?;
        let options = CargoOptions {
            offline: true,
            ..CargoOptions::default()
        };
        let mut inputs = Inputs::declared(&project);
        inputs.resolve(&project, &options)?;
        assert!(
            inputs
                .packages
                .contains(&directory.path().join("leaf").canonicalize()?)
        );
        assert!(
            inputs
                .packages
                .contains(&directory.path().join("supervisor-leaf").canonicalize()?)
        );
        assert!(inputs.excluded.contains(&target));
        // Cargo1.88 omits build_directory, so only newer Cargo can expose it.
        let metadata = super::super::cargo::compiler_directory_metadata(
            &root.join("Cargo.toml"),
            &root,
            &options,
        )?;
        if metadata.build_directory.is_some() {
            assert!(inputs.excluded.contains(&build));
        }
        fs::create_dir_all(&target)?;
        fs::write(target.join("generated.rs"), "output")?;
        inputs.files.insert(target.join("generated.rs"));
        assert!(!inputs.scan()?.0.contains_key(&target.join("generated.rs")));
        Ok(())
    }
}
