//! GitHub API submission of one Cargo package to the registry.
//!
//! This module never invokes Git or GitHub CLI.
//! It authenticates through an explicitly supplied environment token, a
//! credential previously stored by this tool, or GitHub's bounded OAuth device
//! flow, then creates immutable Git objects and an upstream pull request.

use std::env;
use std::fs;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use flate2::read::GzDecoder;
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::project::{Error, PublicationError, PublicationResult};

const API: &str = "https://api.github.com";
const OWNER: &str = "phoxal";
const REPOSITORY: &str = "registry";
const TOKEN_ENV: &str = "PHOXAL_GITHUB_TOKEN";
const CLIENT_ID_ENV: &str = "PHOXAL_GITHUB_CLIENT_ID";
const KEYRING_SERVICE: &str = "org.phoxal.cargo-phoxal";
const KEYRING_ACCOUNT: &str = "github.com";
const USER_AGENT: &str = "cargo-phoxal";
const CRATES_IO_INDEX: &str = "https://github.com/rust-lang/crates.io-index";
const MAX_GIT_BLOB_BYTES: usize = 100 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// A browser authorization prompt emitted before device-flow polling starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceAuthorization {
    /// The short code the user enters in the browser.
    pub user_code: String,
    /// GitHub's browser verification URL.
    pub verification_uri: String,
    /// Maximum time for this authorization attempt.
    pub expires_in: Duration,
}

/// The durable state returned by a normal publication command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubmissionResult {
    /// Identical bytes are already deployed and publicly retrievable.
    Available {
        /// Public archive URL whose bytes were verified.
        archive_url: String,
    },
    /// Exact bytes are awaiting human review in a pull request.
    PendingReview {
        /// Existing or newly opened pull request URL.
        pull_request_url: String,
        /// Contributor publication branch.
        branch: String,
    },
}

/// Submit a prepared archive through GitHub's HTTPS APIs.
///
/// `authorize` is called only when no explicit or stored credential is usable.
/// It should display the code and URL without recording the opaque device code.
pub fn submit_publication(
    publication: &PublicationResult,
    authorize: impl FnOnce(&DeviceAuthorization),
) -> Result<SubmissionResult, Error> {
    if publication.bytes() as usize > MAX_GIT_BLOB_BYTES {
        return Err(PublicationError::SubmissionArchiveTooLarge {
            bytes: publication.bytes(),
            maximum: MAX_GIT_BLOB_BYTES as u64,
        }
        .into());
    }
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(submission_transport)?;
    let credential = authenticate(&client, authorize)?;
    let github = GitHub::new(client, credential.access_token);
    // Publication branches live in the registry repository itself. The
    // submitter may be a user token or a GitHub App installation token;
    // neither an authenticated-user lookup nor a personal fork is part
    // of the submission path.
    submit(&github, publication)
}

fn submit(github: &GitHub, publication: &PublicationResult) -> Result<SubmissionResult, Error> {
    let repository = github.get::<Repository>(&format!("/repos/{OWNER}/{REPOSITORY}"))?;
    let base = repository.default_branch;
    let index_path = index_path(publication.package())?;
    let archive_path = archive_path(publication.package(), publication.version())?;

    let mut index = github
        .raw_optional(OWNER, REPOSITORY, &base, &index_path)?
        .unwrap_or_default();
    if let Some(existing) = find_index_record(&index, publication.version())? {
        let checksum = existing
            .get("cksum")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if checksum != publication.checksum() {
            return Err(PublicationError::SubmissionConflict {
                message: format!(
                    "{} {} already has a different registry checksum",
                    publication.package(),
                    publication.version()
                ),
            }
            .into());
        }
        let public_url = format!("https://phoxal.github.io/registry/{archive_path}");
        let deployed = github.get_absolute_bytes(&public_url)?;
        verify_checksum(&deployed, publication.checksum(), "deployed archive")?;
        return Ok(SubmissionResult::Available {
            archive_url: public_url,
        });
    }

    let archive =
        fs::read(publication.archive()).map_err(|source| PublicationError::CaptureSource {
            path: publication.archive().to_owned(),
            source,
        })?;
    verify_checksum(&archive, publication.checksum(), "prepared archive")?;
    let record = registry_record(&archive, publication)?;
    if !index.is_empty() && !index.ends_with(b"\n") {
        return Err(PublicationError::SubmissionConflict {
            message: format!("registry index {index_path} has no terminal newline"),
        }
        .into());
    }
    index.extend_from_slice(
        serde_json::to_string(&record)
            .map_err(submission_json)?
            .as_bytes(),
    );
    index.push(b'\n');

    let branch = publication_branch(publication);
    let existing_ref = github.get_optional::<GitRef>(&format!(
        "/repos/{OWNER}/{REPOSITORY}/git/ref/heads/{branch}"
    ))?;
    if existing_ref.is_some() {
        let pending_archive = github
            .raw_optional(OWNER, REPOSITORY, &branch, &archive_path)?
            .ok_or_else(|| PublicationError::SubmissionConflict {
                message: format!("publication branch {branch} has no archive"),
            })?;
        verify_checksum(&pending_archive, publication.checksum(), "pending archive")?;
        verify_pending_file(github, &branch, &index_path, &index)?;
        if let Some(pull) = existing_pull_request(github, &branch)? {
            return Ok(SubmissionResult::PendingReview {
                pull_request_url: pull.html_url,
                branch,
            });
        }
    } else {
        let base_ref =
            github.get::<GitRef>(&format!("/repos/{OWNER}/{REPOSITORY}/git/ref/heads/{base}"))?;
        let commit = github.get::<GitCommit>(&format!(
            "/repos/{OWNER}/{REPOSITORY}/git/commits/{}",
            base_ref.object.sha
        ))?;
        let entries = vec![
            tree_entry(github, &archive_path, &archive, true)?,
            tree_entry(github, &index_path, &index, false)?,
        ];
        let tree = github.post::<GitTree>(
            &format!("/repos/{OWNER}/{REPOSITORY}/git/trees"),
            &json!({"base_tree": commit.tree.sha, "tree": entries}),
        )?;
        let commit = github.post::<GitCommitCreated>(
            &format!("/repos/{OWNER}/{REPOSITORY}/git/commits"),
            &json!({
                "message": format!("publish: {} {}", publication.package(), publication.version()),
                "tree": tree.sha,
                "parents": [base_ref.object.sha],
            }),
        )?;
        github.post_unit(
            &format!("/repos/{OWNER}/{REPOSITORY}/git/refs"),
            &json!({"ref": format!("refs/heads/{branch}"), "sha": commit.sha}),
        )?;
    }

    let pull = github.post::<PullRequest>(
        &format!("/repos/{OWNER}/{REPOSITORY}/pulls"),
        &json!({
            "title": format!("publish: {} {}", publication.package(), publication.version()),
            "head": branch,
            "base": base,
            "body": format!(
                "Publishes `{}` `{}` to the registry.\n\nArchive SHA-256: `{}`\nContent role: `{}`\n",
                publication.package(),
                publication.version(),
                publication.checksum(),
                publication.registry_kind(),
            ),
        }),
    )?;
    Ok(SubmissionResult::PendingReview {
        pull_request_url: pull.html_url,
        branch,
    })
}

fn package_stem(publication: &PublicationResult) -> String {
    format!("{}-{}", publication.package(), publication.version())
}

fn publication_branch(publication: &PublicationResult) -> String {
    format!(
        "publish/{}/{}/{}",
        publication.package(),
        publication.version(),
        &publication.checksum()[..12]
    )
}

fn index_path(name: &str) -> Result<String, Error> {
    let lower = name.to_ascii_lowercase();
    if !(1..=64).contains(&name.len())
        || !name.as_bytes()[0].is_ascii_lowercase()
        || lower != name
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(PublicationError::SubmissionConflict {
            message: format!("package name {name:?} is not a canonical lowercase registry name"),
        }
        .into());
    }
    Ok(match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    })
}

fn archive_path(name: &str, version: &str) -> Result<String, Error> {
    Ok(format!("crates/{}/{}.crate", index_path(name)?, version))
}

fn registry_record(archive: &[u8], publication: &PublicationResult) -> Result<Value, Error> {
    let manifest = archive_manifest(archive, publication)?;
    let package = manifest
        .get("package")
        .and_then(ValueExt::table)
        .ok_or_else(|| PublicationError::SubmissionConflict {
            message: "normalized Cargo.toml has no package table".to_owned(),
        })?;
    let mut dependencies = Vec::new();
    dependency_tables(&manifest, None, &mut dependencies)?;
    if let Some(targets) = manifest.get("target").and_then(ValueExt::table) {
        for (target, value) in targets {
            dependency_tables(value, Some(target), &mut dependencies)?;
        }
    }
    dependencies.sort_by_key(Value::to_string);
    let features = manifest
        .get("features")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let mut record = serde_json::Map::from_iter([
        ("name".to_owned(), json!(publication.package())),
        ("vers".to_owned(), json!(publication.version())),
        ("deps".to_owned(), Value::Array(dependencies)),
        ("cksum".to_owned(), json!(publication.checksum())),
        ("features".to_owned(), features),
        ("yanked".to_owned(), Value::Bool(false)),
        ("v".to_owned(), Value::Number(2.into())),
    ]);
    if let Some(value) = package.get("rust-version").and_then(Value::as_str) {
        record.insert("rust_version".to_owned(), json!(value));
    }
    if let Some(value) = package.get("links").and_then(Value::as_str) {
        record.insert("links".to_owned(), json!(value));
    }
    Ok(Value::Object(record))
}

trait ValueExt {
    fn table(&self) -> Option<&serde_json::Map<String, Value>>;
}

impl ValueExt for Value {
    fn table(&self) -> Option<&serde_json::Map<String, Value>> {
        self.as_object()
    }
}

fn archive_manifest(archive: &[u8], publication: &PublicationResult) -> Result<Value, Error> {
    let decoder = GzDecoder::new(archive);
    let mut archive = Archive::new(decoder);
    let expected = format!("{}/Cargo.toml", package_stem(publication));
    let entries = archive.entries().map_err(submission_transport)?;
    for entry in entries {
        let entry = entry.map_err(submission_transport)?;
        let path = entry.path().map_err(submission_transport)?;
        if path.to_string_lossy() != expected {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(submission_transport)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(PublicationError::SubmissionConflict {
                message: "normalized Cargo.toml exceeds 4 MiB".to_owned(),
            }
            .into());
        }
        let manifest = toml::from_slice::<toml::Value>(&bytes).map_err(|error| {
            PublicationError::SubmissionConflict {
                message: format!("normalized Cargo.toml is invalid: {error}"),
            }
        })?;
        return serde_json::to_value(manifest)
            .map_err(submission_json)
            .map_err(Into::into);
    }
    Err(PublicationError::SubmissionConflict {
        message: "prepared archive has no normalized Cargo.toml".to_owned(),
    }
    .into())
}

fn dependency_tables(
    manifest: &Value,
    target: Option<&String>,
    output: &mut Vec<Value>,
) -> Result<(), Error> {
    for (section, kind) in [
        ("dependencies", "normal"),
        ("build-dependencies", "build"),
        ("dev-dependencies", "dev"),
    ] {
        let Some(table) = manifest.get(section).and_then(ValueExt::table) else {
            continue;
        };
        for (alias, value) in table {
            let (requirement, features, optional, default_features, registry, package) =
                if let Some(requirement) = value.as_str() {
                    (requirement, Vec::new(), false, true, None, None)
                } else {
                    let fields =
                        value
                            .as_object()
                            .ok_or_else(|| PublicationError::SubmissionConflict {
                                message: format!("dependency {alias} is not normalized"),
                            })?;
                    let requirement =
                        fields
                            .get("version")
                            .and_then(Value::as_str)
                            .ok_or_else(|| PublicationError::SubmissionConflict {
                                message: format!("dependency {alias} has no version"),
                            })?;
                    let features = fields
                        .get("features")
                        .and_then(Value::as_array)
                        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    (
                        requirement,
                        features,
                        fields
                            .get("optional")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        fields
                            .get("default-features")
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                        fields.get("registry-index").and_then(Value::as_str),
                        fields.get("package").and_then(Value::as_str),
                    )
                };
            let registry = match registry {
                Some(crate::project::cargo::PHOXAL_REGISTRY_INDEX) => Value::Null,
                Some(value) => json!(value),
                None => json!(CRATES_IO_INDEX),
            };
            let mut record = serde_json::Map::from_iter([
                ("name".to_owned(), json!(alias)),
                ("req".to_owned(), json!(requirement)),
                ("features".to_owned(), json!(features)),
                ("optional".to_owned(), json!(optional)),
                ("default_features".to_owned(), json!(default_features)),
                ("kind".to_owned(), json!(kind)),
                ("registry".to_owned(), registry),
                (
                    "target".to_owned(),
                    target.map_or(Value::Null, |value| json!(value)),
                ),
            ]);
            if let Some(package) = package {
                record.insert("package".to_owned(), json!(package));
            }
            output.push(Value::Object(record));
        }
    }
    Ok(())
}

fn find_index_record(index: &[u8], version: &str) -> Result<Option<Value>, Error> {
    for line in index
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line).map_err(submission_json)?;
        if record.get("vers").and_then(Value::as_str) == Some(version) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn verify_checksum(bytes: &[u8], expected: &str, label: &str) -> Result<(), Error> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(PublicationError::SubmissionConflict {
            message: format!("{label} checksum is {actual}, expected {expected}"),
        }
        .into());
    }
    Ok(())
}

fn tree_entry(github: &GitHub, path: &str, bytes: &[u8], binary: bool) -> Result<Value, Error> {
    let blob = github.post::<GitBlob>(
        &format!("/repos/{OWNER}/{REPOSITORY}/git/blobs"),
        &if binary {
            json!({
                "content": base64::engine::general_purpose::STANDARD.encode(bytes),
                "encoding": "base64",
            })
        } else {
            json!({
                "content": String::from_utf8(bytes.to_vec()).map_err(|error| PublicationError::SubmissionConflict { message: error.to_string() })?,
                "encoding": "utf-8",
            })
        },
    )?;
    Ok(json!({"path": path, "mode": "100644", "type": "blob", "sha": blob.sha}))
}

fn existing_pull_request(github: &GitHub, branch: &str) -> Result<Option<PullRequest>, Error> {
    // The list filter wants the owner-qualified form; a bare branch
    // name is silently ignored and would return every open pull.
    let pulls = github.get_query::<Vec<PullRequest>>(
        &format!("/repos/{OWNER}/{REPOSITORY}/pulls"),
        &[("state", "open"), ("head", &format!("{OWNER}:{branch}"))],
    )?;
    // Defensive: only accept a pull whose head really is this branch,
    // never whatever the filter happened to return.
    Ok(pulls.into_iter().find(|pull| {
        pull.head
            .as_ref()
            .is_some_and(|head| head.reference == branch)
    }))
}

fn verify_pending_file(
    github: &GitHub,
    branch: &str,
    path: &str,
    expected: &[u8],
) -> Result<(), Error> {
    let actual = github
        .raw_optional(OWNER, REPOSITORY, branch, path)?
        .ok_or_else(|| PublicationError::SubmissionConflict {
            message: format!("publication branch {branch} has no {path}"),
        })?;
    if actual != expected {
        return Err(PublicationError::SubmissionConflict {
            message: format!("publication branch {branch} has different bytes at {path}"),
        }
        .into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredCredential {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
}

fn authenticate(
    client: &Client,
    authorize: impl FnOnce(&DeviceAuthorization),
) -> Result<StoredCredential, Error> {
    if let Some(token) = env::var_os(TOKEN_ENV) {
        let token = token
            .into_string()
            .map_err(|_| PublicationError::Authentication {
                message: format!("{TOKEN_ENV} is not valid UTF-8"),
            })?;
        if token.is_empty() {
            return Err(PublicationError::Authentication {
                message: format!("{TOKEN_ENV} is empty"),
            }
            .into());
        }
        return Ok(StoredCredential {
            access_token: token,
            refresh_token: None,
            expires_at: None,
        });
    }

    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(keyring_error)?;
    if let Ok(encoded) = entry.get_password()
        && let Ok(mut credential) = serde_json::from_str::<StoredCredential>(&encoded)
    {
        if credential
            .expires_at
            .is_some_and(|expires| expires <= unix_now().saturating_add(60))
            && let Some(refresh) = credential.refresh_token.clone()
        {
            match refresh_credential(client, &refresh) {
                Ok(refreshed) => {
                    credential = refreshed;
                    store_credential(&entry, &credential)?;
                }
                Err(_) => {
                    let _ = entry.delete_credential();
                    credential.access_token.clear();
                }
            }
        }
        if !credential.access_token.is_empty() && validate_token(client, &credential.access_token)?
        {
            return Ok(credential);
        }
        let _ = entry.delete_credential();
    }

    let client_id = env::var(CLIENT_ID_ENV)
        .ok()
        .or_else(|| option_env!("PHOXAL_GITHUB_CLIENT_ID").map(str::to_owned))
        .ok_or_else(|| PublicationError::Authentication {
            message: format!(
                "this build has no Phoxal GitHub OAuth client id; set public {CLIENT_ID_ENV} for a development build or use {TOKEN_ENV}"
            ),
        })?;
    let requested = request_device_code(client, &client_id)?;
    authorize(&DeviceAuthorization {
        user_code: requested.user_code.clone(),
        verification_uri: requested.verification_uri.clone(),
        expires_in: Duration::from_secs(requested.expires_in),
    });
    let credential = poll_device_code(client, &client_id, requested)?;
    if !validate_token(client, &credential.access_token)? {
        return Err(PublicationError::Authentication {
            message: "GitHub returned a credential that cannot identify its user".to_owned(),
        }
        .into());
    }
    store_credential(&entry, &credential)?;
    Ok(credential)
}

fn store_credential(entry: &keyring::Entry, credential: &StoredCredential) -> Result<(), Error> {
    let encoded = serde_json::to_string(credential).map_err(submission_json)?;
    entry.set_password(&encoded).map_err(keyring_error)
}

fn validate_token(client: &Client, token: &str) -> Result<bool, Error> {
    let response = client
        .get(format!("{API}/user"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .bearer_auth(token)
        .send()
        .map_err(submission_transport)?;
    match response.status() {
        StatusCode::OK => Ok(true),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Ok(false),
        _ => Err(response_error(response)),
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

fn request_device_code(client: &Client, client_id: &str) -> Result<DeviceCode, Error> {
    send_json(
        client
            .post("https://github.com/login/device/code")
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .form(&[("client_id", client_id), ("scope", "public_repo")]),
    )
}

fn poll_device_code(
    client: &Client,
    client_id: &str,
    requested: DeviceCode,
) -> Result<StoredCredential, Error> {
    let deadline = Instant::now() + Duration::from_secs(requested.expires_in);
    let mut interval = requested.interval.max(1);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(interval));
        let response: TokenResponse = send_json(
            client
                .post("https://github.com/login/oauth/access_token")
                .header("Accept", "application/json")
                .header("User-Agent", USER_AGENT)
                .form(&[
                    ("client_id", client_id),
                    ("device_code", requested.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
        )?;
        if let Some(access_token) = response.access_token {
            let permitted = response.scope.as_deref().is_some_and(|scopes| {
                scopes
                    .split(',')
                    .map(str::trim)
                    .any(|scope| scope == "public_repo")
            });
            if !permitted {
                return Err(PublicationError::Authentication {
                    message: "GitHub authorization did not grant the required public_repo scope"
                        .to_owned(),
                }
                .into());
            }
            return Ok(StoredCredential {
                access_token,
                refresh_token: response.refresh_token,
                expires_at: response
                    .expires_in
                    .map(|duration| unix_now().saturating_add(duration)),
            });
        }
        match response.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = response.interval.unwrap_or(interval + 5),
            Some(error) => {
                return Err(PublicationError::Authentication {
                    message: response
                        .error_description
                        .unwrap_or_else(|| error.to_owned()),
                }
                .into());
            }
            None => {
                return Err(PublicationError::Authentication {
                    message: "GitHub device flow returned neither a token nor an error".to_owned(),
                }
                .into());
            }
        }
    }
    Err(PublicationError::Authentication {
        message: "GitHub device authorization expired".to_owned(),
    }
    .into())
}

fn refresh_credential(client: &Client, refresh_token: &str) -> Result<StoredCredential, Error> {
    let client_id = env::var(CLIENT_ID_ENV)
        .ok()
        .or_else(|| option_env!("PHOXAL_GITHUB_CLIENT_ID").map(str::to_owned))
        .ok_or_else(|| PublicationError::Authentication {
            message: format!("cannot refresh GitHub credential without {CLIENT_ID_ENV}"),
        })?;
    let response: TokenResponse = send_json(
        client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .form(&[
                ("client_id", client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ]),
    )?;
    let access_token = response
        .access_token
        .ok_or_else(|| PublicationError::Authentication {
            message: response
                .error_description
                .or(response.error)
                .unwrap_or_else(|| "GitHub credential refresh failed".to_owned()),
        })?;
    Ok(StoredCredential {
        access_token,
        refresh_token: response.refresh_token,
        expires_at: response
            .expires_in
            .map(|duration| unix_now().saturating_add(duration)),
    })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct GitHub {
    client: Client,
    token: String,
}

impl GitHub {
    fn new(client: Client, token: String) -> Self {
        Self { client, token }
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .bearer_auth(&self.token)
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, Error> {
        send_json(self.request(self.client.get(format!("{API}{path}"))))
    }

    fn get_query<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(impl AsRef<str>, impl AsRef<str>)],
    ) -> Result<T, Error> {
        let pairs = query
            .iter()
            .map(|(key, value)| (key.as_ref(), value.as_ref()))
            .collect::<Vec<_>>();
        send_json(self.request(self.client.get(format!("{API}{path}")).query(&pairs)))
    }

    fn get_optional<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Option<T>, Error> {
        let response = self
            .request(self.client.get(format!("{API}{path}")))
            .send()
            .map_err(submission_transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse_response(response).map(Some)
    }

    fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, body: &Value) -> Result<T, Error> {
        send_json(self.request(self.client.post(format!("{API}{path}")).json(body)))
    }

    fn post_unit(&self, path: &str, body: &Value) -> Result<(), Error> {
        let response = self
            .request(self.client.post(format!("{API}{path}")).json(body))
            .send()
            .map_err(submission_transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(response_error(response))
        }
    }

    fn raw_optional(
        &self,
        owner: &str,
        repository: &str,
        revision: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, Error> {
        let url =
            format!("https://raw.githubusercontent.com/{owner}/{repository}/{revision}/{path}");
        let response = self
            .request(self.client.get(url))
            .send()
            .map_err(submission_transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response_bytes(response).map(Some)
    }

    fn get_absolute_bytes(&self, url: &str) -> Result<Vec<u8>, Error> {
        let response = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .send()
            .map_err(submission_transport)?;
        response_bytes(response)
    }
}

fn send_json<T: for<'de> Deserialize<'de>>(request: RequestBuilder) -> Result<T, Error> {
    let response = request.send().map_err(submission_transport)?;
    parse_response(response)
}

fn parse_response<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, Error> {
    if !response.status().is_success() {
        return Err(response_error(response));
    }
    let bytes = response.bytes().map_err(submission_transport)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(PublicationError::SubmissionTransport {
            message: "GitHub JSON response exceeds 2 MiB".to_owned(),
        }
        .into());
    }
    serde_json::from_slice(&bytes)
        .map_err(submission_json)
        .map_err(Into::into)
}

fn response_bytes(response: Response) -> Result<Vec<u8>, Error> {
    if !response.status().is_success() {
        return Err(response_error(response));
    }
    let bytes = response.bytes().map_err(submission_transport)?;
    if bytes.len() > MAX_GIT_BLOB_BYTES {
        return Err(PublicationError::SubmissionConflict {
            message: "GitHub response exceeds the supported archive bound".to_owned(),
        }
        .into());
    }
    Ok(bytes.to_vec())
}

fn response_error(response: Response) -> Error {
    let status = response.status();
    let body = response
        .bytes()
        .ok()
        .map(|bytes| {
            let limit = bytes.len().min(MAX_RESPONSE_BYTES);
            String::from_utf8_lossy(&bytes[..limit]).into_owned()
        })
        .unwrap_or_default();
    PublicationError::GitHub {
        status: status.as_u16(),
        message: body,
    }
    .into()
}

fn submission_transport(error: impl std::fmt::Display) -> PublicationError {
    PublicationError::SubmissionTransport {
        message: error.to_string(),
    }
}

fn submission_json(error: impl std::fmt::Display) -> PublicationError {
    PublicationError::SubmissionConflict {
        message: error.to_string(),
    }
}

fn keyring_error(error: keyring::Error) -> Error {
    PublicationError::CredentialStore {
        message: error.to_string(),
    }
    .into()
}

#[derive(Debug, Deserialize)]
struct Repository {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct GitObject {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitRef {
    object: GitObject,
}

#[derive(Debug, Deserialize)]
struct GitTreeRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitCommit {
    tree: GitTreeRef,
}

#[derive(Debug, Deserialize)]
struct GitBlob {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitTree {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitCommitCreated {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PullRequest {
    html_url: String,
    head: Option<PullRequestHead>,
}

#[derive(Debug, Deserialize)]
struct PullRequestHead {
    #[serde(rename = "ref")]
    reference: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_sparse_index_paths_are_canonical() {
        assert_eq!(index_path("a").expect("one"), "1/a");
        assert_eq!(index_path("ab").expect("two"), "2/ab");
        assert_eq!(index_path("abc").expect("three"), "3/a/abc");
        assert_eq!(
            index_path("phoxal-port").expect("long"),
            "ph/ox/phoxal-port"
        );
        assert_eq!(
            archive_path("phoxal-port", "0.0.0-dev.1").expect("archive"),
            "crates/ph/ox/phoxal-port/0.0.0-dev.1.crate"
        );
    }

    #[test]
    fn index_lookup_is_exact_and_rejects_malformed_json() {
        let bytes = b"{\"vers\":\"0.1.0\",\"cksum\":\"abc\"}\n";
        assert!(find_index_record(bytes, "0.1.0").expect("valid").is_some());
        assert!(find_index_record(bytes, "0.2.0").expect("valid").is_none());
        assert!(find_index_record(b"not-json\n", "0.1.0").is_err());
    }
}
