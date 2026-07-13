use chrono::{DateTime, Local, Utc};
use rusqlite::{backup::Backup, params, Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Once,
    },
    time::{Duration, SystemTime},
};
use walkdir::WalkDir;

pub const ALLOWED_PROVIDERS: &[&str] = &["openai", "custom", "codexpilot"];

fn canonical_provider(id: &str) -> &'static str {
    match normalize_provider(id).as_str() {
        "openai" => "OpenAI",
        "custom" => "custom",
        "codexpilot" => "CodexPilot",
        _ => "",
    }
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub color: String,
    pub sessions: usize,
    pub indexed: usize,
    pub status: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SourceSummary {
    pub name: String,
    pub path: String,
    pub records: usize,
    pub readable: bool,
    pub note: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LockSummary {
    pub state: String,
    pub path: String,
    pub owner_pid: Option<u32>,
    pub age_seconds: Option<u64>,
    pub active_processes: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub codex_home: String,
    pub current_provider: String,
    pub providers: Vec<ProviderSummary>,
    pub sessions: usize,
    pub discovered_sessions: usize,
    pub orphaned_sessions: usize,
    pub archived_sessions: usize,
    pub ordinary_sessions: usize,
    pub recoverable_sessions: usize,
    pub recoverable_indexed: usize,
    pub session_index_covered: usize,
    pub remote_sessions: usize,
    pub remote_excluded_sessions: usize,
    pub automated_sessions: usize,
    pub rollout_sessions: usize,
    pub valid_rollout_sessions: usize,
    pub indexed: usize,
    pub session_indexed: usize,
    pub drift: usize,
    pub provider_drift: usize,
    pub rollout_provider_drift: usize,
    pub missing_catalog: usize,
    pub missing_rollout: usize,
    pub skipped: usize,
    pub sqlite: usize,
    pub jsonl: usize,
    pub lock: String,
    pub lock_detail: LockSummary,
    pub needs_admin: bool,
    pub last_backup: Option<String>,
    pub sources: Vec<SourceSummary>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BackupResult {
    pub path: String,
    pub files: Vec<String>,
    pub manifest: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SkipReason {
    pub thread_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RepairResult {
    pub changed: usize,
    pub providers_fixed: usize,
    pub index_added: usize,
    pub skipped: usize,
    pub skipped_reasons: Vec<SkipReason>,
    pub dry_run: bool,
    pub verified: bool,
    pub backup_path: Option<String>,
    pub lock: String,
    pub needs_admin: bool,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    pub ok: bool,
    pub checked: usize,
    pub remaining: usize,
    pub skipped: usize,
    pub reasons: Vec<SkipReason>,
}

#[derive(Debug, Clone)]
struct ThreadRow {
    id: String,
    provider: String,
    archived: bool,
    source: String,
    thread_source: String,
    agent_role: Option<String>,
    title: String,
    cwd: String,
    created_at: f64,
    updated_at: f64,
}

#[derive(Debug, Clone)]
struct CatalogRow {
    host_id: String,
    thread_id: String,
    provider: String,
    missing_candidate: bool,
    source_kind: String,
    source_detail: String,
}

#[derive(Debug, Clone)]
struct Snapshot {
    threads: Vec<ThreadRow>,
    catalog: Vec<CatalogRow>,
    rollouts: HashSet<String>,
    valid_active_rollouts: HashSet<String>,
    valid_archived_rollouts: HashSet<String>,
    rollout_providers: HashMap<String, HashSet<String>>,
    session_index: HashSet<String>,
    jsonl_files: Vec<PathBuf>,
    sqlite_readable: usize,
    threads_readable: bool,
    catalog_readable: bool,
    sources: Vec<SourceSummary>,
}

#[derive(Debug, Clone)]
struct CatalogUpdate {
    host_id: String,
    thread_id: String,
}

#[derive(Debug, Clone)]
struct CatalogInsert {
    host_id: String,
    thread_id: String,
    title: String,
    created_at: f64,
    updated_at: f64,
    cwd: String,
    source_detail: String,
    source_kind: String,
    provider: String,
    git_branch: Option<String>,
}

#[derive(Debug, Clone)]
struct RepairPlan {
    state_updates: Vec<String>,
    catalog_updates: Vec<CatalogUpdate>,
    catalog_inserts: Vec<CatalogInsert>,
    changed_ids: HashSet<String>,
    skipped: Vec<SkipReason>,
}

#[derive(Debug, Default)]
struct SessionCohorts {
    local_catalog_ids: HashSet<String>,
    remote_catalog_ids: HashSet<String>,
    remote_session_ids: HashSet<String>,
    remote_excluded_thread_ids: HashSet<String>,
    ordinary_active_ids: HashSet<String>,
    recoverable_ids: HashSet<String>,
    recoverable_indexed_ids: HashSet<String>,
    session_index_covered_ids: HashSet<String>,
    missing_rollout_ids: HashSet<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct LockFile {
    pid: u32,
    created_at: String,
    command: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    path: String,
    size: u64,
    modified: Option<String>,
    sha256: Option<String>,
    backed_up: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    version: u32,
    created_at: String,
    source: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    sqlite_user_versions: BTreeMap<String, i64>,
    files: Vec<ManifestFile>,
}

fn normalize_provider(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn provider_name(id: &str) -> &'static str {
    match id {
        "openai" => "OpenAI",
        "codexpilot" => "CodexPilot",
        "custom" => "Custom",
        _ => "Unknown",
    }
}

fn provider_color(id: &str) -> &'static str {
    match id {
        "openai" => "#4779a7",
        "codexpilot" => "#b17842",
        _ => "#2d7b6f",
    }
}

pub fn default_codex_home() -> PathBuf {
    if let Ok(value) = std::env::var("CODEX_HOME") {
        return PathBuf::from(value);
    }
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    #[cfg(not(windows))]
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".codex")
}

pub fn validate_provider(provider: &str) -> Result<String, String> {
    let normalized = normalize_provider(provider);
    if ALLOWED_PROVIDERS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(format!("unsupported provider: {provider}"))
    }
}

fn current_provider(home: &Path) -> String {
    fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|content| content.parse::<toml::Value>().ok())
        .and_then(|value| {
            value
                .get("model_provider")
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .map(|value| normalize_provider(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?;
    let mut columns = HashSet::new();
    for row in rows {
        columns.insert(row.map_err(|error| error.to_string())?);
    }
    Ok(columns)
}

fn select_expr(columns: &HashSet<String>, name: &str, fallback: &str) -> String {
    if columns.contains(name) {
        format!("\"{name}\"")
    } else {
        fallback.to_string()
    }
}

fn select_time_expr(columns: &HashSet<String>, millis: &str, seconds: &str) -> String {
    match (columns.contains(millis), columns.contains(seconds)) {
        (true, true) => format!("COALESCE(\"{millis}\", \"{seconds}\", 0)"),
        (true, false) => format!("COALESCE(\"{millis}\", 0)"),
        (false, true) => format!("COALESCE(\"{seconds}\", 0)"),
        (false, false) => "0".into(),
    }
}

static SNAPSHOT_COUNTER: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_CLEANUP: Once = Once::new();

#[derive(Clone, PartialEq, Eq)]
struct FileFingerprint {
    length: u64,
    modified: SystemTime,
}

struct SnapshotConnection {
    connection: Option<Connection>,
    directory: PathBuf,
}

impl std::ops::Deref for SnapshotConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("snapshot connection already closed")
    }
}

impl Drop for SnapshotConnection {
    fn drop(&mut self) {
        self.connection.take();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn file_fingerprint(path: &Path) -> Result<Option<FileFingerprint>, String> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(FileFingerprint {
            length: metadata.len(),
            modified: metadata.modified().map_err(|error| error.to_string())?,
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn open_readonly(path: &Path) -> Result<SnapshotConnection, String> {
    let snapshot_root = std::env::temp_dir().join("codex-provider-hub-readonly");
    fs::create_dir_all(&snapshot_root).map_err(|error| error.to_string())?;
    SNAPSHOT_CLEANUP.call_once(|| {
        let Ok(entries) = fs::read_dir(&snapshot_root) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if stale {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    });
    let mut last_error = "SQLite changed while creating a read-only snapshot".to_string();
    for _ in 0..4 {
        let nonce = SNAPSHOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = snapshot_root.join(format!(
            "{}-{}-{}",
            std::process::id(),
            Local::now().timestamp_millis(),
            nonce
        ));
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let destination = directory.join("snapshot.sqlite");
        let sources = [
            (path.to_path_buf(), destination.clone()),
            (
                sidecar_path(path, "-wal"),
                sidecar_path(&destination, "-wal"),
            ),
            (
                sidecar_path(path, "-journal"),
                sidecar_path(&destination, "-journal"),
            ),
        ];
        let copy_result = (|| {
            let before = sources
                .iter()
                .map(|(source, _)| file_fingerprint(source))
                .collect::<Result<Vec<_>, _>>()?;
            if before.first().is_none_or(Option::is_none) {
                return Err(format!("SQLite file not found: {}", path.display()));
            }
            for ((source, target), fingerprint) in sources.iter().zip(&before) {
                if fingerprint.is_some() {
                    fs::copy(source, target)
                        .map_err(|error| format!("{}: {error}", source.display()))?;
                }
            }
            let after = sources
                .iter()
                .map(|(source, _)| file_fingerprint(source))
                .collect::<Result<Vec<_>, _>>()?;
            if before != after {
                return Err(format!(
                    "SQLite changed while snapshotting: {}",
                    path.display()
                ));
            }
            Connection::open(&destination).map_err(|error| error.to_string())
        })();
        match copy_result {
            Ok(connection) => {
                return Ok(SnapshotConnection {
                    connection: Some(connection),
                    directory,
                });
            }
            Err(error) => {
                last_error = error;
                let _ = fs::remove_dir_all(&directory);
            }
        }
    }
    Err(last_error)
}

fn sqlite_quick_check(connection: &Connection) -> Result<(), String> {
    let result = connection
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("SQLite quick_check failed: {result}"))
    }
}

fn sqlite_user_version(path: &Path) -> Option<i64> {
    let connection = open_readonly(path).ok()?;
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .ok()
}

fn validate_repair_schema(home: &Path) -> Result<(), String> {
    ensure_home_sqlite_paths(home)?;
    validate_repair_schema_files(
        &home.join("state_5.sqlite"),
        &home.join("sqlite/codex-dev.db"),
    )
}

fn ensure_home_sqlite_paths(home: &Path) -> Result<(), String> {
    let canonical_home = fs::canonicalize(home)
        .map_err(|error| format!("CODEX_HOME is unavailable ({}): {error}", home.display()))?;
    for relative in ["state_5.sqlite", "sqlite/codex-dev.db"] {
        let path = home.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "required SQLite is unavailable ({}): {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!("SQLite path is a symlink: {}", path.display()));
        }
        if !metadata.is_file() {
            return Err(format!(
                "SQLite path is not a regular file: {}",
                path.display()
            ));
        }
        let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        if !canonical.starts_with(&canonical_home) {
            return Err(format!(
                "SQLite path escapes CODEX_HOME: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn validate_repair_schema_files(state_path: &Path, catalog_path: &Path) -> Result<(), String> {
    let state = open_readonly(state_path)?;
    sqlite_quick_check(&state)?;
    let state_columns = table_columns(&state, "threads")?;
    for required in ["id", "model_provider", "archived", "source"] {
        if !state_columns.contains(required) {
            return Err(format!("unsupported threads schema: missing {required}"));
        }
    }

    let catalog = open_readonly(catalog_path)?;
    sqlite_quick_check(&catalog)?;
    let catalog_columns = table_columns(&catalog, "local_thread_catalog")?;
    for required in [
        "host_id",
        "thread_id",
        "display_title",
        "source_created_at",
        "source_updated_at",
        "cwd",
        "source_kind",
        "source_detail",
        "model_provider",
        "git_branch",
        "observation_sequence",
        "missing_candidate",
    ] {
        if !catalog_columns.contains(required) {
            return Err(format!(
                "unsupported local_thread_catalog schema: missing {required}"
            ));
        }
    }
    for (table, required) in [
        (
            "local_thread_catalog_sync_state",
            vec!["host_id", "observation_sequence"],
        ),
        (
            "local_thread_catalog_metadata",
            vec!["id", "catalog_revision"],
        ),
    ] {
        let columns = table_columns(&catalog, table)?;
        for column in required {
            if !columns.contains(column) {
                return Err(format!(
                    "unsupported catalog schema: {table}.{column} missing"
                ));
            }
        }
    }
    Ok(())
}

fn read_threads(home: &Path) -> (Vec<ThreadRow>, SourceSummary) {
    let path = home.join("state_5.sqlite");
    let base = SourceSummary {
        name: "threads".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (Vec::new(), SourceSummary { note, ..base });
    }
    let connection = match open_readonly(&path) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                Vec::new(),
                SourceSummary {
                    note: error,
                    ..base
                },
            )
        }
    };
    if let Err(error) = sqlite_quick_check(&connection) {
        return (
            Vec::new(),
            SourceSummary {
                note: error,
                ..base
            },
        );
    }
    let Ok(columns) = table_columns(&connection, "threads") else {
        return (
            Vec::new(),
            SourceSummary {
                note: "table schema unreadable".into(),
                ..base
            },
        );
    };
    if !columns.contains("id") || !columns.contains("model_provider") {
        return (
            Vec::new(),
            SourceSummary {
                note: "required columns missing".into(),
                ..base
            },
        );
    }
    let sql = format!(
        "SELECT id, model_provider, {}, {}, {}, {}, {}, {}, {}, {} FROM threads",
        select_expr(&columns, "archived", "0"),
        select_expr(&columns, "source", "''"),
        select_expr(&columns, "thread_source", "''"),
        select_expr(&columns, "agent_role", "NULL"),
        select_expr(&columns, "title", "''"),
        select_expr(&columns, "cwd", "''"),
        select_time_expr(&columns, "created_at_ms", "created_at"),
        select_time_expr(&columns, "updated_at_ms", "updated_at"),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return (
            Vec::new(),
            SourceSummary {
                note: "query failed".into(),
                ..base
            },
        );
    };
    let rows = statement.query_map([], |row| {
        Ok(ThreadRow {
            id: row.get(0)?,
            provider: row.get(1)?,
            archived: row.get::<_, Option<i64>>(2)?.unwrap_or_default() != 0,
            source: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            thread_source: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            agent_role: row.get::<_, Option<String>>(5)?,
            title: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            cwd: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            created_at: row.get::<_, Option<f64>>(8)?.unwrap_or_default(),
            updated_at: row.get::<_, Option<f64>>(9)?.unwrap_or_default(),
        })
    });
    let Ok(rows) = rows else {
        return (
            Vec::new(),
            SourceSummary {
                note: "row read failed".into(),
                ..base
            },
        );
    };
    let mut threads = Vec::new();
    for row in rows {
        match row {
            Ok(value) => threads.push(value),
            Err(error) => {
                return (
                    Vec::new(),
                    SourceSummary {
                        note: format!("row read failed: {error}"),
                        ..base
                    },
                );
            }
        }
    }
    (
        threads.clone(),
        SourceSummary {
            records: threads.len(),
            readable: true,
            note: "read-only".into(),
            ..base
        },
    )
}

fn read_catalog(home: &Path) -> (Vec<CatalogRow>, SourceSummary) {
    let path = home.join("sqlite/codex-dev.db");
    let base = SourceSummary {
        name: "local_thread_catalog".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (Vec::new(), SourceSummary { note, ..base });
    }
    let connection = match open_readonly(&path) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                Vec::new(),
                SourceSummary {
                    note: error,
                    ..base
                },
            )
        }
    };
    if let Err(error) = sqlite_quick_check(&connection) {
        return (
            Vec::new(),
            SourceSummary {
                note: error,
                ..base
            },
        );
    }
    let Ok(columns) = table_columns(&connection, "local_thread_catalog") else {
        return (
            Vec::new(),
            SourceSummary {
                note: "table schema unreadable".into(),
                ..base
            },
        );
    };
    let required = ["host_id", "thread_id", "model_provider"];
    if required.iter().any(|column| !columns.contains(*column)) {
        return (
            Vec::new(),
            SourceSummary {
                note: "required columns missing".into(),
                ..base
            },
        );
    }
    let sql = format!(
        "SELECT host_id, thread_id, model_provider, {}, {}, {} FROM local_thread_catalog",
        select_expr(&columns, "missing_candidate", "0"),
        select_expr(&columns, "source_kind", "''"),
        select_expr(&columns, "source_detail", "''"),
    );
    let Ok(mut statement) = connection.prepare(&sql) else {
        return (
            Vec::new(),
            SourceSummary {
                note: "query failed".into(),
                ..base
            },
        );
    };
    let rows = statement.query_map([], |row| {
        Ok(CatalogRow {
            host_id: row.get(0)?,
            thread_id: row.get(1)?,
            provider: row.get(2)?,
            missing_candidate: row.get::<_, Option<i64>>(3)?.unwrap_or_default() != 0,
            source_kind: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            source_detail: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        })
    });
    let Ok(rows) = rows else {
        return (
            Vec::new(),
            SourceSummary {
                note: "row read failed".into(),
                ..base
            },
        );
    };
    let mut catalog = Vec::new();
    for row in rows {
        match row {
            Ok(value) => catalog.push(value),
            Err(error) => {
                return (
                    Vec::new(),
                    SourceSummary {
                        note: format!("row read failed: {error}"),
                        ..base
                    },
                );
            }
        }
    }
    let local_rows = catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .count();
    let remote_rows = catalog
        .iter()
        .filter(|row| catalog_row_is_remote(row))
        .count();
    (
        catalog.clone(),
        SourceSummary {
            records: catalog.len(),
            readable: true,
            note: format!("read-only; {local_rows} local rows; {remote_rows} non-local rows"),
            ..base
        },
    )
}

fn extract_rollout(value: &Value) -> Option<(String, Option<String>)> {
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let id = payload.get("id")?.as_str()?.to_owned();
    let provider = payload
        .get("model_provider")
        .or_else(|| payload.get("modelProvider"))
        .and_then(Value::as_str)
        .map(normalize_provider)
        .filter(|value| !value.is_empty());
    Some((id, provider))
}

struct RolloutRead {
    ids: HashSet<String>,
    valid_active_ids: HashSet<String>,
    valid_archived_ids: HashSet<String>,
    providers: HashMap<String, HashSet<String>>,
    files: Vec<PathBuf>,
    malformed_lines: usize,
    files_without_metadata: usize,
    multi_metadata_files: usize,
    duplicate_ids: usize,
    walk_errors: usize,
}

fn read_rollouts(home: &Path) -> RolloutRead {
    let mut rollouts = HashSet::new();
    let mut rollout_providers: HashMap<String, HashSet<String>> = HashMap::new();
    let mut files = Vec::new();
    let mut malformed_lines = 0;
    let mut files_without_metadata = 0;
    let mut multi_metadata_files = 0;
    let mut walk_errors = 0;
    let mut valid_file_counts: HashMap<String, usize> = HashMap::new();
    let mut active_file_counts: HashMap<String, usize> = HashMap::new();
    let mut archived_file_counts: HashMap<String, usize> = HashMap::new();
    for root_name in ["sessions", "archived_sessions"] {
        let root = home.join(root_name);
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    walk_errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            files.push(path.to_path_buf());
            let Ok(file) = File::open(path) else {
                files_without_metadata += 1;
                continue;
            };
            let mut file_ids = HashSet::new();
            let mut metadata_read_failed = false;
            for line in BufReader::new(file).lines() {
                let Ok(line) = line else {
                    malformed_lines += 1;
                    metadata_read_failed = true;
                    continue;
                };
                if !line.contains("session_meta") {
                    continue;
                }
                let line = line.trim_start_matches('\u{feff}');
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    malformed_lines += 1;
                    metadata_read_failed = true;
                    continue;
                };
                let Some((id, provider)) = extract_rollout(&value) else {
                    continue;
                };
                file_ids.insert(id.clone());
                if let Some(provider) = provider {
                    rollout_providers
                        .entry(id.clone())
                        .or_default()
                        .insert(provider);
                }
                rollouts.insert(id);
            }
            if file_ids.is_empty() {
                files_without_metadata += 1;
            }
            if file_ids.len() > 1 {
                multi_metadata_files += 1;
            }
            if !metadata_read_failed && file_ids.len() == 1 {
                let id = file_ids.into_iter().next().expect("single rollout id");
                *valid_file_counts.entry(id.clone()).or_default() += 1;
                let root_counts = if root_name == "archived_sessions" {
                    &mut archived_file_counts
                } else {
                    &mut active_file_counts
                };
                *root_counts.entry(id).or_default() += 1;
            }
        }
    }
    let valid_active_ids = active_file_counts
        .into_iter()
        .filter(|(id, count)| *count == 1 && valid_file_counts.get(id) == Some(&1))
        .map(|(id, _)| id)
        .collect();
    let valid_archived_ids = archived_file_counts
        .into_iter()
        .filter(|(id, count)| *count == 1 && valid_file_counts.get(id) == Some(&1))
        .map(|(id, _)| id)
        .collect();
    let duplicate_ids = valid_file_counts
        .values()
        .filter(|count| **count > 1)
        .count();
    RolloutRead {
        ids: rollouts,
        valid_active_ids,
        valid_archived_ids,
        providers: rollout_providers,
        files,
        malformed_lines,
        files_without_metadata,
        multi_metadata_files,
        duplicate_ids,
        walk_errors,
    }
}

fn read_session_index(home: &Path) -> (HashSet<String>, SourceSummary) {
    let path = home.join("session_index.jsonl");
    let base = SourceSummary {
        name: "session_index".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    if let Err(note) = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("not a regular file".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("file not found".to_string())
        }
        Err(error) => Err(error.to_string()),
    } {
        return (HashSet::new(), SourceSummary { note, ..base });
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            return (
                HashSet::new(),
                SourceSummary {
                    note: error.to_string(),
                    ..base
                },
            )
        }
    };
    let mut ids = HashSet::new();
    let mut malformed = 0;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                return (
                    HashSet::new(),
                    SourceSummary {
                        note: format!("line read failed: {error}"),
                        ..base
                    },
                )
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}'))
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned));
        if let Some(id) = id {
            ids.insert(id);
        } else {
            malformed += 1;
        }
    }
    (
        ids.clone(),
        SourceSummary {
            records: ids.len(),
            readable: true,
            note: format!("index read-only; {malformed} malformed lines"),
            ..base
        },
    )
}

fn source_metadata(thread: &ThreadRow) -> Option<(String, String)> {
    let raw = thread.source.trim();
    let parsed = serde_json::from_str::<Value>(raw).ok();
    let (kind, detail) = match parsed {
        Some(Value::String(value)) => (value, String::new()),
        Some(Value::Object(object)) => {
            if let Some(custom) = object.get("custom").and_then(Value::as_str) {
                ("custom".into(), custom.into())
            } else if let Some(kind) = object
                .get("kind")
                .or_else(|| object.get("type"))
                .and_then(Value::as_str)
            {
                (
                    kind.into(),
                    object
                        .get("detail")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                )
            } else {
                return None;
            }
        }
        _ if !raw.is_empty() => (raw.into(), String::new()),
        _ => (thread.thread_source.clone(), String::new()),
    };
    let normalized = kind.trim().to_ascii_lowercase();
    let canonical = match normalized.as_str() {
        "cli" => "cli",
        "vscode" => "vscode",
        "appserver" | "app_server" => "appServer",
        "custom" => "custom",
        "user" => "user",
        _ => return None,
    };
    Some((canonical.into(), detail))
}

fn catalog_time(value: f64) -> f64 {
    if value.abs() > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    }
}

fn catalog_title(thread: &ThreadRow) -> String {
    let raw = if !thread.title.trim().is_empty() {
        thread.title.as_str()
    } else if !thread.cwd.trim().is_empty() {
        thread.cwd.as_str()
    } else {
        thread.id.as_str()
    };
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 80 {
        compact
    } else {
        format!("{}...", compact.chars().take(77).collect::<String>())
    }
}

fn scan_snapshot(home: &Path) -> Snapshot {
    let (threads, thread_source) = read_threads(home);
    let (catalog, catalog_source) = read_catalog(home);
    let rollout_read = read_rollouts(home);
    let (session_index, session_index_source) = read_session_index(home);
    let sqlite_readable =
        usize::from(thread_source.readable) + usize::from(catalog_source.readable);
    let rollout_count = rollout_read.ids.len();
    let valid_rollout_count =
        rollout_read.valid_active_ids.len() + rollout_read.valid_archived_ids.len();
    let rollout_provider_count = rollout_read.providers.len();
    let jsonl_count = rollout_read.files.len();
    Snapshot {
        threads,
        catalog,
        rollouts: rollout_read.ids,
        valid_active_rollouts: rollout_read.valid_active_ids,
        valid_archived_rollouts: rollout_read.valid_archived_ids,
        rollout_providers: rollout_read.providers,
        session_index,
        jsonl_files: rollout_read.files,
        sqlite_readable,
        threads_readable: thread_source.readable,
        catalog_readable: catalog_source.readable,
        sources: vec![
            thread_source,
            catalog_source,
            SourceSummary {
                name: "rollouts".into(),
                path: home.join("sessions").to_string_lossy().to_string(),
                records: rollout_count,
                readable: (home.join("sessions").is_dir()
                    || home.join("archived_sessions").is_dir())
                    && rollout_read.walk_errors == 0,
                note: format!(
                    "{jsonl_count} files; {valid_rollout_count} unique valid rollouts; {} malformed metadata lines; {} without metadata; {} with multiple IDs; {} duplicate IDs; provider metadata for {rollout_provider_count} IDs; {} traversal errors; read-only",
                    rollout_read.malformed_lines,
                    rollout_read.files_without_metadata,
                    rollout_read.multi_metadata_files,
                    rollout_read.duplicate_ids,
                    rollout_read.walk_errors,
                ),
            },
            session_index_source,
        ],
    }
}

fn global_state_source(home: &Path) -> SourceSummary {
    let path = home.join(".codex-global-state.json");
    let base = SourceSummary {
        name: "global_state".into(),
        path: path.to_string_lossy().to_string(),
        records: 0,
        readable: false,
        note: String::new(),
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SourceSummary {
                note: error.to_string(),
                ..base
            }
        }
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return SourceSummary {
            note: "invalid JSON; read-only check failed".into(),
            ..base
        };
    };
    let records = value.as_object().map_or(1, |object| object.len());
    SourceSummary {
        records,
        readable: true,
        note: "read-only UI/thread metadata; provider fields are ignored".into(),
        ..base
    }
}

fn is_skipped_thread(thread: &ThreadRow) -> Option<String> {
    let source = format!("{} {}", thread.source, thread.thread_source).to_ascii_lowercase();
    if thread
        .agent_role
        .as_deref()
        .is_some_and(|role| !role.trim().is_empty())
        || source.contains("subagent")
        || source.contains("automation")
        || source.contains("automated")
        || source.contains("thread_spawn")
        || source.contains("guardian")
        || source.contains("parentthreadid")
        || source.contains("parent_thread")
        || source.contains("pull_request_fix_automation")
        || source.contains("\"ephemeral\":true")
    {
        return Some("subagent_or_automation".into());
    }
    if !ALLOWED_PROVIDERS.contains(&normalize_provider(&thread.provider).as_str()) {
        return Some("untrusted_provider".into());
    }
    // Only the ordinary local clients are repair candidates. Unknown source
    // records are left untouched so a future Codex source cannot be promoted
    // into the local catalog accidentally.
    if source_metadata(thread).is_none() {
        return Some("unknown_source".into());
    }
    None
}

fn has_explicit_remote_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase().replace(' ', "");
    [
        "\"remote\":true",
        "remoteauthority",
        "ssh-remote",
        "dev-container",
        "devcontainer",
        "codespaces",
        "\"wsl\"",
        "wsl+",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn thread_is_explicit_remote(thread: &ThreadRow) -> bool {
    has_explicit_remote_marker(&thread.source) || has_explicit_remote_marker(&thread.thread_source)
}

fn catalog_row_is_remote(row: &CatalogRow) -> bool {
    let kind = row.source_kind.trim().to_ascii_lowercase();
    !row.host_id.eq_ignore_ascii_case("local")
        || matches!(
            kind.as_str(),
            "remote" | "ssh" | "ssh-remote" | "wsl" | "devcontainer" | "dev-container"
        )
        || has_explicit_remote_marker(&row.source_detail)
}

fn catalog_row_is_local(row: &CatalogRow) -> bool {
    row.host_id.eq_ignore_ascii_case("local") && !catalog_row_is_remote(row)
}

fn session_cohorts(snapshot: &Snapshot) -> SessionCohorts {
    let local_mapping_ids: HashSet<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .map(|row| row.thread_id.clone())
        .collect();
    let local_catalog_ids: HashSet<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row) && !row.missing_candidate)
        .map(|row| row.thread_id.clone())
        .collect();
    let remote_catalog_ids: HashSet<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_remote(row))
        .map(|row| row.thread_id.clone())
        .collect();
    let explicit_remote_ids: HashSet<_> = snapshot
        .threads
        .iter()
        .filter(|thread| thread_is_explicit_remote(thread))
        .map(|thread| thread.id.clone())
        .collect();
    let mut remote_session_ids = remote_catalog_ids.clone();
    remote_session_ids.extend(explicit_remote_ids.iter().cloned());
    let remote_excluded_thread_ids = snapshot
        .threads
        .iter()
        .filter(|thread| {
            explicit_remote_ids.contains(&thread.id)
                || (remote_catalog_ids.contains(&thread.id)
                    && !local_mapping_ids.contains(&thread.id))
        })
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let ordinary_active_ids = snapshot
        .threads
        .iter()
        .filter(|thread| !thread.archived && is_skipped_thread(thread).is_none())
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let missing_rollout_ids = ordinary_active_ids
        .iter()
        .filter(|id| {
            !remote_excluded_thread_ids.contains(*id)
                && !snapshot.valid_active_rollouts.contains(*id)
        })
        .cloned()
        .collect::<HashSet<_>>();
    let recoverable_ids = ordinary_active_ids
        .iter()
        .filter(|id| {
            !remote_excluded_thread_ids.contains(*id)
                && snapshot.valid_active_rollouts.contains(*id)
        })
        .cloned()
        .collect::<HashSet<_>>();
    let recoverable_indexed_ids = recoverable_ids
        .intersection(&local_catalog_ids)
        .cloned()
        .collect();
    let session_index_covered_ids = recoverable_ids
        .intersection(&snapshot.session_index)
        .cloned()
        .collect();
    SessionCohorts {
        local_catalog_ids,
        remote_catalog_ids,
        remote_session_ids,
        remote_excluded_thread_ids,
        ordinary_active_ids,
        recoverable_ids,
        recoverable_indexed_ids,
        session_index_covered_ids,
        missing_rollout_ids,
    }
}

fn repair_exclusion_reason(
    snapshot: &Snapshot,
    cohorts: &SessionCohorts,
    thread: &ThreadRow,
) -> Option<String> {
    if let Some(reason) = is_skipped_thread(thread) {
        return Some(reason);
    }
    if cohorts.remote_excluded_thread_ids.contains(&thread.id) {
        return Some("remote_mapped".into());
    }
    if !snapshot.valid_active_rollouts.contains(&thread.id) {
        return Some("rollout_missing_or_ambiguous".into());
    }
    None
}

fn active_processes() -> Vec<String> {
    #[cfg(windows)]
    let output = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output();
    #[cfg(not(windows))]
    let output = Command::new("ps").args(["-eo", "pid=,comm="]).output();
    let Ok(output) = output else {
        return vec!["process-enumeration-failed".into()];
    };
    if !output.status.success() {
        return vec!["process-enumeration-failed".into()];
    }
    let text = String::from_utf8_lossy(&output.stdout);
    #[cfg(windows)]
    let rows = text
        .lines()
        .filter_map(|line| {
            let fields = line.split(',').collect::<Vec<_>>();
            let name = fields.first()?.trim().trim_matches('"').to_string();
            let pid = fields
                .get(1)
                .and_then(|value| value.trim().trim_matches('"').parse::<u32>().ok());
            Some((pid, name))
        })
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let rows = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok();
            let name = fields.collect::<Vec<_>>().join(" ");
            Some((pid, name))
        })
        .collect::<Vec<_>>();
    let self_pid = std::process::id();
    rows.into_iter()
        .filter_map(|(pid, name)| {
            let lower = name.to_ascii_lowercase();
            if pid == Some(self_pid)
                || lower.contains("codex-provider-hub")
                || lower.contains("codex_provider_hub")
                || lower.contains("codex-session-repair")
            {
                return None;
            }
            (lower.contains("codex") || lower.contains("launcher")).then_some(name)
        })
        .collect()
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .args(["/FO", "CSV"])
            .output();
        let Ok(output) = output else {
            return true;
        };
        if !output.status.success() {
            return true;
        }
        String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.split(',')
                .nth(1)
                .and_then(|value| value.trim().trim_matches('"').parse::<u32>().ok())
                == Some(pid)
        })
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

fn lock_path(home: &Path) -> PathBuf {
    home.join(".provider-hub.lock")
}

pub fn inspect_lock(home: &Path) -> LockSummary {
    let path = lock_path(home);
    let active = active_processes();
    if !path.is_file() {
        return LockSummary {
            state: if active.is_empty() {
                "clear"
            } else {
                "process-active"
            }
            .into(),
            path: path.to_string_lossy().to_string(),
            owner_pid: None,
            age_seconds: None,
            active_processes: active,
        };
    }
    let age_seconds = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|duration| duration.as_secs());
    let lock: Option<LockFile> = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let owner_pid = lock.as_ref().map(|value| value.pid);
    let owner_alive = owner_pid.is_some_and(pid_alive);
    LockSummary {
        state: if owner_alive || age_seconds.unwrap_or(0) < 1800 {
            "active"
        } else {
            "stale"
        }
        .into(),
        path: path.to_string_lossy().to_string(),
        owner_pid,
        age_seconds,
        active_processes: active,
    }
}

fn hash_file(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 64];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Some(format!("{:x}", digest.finalize()))
}

fn iso_modified(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let timestamp: DateTime<Utc> = modified.into();
    Some(timestamp.to_rfc3339())
}

fn ensure_backup_root(home: &Path, create: bool) -> Result<PathBuf, String> {
    let canonical_home = fs::canonicalize(home)
        .map_err(|error| format!("CODEX_HOME is unavailable ({}): {error}", home.display()))?;
    let mut current = home.to_path_buf();
    for component in ["backups", "provider-hub"] {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "backup directory is a symlink: {}",
                    current.display()
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "backup path is not a directory: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                fs::create_dir(&current).map_err(|error| {
                    format!(
                        "cannot create backup directory ({}): {error}",
                        current.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "backup directory unavailable ({}): {error}",
                    current.display()
                ));
            }
        }
        let canonical = fs::canonicalize(&current).map_err(|error| error.to_string())?;
        if !canonical.starts_with(&canonical_home) {
            return Err(format!(
                "backup directory escapes CODEX_HOME: {}",
                current.display()
            ));
        }
    }
    Ok(current)
}

fn relevant_manifest_paths(home: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for relative in [
        "config.toml",
        "auth.json",
        ".codex-global-state.json",
        "state_5.sqlite",
        "sqlite/codex-dev.db",
    ] {
        let path = home.join(relative);
        if path.is_file() {
            paths.push(path);
        }
    }
    for root in [home.join("sessions"), home.join("archived_sessions")] {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry.map_err(|error| format!("manifest traversal failed: {error}"))?;
            if entry.file_type().is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl")
            {
                paths.push(entry.into_path());
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn sqlite_online_copy(source: &Path, destination: &Path) -> Result<(), String> {
    let source_connection = open_readonly(source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut destination_connection = Connection::open(destination)
        .map_err(|error| format!("{}: {error}", destination.display()))?;
    {
        let backup = Backup::new(&source_connection, &mut destination_connection)
            .map_err(|error| error.to_string())?;
        backup
            .run_to_completion(64, Duration::from_millis(20), None)
            .map_err(|error| error.to_string())?;
    }
    destination_connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| {
            format!(
                "SQLite checkpoint failed ({}): {error}",
                destination.display()
            )
        })?;
    Ok(())
}

fn probe_sqlite_write_lock(path: &Path) -> Result<(), String> {
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_millis(500))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
        .map_err(|error| {
            format!(
                "database is locked or not writable ({}): {error}",
                path.display()
            )
        })
}

pub fn create_backup_at(home: &Path) -> Result<BackupResult, String> {
    validate_repair_schema(home)?;
    let stamp = Local::now().format("repair-%Y%m%d-%H%M%S-%3f").to_string();
    let destination = ensure_backup_root(home, true)?.join(stamp);
    fs::create_dir(&destination).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut files = Vec::new();
        for relative in ["state_5.sqlite", "sqlite/codex-dev.db"] {
            let source = home.join(relative);
            if !source.is_file() {
                return Err(format!(
                    "complete backup unavailable: {} is missing",
                    source.display()
                ));
            }
            let connection = open_readonly(&source)?;
            sqlite_quick_check(&connection)
                .map_err(|error| format!("{}: {error}", source.display()))?;
            let target = destination.join(relative.replace('/', "_"));
            sqlite_online_copy(&source, &target)?;
            files.push(relative.to_string());
        }
        let manifest_files = relevant_manifest_paths(home)?
            .into_iter()
            .map(|path| -> Result<ManifestFile, String> {
                let relative = path
                    .strip_prefix(home)
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                let backed_up = files.iter().any(|item| item == &relative);
                let recorded_path = if backed_up {
                    destination.join(relative.replace('/', "_"))
                } else {
                    path.clone()
                };
                let size = fs::metadata(&recorded_path)
                    .map_err(|error| format!("{}: {error}", recorded_path.display()))?
                    .len();
                let sha256 = if backed_up {
                    Some(hash_file(&recorded_path).ok_or_else(|| {
                        format!("cannot hash backup file: {}", recorded_path.display())
                    })?)
                } else {
                    None
                };
                Ok(ManifestFile {
                    path: relative,
                    size,
                    modified: iso_modified(&recorded_path),
                    sha256,
                    backed_up,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sqlite_user_versions = ["state_5.sqlite", "sqlite/codex-dev.db"]
            .into_iter()
            .filter_map(|relative| {
                sqlite_user_version(&home.join(relative)).map(|version| (relative.into(), version))
            })
            .collect();
        let manifest = BackupManifest {
            version: 2,
            created_at: Local::now().to_rfc3339(),
            source: home.to_string_lossy().to_string(),
            provider: current_provider(home),
            sqlite_user_versions,
            files: manifest_files,
        };
        let manifest_path = destination.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        Ok(BackupResult {
            path: destination.to_string_lossy().to_string(),
            files,
            manifest: manifest_path.to_string_lossy().to_string(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&destination);
    }
    result
}

pub fn create_backup_safe_at(home: &Path) -> Result<BackupResult, String> {
    let lock = inspect_lock(home);
    if !lock.active_processes.is_empty() {
        return Err(format!(
            "backup blocked by active Codex processes: {}",
            lock.active_processes.join(", ")
        ));
    }
    if Path::new(&lock.path).is_file() {
        if lock.state == "stale" {
            recover_stale_lock(home, &lock)?;
        } else {
            return Err(format!("backup blocked by lock state: {}", lock.state));
        }
    }
    let _guard = create_lock_with_command(home, "backup")?;
    let result = create_backup_at(home);
    remove_lock(home);
    result
}

fn latest_backup(home: &Path) -> Option<PathBuf> {
    let root = ensure_backup_root(home, false).ok()?;
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| is_restorable_backup(&entry.path()))
        .max_by_key(|entry| entry.file_name())
        .map(|entry| entry.path())
}

fn is_restorable_backup(path: &Path) -> bool {
    let manifest = fs::read(path.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BackupManifest>(&bytes).ok());
    let Some(manifest) = manifest else {
        return false;
    };
    if manifest.version != 2 {
        return false;
    }
    let paths: HashSet<_> = manifest
        .files
        .iter()
        .filter(|file| file.backed_up)
        .map(|file| file.path.as_str())
        .collect();
    manifest.files.iter().filter(|file| file.backed_up).count() == 2
        && paths == HashSet::from(["state_5.sqlite", "sqlite/codex-dev.db"])
        && manifest
            .files
            .iter()
            .filter(|file| file.backed_up)
            .all(|file| {
                let backup_file = path.join(file.path.replace('/', "_"));
                file.sha256.is_some()
                    && fs::symlink_metadata(&backup_file)
                        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                        .unwrap_or(false)
            })
}

fn safe_backup_path(home: &Path, requested: Option<&Path>) -> Result<PathBuf, String> {
    let path = match requested {
        Some(value) if value.is_absolute() => value.to_path_buf(),
        Some(value) => home.join(value),
        None => latest_backup(home).ok_or("no backup found")?,
    };
    let root =
        fs::canonicalize(ensure_backup_root(home, false)?).map_err(|error| error.to_string())?;
    let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
    if !canonical.starts_with(&root) {
        return Err("backup path is outside CODEX_HOME/backups/provider-hub".into());
    }
    Ok(canonical)
}

fn restore_target_is_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("restore target is a symlink: {}", path.display()))
        }
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "restore target is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "restore target unavailable ({}): {error}",
            path.display()
        )),
    }
}

fn restore_backup_unchecked(home: &Path, requested: Option<&Path>) -> Result<(), String> {
    // Rollback is intentionally in-place: both live databases must exist so a
    // failed two-database restore can always put the complete pre-image back.
    ensure_home_sqlite_paths(home)?;
    for path in [
        home.join("state_5.sqlite"),
        home.join("sqlite/codex-dev.db"),
    ] {
        probe_sqlite_write_lock(&path)?;
    }
    let backup = safe_backup_path(home, requested)?;
    let manifest: BackupManifest = serde_json::from_slice(
        &fs::read(backup.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if manifest.version != 2 {
        return Err(format!(
            "unsupported backup manifest version: {}",
            manifest.version
        ));
    }
    let files = manifest
        .files
        .iter()
        .filter(|file| file.backed_up)
        .collect::<Vec<_>>();
    let write_set: HashSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    if files.len() != 2 || write_set != HashSet::from(["state_5.sqlite", "sqlite/codex-dev.db"]) {
        return Err("backup manifest does not contain the complete SQLite write set".into());
    }
    let mut sources = Vec::new();
    for file in &files {
        if !matches!(file.path.as_str(), "state_5.sqlite" | "sqlite/codex-dev.db") {
            return Err(format!(
                "backup manifest contains unsafe path: {}",
                file.path
            ));
        }
        let source = backup.join(file.path.replace('/', "_"));
        if !source.is_file() {
            return Err(format!("backup file missing: {}", source.display()));
        }
        if fs::symlink_metadata(&source)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(true)
        {
            return Err(format!("backup file is a symlink: {}", source.display()));
        }
        let expected_hash = file
            .sha256
            .as_deref()
            .ok_or_else(|| format!("backup checksum missing: {}", source.display()))?;
        let actual_hash = hash_file(&source)
            .ok_or_else(|| format!("cannot hash backup file: {}", source.display()))?;
        if expected_hash != actual_hash {
            return Err(format!("backup checksum mismatch: {}", source.display()));
        }
        let source_connection = open_readonly(&source)?;
        sqlite_quick_check(&source_connection)?;
        sources.push((file.path.clone(), source));
    }
    let state_backup = sources
        .iter()
        .find(|(relative, _)| relative == "state_5.sqlite")
        .map(|(_, path)| path.as_path())
        .ok_or("state_5.sqlite backup missing")?;
    let catalog_backup = sources
        .iter()
        .find(|(relative, _)| relative == "sqlite/codex-dev.db")
        .map(|(_, path)| path.as_path())
        .ok_or("codex-dev.db backup missing")?;
    validate_repair_schema_files(state_backup, catalog_backup)?;
    let temporary = ensure_backup_root(home, false)?.join(format!(
        ".restore-before-{}",
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    fs::create_dir(&temporary).map_err(|error| error.to_string())?;
    let mut previous = Vec::new();
    for (relative, _) in &sources {
        let target = home.join(relative);
        let is_file = match restore_target_is_file(&target) {
            Ok(is_file) => is_file,
            Err(error) => {
                let _ = fs::remove_dir_all(&temporary);
                return Err(error);
            }
        };
        if !is_file {
            let _ = fs::remove_dir_all(&temporary);
            return Err(format!("restore target is missing: {}", target.display()));
        }
        let saved = temporary.join(relative.replace('/', "_"));
        if let Err(error) = sqlite_online_copy(&target, &saved) {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
        previous.push((relative.clone(), saved));
    }
    let apply_result = (|| {
        for (relative, source) in &sources {
            let target = home.join(relative);
            if !restore_target_is_file(&target)? {
                return Err(format!("restore target is missing: {}", target.display()));
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                let canonical_home = fs::canonicalize(home).map_err(|error| error.to_string())?;
                let canonical_parent =
                    fs::canonicalize(parent).map_err(|error| error.to_string())?;
                if !canonical_parent.starts_with(&canonical_home) {
                    return Err(format!(
                        "restore target escapes CODEX_HOME: {}",
                        target.display()
                    ));
                }
            }
            sqlite_online_copy(source, &target)?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = apply_result {
        let recovery = restore_previous_databases(home, &previous);
        let _ = fs::remove_dir_all(&temporary);
        return match recovery {
            Ok(()) => Err(error),
            Err(recovery_error) => Err(format!(
                "restore failed: {error}; recovery failed: {recovery_error}"
            )),
        };
    }
    if let Err(error) = validate_repair_schema(home) {
        let recovery = restore_previous_databases(home, &previous);
        let _ = fs::remove_dir_all(&temporary);
        return match recovery {
            Ok(()) => Err(format!("restored databases failed validation: {error}")),
            Err(recovery_error) => Err(format!(
                "restored databases failed validation: {error}; recovery failed: {recovery_error}"
            )),
        };
    }
    let _ = fs::remove_dir_all(&temporary);
    Ok(())
}

fn restore_previous_databases(home: &Path, previous: &[(String, PathBuf)]) -> Result<(), String> {
    let mut errors = Vec::new();
    for (relative, saved) in previous {
        let target = home.join(relative);
        match restore_target_is_file(&target) {
            Ok(true) => {}
            Ok(false) => {
                errors.push(format!("{relative}: restore target is missing"));
                continue;
            }
            Err(error) => {
                errors.push(format!("{relative}: {error}"));
                continue;
            }
        }
        if let Err(error) = sqlite_online_copy(saved, &target) {
            errors.push(format!("{relative}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn restore_backup_at(home: &Path, requested: Option<&Path>) -> Result<(), String> {
    let lock = inspect_lock(home);
    if !lock.active_processes.is_empty() {
        return Err(format!(
            "restore blocked by active Codex processes: {}",
            lock.active_processes.join(", ")
        ));
    }
    if Path::new(&lock.path).is_file() {
        if lock.state == "stale" {
            recover_stale_lock(home, &lock)?;
        } else {
            return Err(format!("restore blocked by lock state: {}", lock.state));
        }
    }
    let _guard = create_lock_with_command(home, "restore")?;
    let result =
        restore_backup_unchecked(home, requested).and_then(|_| validate_repair_schema(home));
    remove_lock(home);
    result
}

fn recover_stale_lock(home: &Path, lock: &LockSummary) -> Result<(), String> {
    if lock.state != "stale" || !Path::new(&lock.path).is_file() {
        return Ok(());
    }
    let destination = home.join(format!(
        ".provider-hub.lock.stale-{}",
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    fs::rename(&lock.path, destination).map_err(|error| error.to_string())
}

fn sort_reasons(reasons: &mut [SkipReason]) {
    reasons.sort_by(|left, right| {
        left.thread_id
            .cmp(&right.thread_id)
            .then_with(|| left.reason.cmp(&right.reason))
    });
}

fn build_plan(snapshot: &Snapshot, target_provider: &str) -> RepairPlan {
    let target_value = canonical_provider(target_provider);
    let cohorts = session_cohorts(snapshot);
    let mut plan = RepairPlan {
        state_updates: Vec::new(),
        catalog_updates: Vec::new(),
        catalog_inserts: Vec::new(),
        changed_ids: HashSet::new(),
        skipped: Vec::new(),
    };
    let mut catalog_by_id: HashMap<String, Vec<&CatalogRow>> = HashMap::new();
    for row in &snapshot.catalog {
        catalog_by_id
            .entry(row.thread_id.clone())
            .or_default()
            .push(row);
    }
    let thread_ids: HashSet<_> = snapshot
        .threads
        .iter()
        .map(|thread| thread.id.clone())
        .collect();
    let mut discovered_ids = thread_ids.clone();
    discovered_ids.extend(
        snapshot
            .catalog
            .iter()
            .filter(|row| catalog_row_is_local(row))
            .map(|row| row.thread_id.clone()),
    );
    discovered_ids.extend(snapshot.rollouts.iter().cloned());
    discovered_ids.extend(snapshot.session_index.iter().cloned());
    plan.skipped.extend(
        discovered_ids
            .difference(&thread_ids)
            .filter(|id| !cohorts.remote_catalog_ids.contains(*id))
            .map(|id| SkipReason {
                thread_id: Some(id.clone()),
                reason: "orphan_without_thread_row".into(),
            }),
    );
    if !snapshot.catalog_readable {
        for thread in &snapshot.threads {
            if !thread.archived && repair_exclusion_reason(snapshot, &cohorts, thread).is_none() {
                plan.skipped.push(SkipReason {
                    thread_id: Some(thread.id.clone()),
                    reason: "catalog_unreadable".into(),
                });
            }
        }
        sort_reasons(&mut plan.skipped);
        return plan;
    }

    for thread in &snapshot.threads {
        if thread.archived {
            continue;
        }
        if let Some(reason) = repair_exclusion_reason(snapshot, &cohorts, thread) {
            plan.skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason,
            });
            continue;
        }
        let current = thread.provider.as_str();
        let local_row = catalog_by_id
            .get(&thread.id)
            .and_then(|rows| rows.iter().find(|row| catalog_row_is_local(row)));
        if let Some(row) = local_row {
            if !ALLOWED_PROVIDERS.contains(&normalize_provider(&row.provider).as_str()) {
                plan.skipped.push(SkipReason {
                    thread_id: Some(thread.id.clone()),
                    reason: "untrusted_catalog_provider".into(),
                });
                continue;
            }
        }
        if current != target_value {
            plan.state_updates.push(thread.id.clone());
            plan.changed_ids.insert(thread.id.clone());
        }
        match local_row {
            Some(row) => {
                if row.provider != target_value || row.missing_candidate {
                    plan.catalog_updates.push(CatalogUpdate {
                        host_id: "local".into(),
                        thread_id: row.thread_id.clone(),
                    });
                    plan.changed_ids.insert(thread.id.clone());
                }
            }
            None => {
                let Some((source_kind, source_detail)) = source_metadata(thread) else {
                    plan.skipped.push(SkipReason {
                        thread_id: Some(thread.id.clone()),
                        reason: "unknown_source".into(),
                    });
                    continue;
                };
                let title = catalog_title(thread);
                plan.catalog_inserts.push(CatalogInsert {
                    host_id: "local".into(),
                    thread_id: thread.id.clone(),
                    title,
                    created_at: catalog_time(thread.created_at),
                    updated_at: catalog_time(thread.updated_at),
                    cwd: thread.cwd.clone(),
                    source_detail,
                    source_kind,
                    provider: canonical_provider(target_provider).into(),
                    git_branch: None,
                });
                plan.changed_ids.insert(thread.id.clone());
            }
        }
    }
    sort_reasons(&mut plan.skipped);
    plan
}

fn apply_state_updates(
    transaction: &Transaction<'_>,
    updates: &[String],
    target_provider: &str,
) -> Result<usize, String> {
    let mut statement = transaction
        .prepare("UPDATE threads SET model_provider = ?1 WHERE id = ?2")
        .map_err(|error| error.to_string())?;
    let mut count = 0;
    for id in updates {
        count += statement
            .execute(params![canonical_provider(target_provider), id])
            .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn apply_catalog_updates(
    transaction: &Transaction<'_>,
    updates: &[CatalogUpdate],
    target_provider: &str,
    sequence: i64,
) -> Result<usize, String> {
    let mut count = 0;
    for update in updates {
        count += transaction.execute("UPDATE local_thread_catalog SET model_provider = ?1, observation_sequence = ?2, missing_candidate = 0 WHERE host_id = ?3 AND thread_id = ?4", params![canonical_provider(target_provider), sequence, update.host_id, update.thread_id]).map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn apply_catalog_inserts(
    transaction: &Transaction<'_>,
    inserts: &[CatalogInsert],
    sequence: i64,
) -> Result<usize, String> {
    let mut count = 0;
    for insert in inserts {
        count += transaction.execute("INSERT OR IGNORE INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, source_detail, model_provider, git_branch, observation_sequence, missing_candidate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)", params![insert.host_id, insert.thread_id, insert.title, insert.created_at, insert.updated_at, insert.cwd, insert.source_kind, insert.source_detail, insert.provider, insert.git_branch, sequence]).map_err(|error| error.to_string())?;
    }
    Ok(count)
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> bool {
    transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |_| Ok(()),
        )
        .is_ok()
}

fn catalog_next_sequence(transaction: &Transaction<'_>) -> Result<i64, String> {
    if table_exists(transaction, "local_thread_catalog_sync_state") {
        return transaction
            .query_row(
                "SELECT observation_sequence + 1 FROM local_thread_catalog_sync_state WHERE host_id='local'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("local catalog sync state unavailable: {error}"));
    }
    Err("unsupported catalog schema: sync state table missing".into())
}

fn update_catalog_watermarks(transaction: &Transaction<'_>, sequence: i64) -> Result<(), String> {
    let sync_changed = transaction
        .execute(
            "UPDATE local_thread_catalog_sync_state SET observation_sequence=?1 WHERE host_id='local'",
            params![sequence],
        )
        .map_err(|error| error.to_string())?;
    if sync_changed != 1 {
        return Err("local catalog sync state row missing".into());
    }
    let revision_changed = transaction
        .execute(
            "UPDATE local_thread_catalog_metadata SET catalog_revision=catalog_revision+1 WHERE id=1",
            [],
        )
        .map_err(|error| error.to_string())?;
    if revision_changed != 1 {
        return Err("local catalog metadata row missing".into());
    }
    Ok(())
}

fn apply_plan(
    home: &Path,
    plan: &RepairPlan,
    target_provider: &str,
) -> Result<(usize, usize), String> {
    ensure_home_sqlite_paths(home)?;
    let mut provider_count = 0;
    let mut index_count = 0;
    if !plan.state_updates.is_empty() {
        let mut connection =
            Connection::open(home.join("state_5.sqlite")).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = apply_state_updates(&transaction, &plan.state_updates, target_provider)?;
        if changed != plan.state_updates.len() {
            return Err(format!(
                "state update count mismatch: expected {}, got {changed}",
                plan.state_updates.len()
            ));
        }
        provider_count += changed;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    if !plan.catalog_updates.is_empty() || !plan.catalog_inserts.is_empty() {
        let mut connection = Connection::open(home.join("sqlite/codex-dev.db"))
            .map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let sequence = catalog_next_sequence(&transaction)?;
        let updated = apply_catalog_updates(
            &transaction,
            &plan.catalog_updates,
            target_provider,
            sequence,
        )?;
        if updated != plan.catalog_updates.len() {
            return Err(format!(
                "catalog update count mismatch: expected {}, got {updated}",
                plan.catalog_updates.len()
            ));
        }
        let inserted = apply_catalog_inserts(&transaction, &plan.catalog_inserts, sequence)?;
        if inserted != plan.catalog_inserts.len() {
            return Err(format!(
                "catalog insert count mismatch: expected {}, got {inserted}",
                plan.catalog_inserts.len()
            ));
        }
        provider_count += updated;
        index_count += inserted;
        update_catalog_watermarks(&transaction, sequence)?;
        transaction.commit().map_err(|error| error.to_string())?;
    }
    Ok((provider_count, index_count))
}

fn eligible_threads(
    snapshot: &Snapshot,
    target_provider: &str,
) -> (HashSet<String>, Vec<SkipReason>) {
    let cohorts = session_cohorts(snapshot);
    let mut ids = HashSet::new();
    let mut skipped = Vec::new();
    for thread in &snapshot.threads {
        if thread.archived {
            continue;
        }
        if let Some(reason) = repair_exclusion_reason(snapshot, &cohorts, thread) {
            skipped.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason,
            });
        } else {
            ids.insert(thread.id.clone());
        }
    }
    if !ALLOWED_PROVIDERS.contains(&target_provider) {
        skipped.push(SkipReason {
            thread_id: None,
            reason: "unsupported_target_provider".into(),
        });
    }
    (ids, skipped)
}

pub fn verify_at(home: &Path, target_provider: &str) -> Result<VerifyResult, String> {
    let target_provider = validate_provider(target_provider)?;
    let target_value = canonical_provider(&target_provider);
    let snapshot = scan_snapshot(home);
    if !snapshot.threads_readable || !snapshot.catalog_readable {
        return Err("verification unavailable: required SQLite sources are not readable".into());
    }
    let (eligible, mut reasons) = eligible_threads(&snapshot, &target_provider);
    let cohorts = session_cohorts(&snapshot);
    let thread_ids: HashSet<_> = snapshot.threads.iter().map(|row| row.id.clone()).collect();
    let mut discovered_ids = thread_ids.clone();
    discovered_ids.extend(
        snapshot
            .catalog
            .iter()
            .filter(|row| catalog_row_is_local(row))
            .map(|row| row.thread_id.clone()),
    );
    discovered_ids.extend(snapshot.rollouts.iter().cloned());
    discovered_ids.extend(snapshot.session_index.iter().cloned());
    reasons.extend(
        discovered_ids
            .difference(&thread_ids)
            .filter(|id| !cohorts.remote_catalog_ids.contains(*id))
            .map(|id| SkipReason {
                thread_id: Some(id.clone()),
                reason: "orphan_without_thread_row".into(),
            }),
    );
    let mut seen_skips = HashSet::new();
    reasons.retain(|reason| seen_skips.insert((reason.thread_id.clone(), reason.reason.clone())));
    let skipped = reasons.len();
    let local_catalog: HashMap<_, _> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .map(|row| (row.thread_id.as_str(), row))
        .collect();
    let mut remaining = 0;
    for thread in &snapshot.threads {
        if !eligible.contains(&thread.id) {
            continue;
        }
        let state_mismatch = thread.provider != target_value;
        if state_mismatch {
            reasons.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason: "state_provider_mismatch".into(),
            });
        }
        let catalog_issue = if thread.archived {
            None
        } else {
            match local_catalog.get(thread.id.as_str()) {
                None => Some("catalog_missing"),
                Some(row) if row.missing_candidate => Some("catalog_missing_candidate"),
                Some(row) if row.provider != target_value => Some("catalog_provider_mismatch"),
                Some(_) => None,
            }
        };
        if let Some(reason) = catalog_issue {
            reasons.push(SkipReason {
                thread_id: Some(thread.id.clone()),
                reason: reason.into(),
            });
        }
        if state_mismatch || catalog_issue.is_some() {
            remaining += 1;
        }
    }
    sort_reasons(&mut reasons);
    Ok(VerifyResult {
        ok: remaining == 0,
        checked: eligible.len(),
        remaining,
        skipped,
        reasons,
    })
}

pub fn scan_at(home: &Path) -> Result<ScanResult, String> {
    let snapshot = scan_snapshot(home);
    let current = current_provider(home);
    let cohorts = session_cohorts(&snapshot);
    let thread_ids: HashSet<_> = snapshot.threads.iter().map(|row| row.id.clone()).collect();
    let local_rows: Vec<_> = snapshot
        .catalog
        .iter()
        .filter(|row| catalog_row_is_local(row))
        .collect();
    let mut all_ids = thread_ids.clone();
    all_ids.extend(snapshot.catalog.iter().map(|row| row.thread_id.clone()));
    all_ids.extend(snapshot.rollouts.iter().cloned());
    all_ids.extend(snapshot.session_index.iter().cloned());
    let orphaned_ids = all_ids
        .difference(&thread_ids)
        .filter(|id| !cohorts.remote_catalog_ids.contains(*id))
        .cloned()
        .collect::<HashSet<_>>();
    let automated_sessions = snapshot
        .threads
        .iter()
        .filter(|thread| {
            !thread.archived
                && is_skipped_thread(thread).as_deref() == Some("subagent_or_automation")
        })
        .count();
    let skipped_state_ids = snapshot
        .threads
        .iter()
        .filter(|thread| !thread.archived)
        .filter(|thread| {
            is_skipped_thread(thread).is_some()
                || cohorts.remote_excluded_thread_ids.contains(&thread.id)
                || cohorts.missing_rollout_ids.contains(&thread.id)
        })
        .map(|thread| thread.id.clone())
        .collect::<HashSet<_>>();
    let missing_catalog = cohorts
        .recoverable_ids
        .difference(&cohorts.local_catalog_ids)
        .cloned()
        .collect::<HashSet<_>>();
    let rollout_provider_drift = snapshot
        .threads
        .iter()
        .filter(|thread| cohorts.recoverable_ids.contains(&thread.id))
        .filter(|thread| {
            snapshot
                .rollout_providers
                .get(&thread.id)
                .is_some_and(|providers| {
                    providers
                        .iter()
                        .any(|provider| provider != &normalize_provider(&thread.provider))
                })
        })
        .count();
    let mut provider_drift_ids = HashSet::new();
    for thread in &snapshot.threads {
        if let Some(row) = local_rows.iter().find(|row| row.thread_id == thread.id) {
            if cohorts.recoverable_ids.contains(&thread.id)
                && !row.missing_candidate
                && thread.provider != row.provider
            {
                provider_drift_ids.insert(thread.id.clone());
            }
        }
    }
    let provider_drift = provider_drift_ids.len();
    let mut drift_ids = missing_catalog.clone();
    drift_ids.extend(provider_drift_ids);
    let mut provider_counts: HashMap<String, (usize, usize)> = HashMap::new();
    for thread in &snapshot.threads {
        let provider = normalize_provider(&thread.provider);
        if !cohorts.recoverable_ids.contains(&thread.id) {
            continue;
        }
        let entry = provider_counts.entry(provider.clone()).or_default();
        entry.0 += 1;
        if local_rows.iter().any(|row| {
            row.thread_id == thread.id
                && !row.missing_candidate
                && row.provider == canonical_provider(&provider)
        }) {
            entry.1 += 1;
        }
    }
    let providers = ALLOWED_PROVIDERS
        .iter()
        .map(|id| {
            let (sessions, indexed) = provider_counts.get(*id).copied().unwrap_or_default();
            ProviderSummary {
                id: (*id).into(),
                name: provider_name(id).into(),
                color: provider_color(id).into(),
                sessions,
                indexed,
                status: if *id == current {
                    "active".into()
                } else if sessions == 0 {
                    "available".into()
                } else {
                    "legacy".into()
                },
            }
        })
        .collect();
    let lock_detail = inspect_lock(home);
    let mut sources = snapshot.sources;
    sources.push(global_state_source(home));
    let needs_admin = lock_detail
        .active_processes
        .iter()
        .any(|process| process == "process-enumeration-failed")
        || sources.iter().any(|source| {
            let note = source.note.to_ascii_lowercase();
            note.contains("permission")
                || note.contains("access")
                || note.contains("denied")
                || note.contains("readonly")
                || note.contains("elevation")
                || ["拒绝访问", "需要提升权限", "不允许访问", "只读"]
                    .iter()
                    .any(|keyword| source.note.contains(keyword))
        });
    Ok(ScanResult {
        codex_home: home.to_string_lossy().to_string(),
        current_provider: current,
        providers,
        sessions: thread_ids.len(),
        discovered_sessions: all_ids.len(),
        orphaned_sessions: orphaned_ids.len(),
        archived_sessions: snapshot
            .threads
            .iter()
            .filter(|thread| thread.archived)
            .count(),
        ordinary_sessions: cohorts.ordinary_active_ids.len(),
        recoverable_sessions: cohorts.recoverable_ids.len(),
        recoverable_indexed: cohorts.recoverable_indexed_ids.len(),
        session_index_covered: cohorts.session_index_covered_ids.len(),
        remote_sessions: cohorts.remote_session_ids.len(),
        remote_excluded_sessions: cohorts.remote_excluded_thread_ids.len(),
        automated_sessions,
        rollout_sessions: snapshot.rollouts.len(),
        valid_rollout_sessions: snapshot.valid_active_rollouts.len()
            + snapshot.valid_archived_rollouts.len(),
        indexed: cohorts.local_catalog_ids.len(),
        session_indexed: snapshot.session_index.len(),
        drift: drift_ids.len(),
        provider_drift,
        rollout_provider_drift,
        missing_catalog: missing_catalog.len(),
        missing_rollout: cohorts.missing_rollout_ids.len(),
        skipped: skipped_state_ids.len() + orphaned_ids.len(),
        sqlite: snapshot.sqlite_readable,
        jsonl: snapshot.jsonl_files.len(),
        lock: lock_detail.state.clone(),
        lock_detail,
        needs_admin,
        last_backup: latest_backup(home).map(|path| path.to_string_lossy().to_string()),
        sources,
    })
}

fn create_lock_with_command(home: &Path, command: &str) -> Result<File, String> {
    let path = lock_path(home);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot acquire repair lock: {error}"))?;
    let lock = LockFile {
        pid: std::process::id(),
        created_at: Local::now().to_rfc3339(),
        command: command.into(),
    };
    file.write_all(&serde_json::to_vec(&lock).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    Ok(file)
}

fn create_lock(home: &Path) -> Result<File, String> {
    create_lock_with_command(home, "repair")
}

fn remove_lock(home: &Path) {
    let _ = fs::remove_file(lock_path(home));
}

pub fn repair_at(
    home: &Path,
    target_provider: &str,
    dry_run: bool,
    check_processes: bool,
) -> Result<RepairResult, String> {
    let target_provider = validate_provider(target_provider)?;
    if !dry_run && normalize_provider(&current_provider(home)) != target_provider {
        return Err(format!(
            "target provider must match config.toml model_provider (current: {})",
            current_provider(home)
        ));
    }
    let snapshot = scan_snapshot(home);
    let plan = build_plan(&snapshot, &target_provider);
    let changed = plan.changed_ids.len();
    if dry_run {
        if !snapshot.threads_readable || !snapshot.catalog_readable {
            return Err("dry-run unavailable: required SQLite sources are not readable".into());
        }
        validate_repair_schema(home)?;
        return Ok(RepairResult {
            changed,
            providers_fixed: plan.state_updates.len() + plan.catalog_updates.len(),
            index_added: plan.catalog_inserts.len(),
            skipped: plan.skipped.len(),
            skipped_reasons: plan.skipped,
            dry_run,
            verified: false,
            backup_path: None,
            lock: inspect_lock(home).state,
            needs_admin: false,
        });
    }
    if changed == 0 {
        validate_repair_schema(home)?;
        let verification = verify_at(home, &target_provider)?;
        return Ok(RepairResult {
            changed: 0,
            providers_fixed: 0,
            index_added: 0,
            skipped: plan.skipped.len(),
            skipped_reasons: plan.skipped,
            dry_run: false,
            verified: verification.ok,
            backup_path: None,
            lock: inspect_lock(home).state,
            needs_admin: false,
        });
    }
    let lock_state = inspect_lock(home);
    if check_processes && !lock_state.active_processes.is_empty() {
        return Err(format!(
            "active Codex processes detected: {}",
            lock_state.active_processes.join(", ")
        ));
    }
    if Path::new(&lock_state.path).is_file() {
        if lock_state.state == "stale" {
            recover_stale_lock(home, &lock_state)?;
        } else {
            return Err(format!(
                "repair blocked by lock state: {}",
                lock_state.state
            ));
        }
    }
    validate_repair_schema(home)?;
    let _lock = create_lock(home)?;
    for path in [
        home.join("state_5.sqlite"),
        home.join("sqlite/codex-dev.db"),
    ] {
        if let Err(error) = probe_sqlite_write_lock(&path) {
            remove_lock(home);
            return Err(error);
        }
    }
    let snapshot = scan_snapshot(home);
    if !snapshot.threads_readable || !snapshot.catalog_readable {
        remove_lock(home);
        return Err("repair aborted: SQLite sources changed or became unreadable".into());
    }
    let plan = build_plan(&snapshot, &target_provider);
    let changed = plan.changed_ids.len();
    if changed == 0 {
        let verification = verify_at(home, &target_provider);
        remove_lock(home);
        return verification.map(|verification| RepairResult {
            changed: 0,
            providers_fixed: 0,
            index_added: 0,
            skipped: plan.skipped.len(),
            skipped_reasons: plan.skipped,
            dry_run: false,
            verified: verification.ok,
            backup_path: None,
            lock: "clear".into(),
            needs_admin: false,
        });
    }
    let backup = match create_backup_at(home) {
        Ok(backup) => backup,
        Err(error) => {
            remove_lock(home);
            return Err(error);
        }
    };
    let apply_result = apply_plan(home, &plan, &target_provider);
    let result = match apply_result {
        Ok((providers_fixed, index_added)) => match verify_at(home, &target_provider) {
            Ok(verify) if verify.ok => Ok(RepairResult {
                changed,
                providers_fixed,
                index_added,
                skipped: plan.skipped.len(),
                skipped_reasons: plan.skipped.clone(),
                dry_run: false,
                verified: true,
                backup_path: Some(backup.path),
                lock: "clear".into(),
                needs_admin: false,
            }),
            verification => {
                let reason = match verification {
                    Ok(verify) => format!("{} records remain", verify.remaining),
                    Err(error) => error,
                };
                match restore_backup_unchecked(home, Some(Path::new(&backup.path)))
                    .and_then(|_| validate_repair_schema(home))
                {
                    Ok(()) => Err(format!("verification failed; restored backup: {reason}")),
                    Err(restore_error) => Err(format!(
                        "verification failed: {reason}; restore failed: {restore_error}"
                    )),
                }
            }
        },
        Err(error) => {
            let restore_result = restore_backup_unchecked(home, Some(Path::new(&backup.path)))
                .and_then(|_| validate_repair_schema(home));
            if let Err(restore_error) = restore_result {
                Err(format!(
                    "repair failed: {error}; restore failed: {restore_error}"
                ))
            } else {
                Err(format!("repair failed and was restored: {error}"))
            }
        }
    };
    remove_lock(home);
    result.map_err(|error| {
        if error.to_ascii_lowercase().contains("permission")
            || error.to_ascii_lowercase().contains("access")
        {
            format!("{error}; administrator permission may be required")
        } else {
            error
        }
    })
}

pub fn restore_latest_at(home: &Path) -> Result<VerifyResult, String> {
    restore_backup_at(home, None)?;
    verify_at(home, &current_provider(home))
}

pub fn run_cli() -> i32 {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "codex-provider-hub scan|repair|verify|restore [BACKUP] \
             [--codex-home PATH] [--target-provider ID] [--dry-run|--apply]\n\
             repair defaults to dry-run; --apply is required for SQLite writes"
        );
        return 0;
    }
    let command = args.remove(0);
    let mut home = default_codex_home();
    let mut target = None;
    let mut dry_run = true;
    let mut restore_path = None;
    let mut parse_error = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--codex-home" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    home = PathBuf::from(value);
                } else {
                    parse_error = Some("--codex-home requires a path".to_string());
                }
            }
            "--target-provider" => {
                index += 1;
                if let Some(value) = args.get(index) {
                    target = Some(value.clone());
                } else {
                    parse_error = Some("--target-provider requires an ID".to_string());
                }
            }
            "--dry-run" => dry_run = true,
            "--apply" | "--write" => dry_run = false,
            value if command == "restore" && !value.starts_with('-') => {
                if restore_path.is_some() {
                    parse_error = Some("restore accepts at most one backup path".to_string());
                } else {
                    restore_path = Some(PathBuf::from(value));
                }
            }
            value => parse_error = Some(format!("unknown argument: {value}")),
        }
        if parse_error.is_some() {
            break;
        }
        index += 1;
    }
    let result: Result<Value, String> = if let Some(error) = parse_error {
        Err(error)
    } else {
        match command.as_str() {
            "scan" => scan_at(&home).map(|value| json!(value)),
            "repair" => {
                let target = target.unwrap_or_else(|| current_provider(&home));
                repair_at(&home, &target, dry_run, true).map(|value| json!(value))
            }
            "verify" => {
                let target = target.unwrap_or_else(|| current_provider(&home));
                verify_at(&home, &target).map(|value| json!(value))
            }
            "restore" => restore_backup_at(&home, restore_path.as_deref()).and_then(|_| {
                let scan = scan_at(&home)?;
                let verification = verify_at(&home, &scan.current_provider)?;
                Ok(json!({ "ok": verification.ok, "verification": verification }))
            }),
            _ => Err(format!("unknown command: {command}")),
        }
    };
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
            );
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("provider-hub-core-{nonce}"))
    }

    fn make_fixture() -> PathBuf {
        let home = fixture();
        fs::create_dir_all(home.join("sessions/2026/07/13")).unwrap();
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        fs::create_dir_all(home.join("sqlite")).unwrap();
        fs::write(home.join("config.toml"), "model_provider = \"openai\"\n").unwrap();
        for (id, minute) in [
            ("thread-one", "00"),
            ("thread-two", "01"),
            ("thread-three", "02"),
            ("thread-subagent", "03"),
            ("thread-explicit-remote", "04"),
        ] {
            fs::write(home.join(format!("sessions/2026/07/13/{id}.jsonl")), format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"2026-07-13T00:{minute}:00Z\",\"model_provider\":\"custom\"}}}}\n")).unwrap();
        }
        fs::write(home.join("archived_sessions/thread-archived.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-archived\",\"timestamp\":\"2026-07-12T23:59:00Z\",\"model_provider\":\"custom\"}}\n").unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"thread-one\"}\n{\"id\":\"thread-two\"}\n{\"id\":\"thread-three\"}\n",
        )
        .unwrap();
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        state.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, source TEXT NOT NULL, model_provider TEXT NOT NULL, cwd TEXT NOT NULL, title TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0, agent_role TEXT, thread_source TEXT);").unwrap();
        for id in [
            "thread-one",
            "thread-two",
            "thread-three",
            "thread-missing-rollout",
        ] {
            state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES (?1, '', 0, 0, 'cli', 'custom', '', ?1, 0, NULL, 'user')", params![id]).unwrap();
        }
        state
            .execute(
                "UPDATE threads SET source='vscode' WHERE id='thread-three'",
                [],
            )
            .unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-explicit-remote', '', 0, 0, '{\"kind\":\"vscode\",\"remoteAuthority\":\"ssh-remote+devbox\"}', 'custom', '/work', 'Remote VS Code', 0, NULL, 'user')", []).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-archived', '', 0, 0, 'cli', 'custom', '', 'Archived', 1, NULL, 'user')", []).unwrap();
        state.execute("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, archived, agent_role, thread_source) VALUES ('thread-subagent', '', 0, 0, 'automation', 'custom', '', 'Subagent', 0, 'worker', 'subagent')", []).unwrap();
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        catalog.execute_batch("CREATE TABLE local_thread_catalog (host_id TEXT NOT NULL, thread_id TEXT NOT NULL, display_title TEXT NOT NULL, source_created_at REAL NOT NULL, source_updated_at REAL NOT NULL, cwd TEXT NOT NULL, source_kind TEXT NOT NULL, source_detail TEXT, model_provider TEXT NOT NULL, git_branch TEXT, observation_sequence INTEGER NOT NULL, missing_candidate INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(host_id, thread_id)); CREATE TABLE local_thread_catalog_sync_state (host_id TEXT PRIMARY KEY, watermark_updated_at REAL, initial_build_complete INTEGER NOT NULL DEFAULT 0, observation_sequence INTEGER NOT NULL DEFAULT 0); INSERT INTO local_thread_catalog_sync_state (host_id, observation_sequence) VALUES ('local', 1); CREATE TABLE local_thread_catalog_metadata (id INTEGER PRIMARY KEY, catalog_revision INTEGER NOT NULL DEFAULT 0); INSERT INTO local_thread_catalog_metadata (id, catalog_revision) VALUES (1, 1);").unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, model_provider, observation_sequence, missing_candidate) VALUES ('local', 'thread-one', 'One', 0, 0, '', 'local', 'custom', 1, 0)", []).unwrap();
        catalog.execute("INSERT INTO local_thread_catalog (host_id, thread_id, display_title, source_created_at, source_updated_at, cwd, source_kind, model_provider, observation_sequence, missing_candidate) VALUES ('remote-host', 'thread-two', 'Remote two', 0, 0, '', 'remote', 'CodexPilot', 1, 0)", []).unwrap();
        drop(catalog);
        home
    }

    #[test]
    fn dry_run_repair_verify_restore_is_idempotent() {
        let home = make_fixture();
        let scan = scan_at(&home).unwrap();
        assert_eq!(scan.sessions, 7);
        assert_eq!(scan.archived_sessions, 1);
        assert_eq!(scan.ordinary_sessions, 5);
        assert_eq!(scan.recoverable_sessions, 2);
        assert_eq!(scan.recoverable_indexed, 1);
        assert_eq!(scan.session_index_covered, 2);
        assert_eq!(scan.remote_sessions, 2);
        assert_eq!(scan.remote_excluded_sessions, 2);
        assert_eq!(scan.missing_rollout, 1);
        assert_eq!(scan.automated_sessions, 1);
        assert_eq!(scan.missing_catalog, 1);
        assert_eq!(scan.skipped, 4);
        let state_before = fs::read(home.join("state_5.sqlite")).unwrap();
        let catalog_before = fs::read(home.join("sqlite/codex-dev.db")).unwrap();
        let config_before = fs::read(home.join("config.toml")).unwrap();
        let index_before = fs::read(home.join("session_index.jsonl")).unwrap();
        let rollout_before = fs::read(home.join("sessions/2026/07/13/thread-two.jsonl")).unwrap();
        let dry = repair_at(&home, "openai", true, false).unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.changed, 2);
        assert!(dry
            .skipped_reasons
            .iter()
            .any(|reason| reason.reason == "remote_mapped"));
        assert!(dry
            .skipped_reasons
            .iter()
            .any(|reason| reason.reason == "rollout_missing_or_ambiguous"));
        assert_eq!(fs::read(home.join("state_5.sqlite")).unwrap(), state_before);
        assert_eq!(
            fs::read(home.join("sqlite/codex-dev.db")).unwrap(),
            catalog_before
        );
        let repaired = repair_at(&home, "openai", false, false).unwrap();
        assert!(repaired.verified);
        assert_eq!(repaired.index_added, 1);
        assert_eq!(verify_at(&home, "openai").unwrap().remaining, 0);
        assert_eq!(scan_at(&home).unwrap().rollout_provider_drift, 2);
        let state = Connection::open(home.join("state_5.sqlite")).unwrap();
        assert_eq!(
            state
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE model_provider='OpenAI'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        for id in [
            "thread-two",
            "thread-explicit-remote",
            "thread-missing-rollout",
            "thread-subagent",
            "thread-archived",
        ] {
            assert_eq!(
                state
                    .query_row(
                        "SELECT model_provider FROM threads WHERE id=?1",
                        params![id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                "custom"
            );
        }
        assert_eq!(
            state
                .query_row(
                    "SELECT model_provider FROM threads WHERE id='thread-subagent'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "custom"
        );
        drop(state);
        let catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND model_provider='OpenAI'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT model_provider FROM local_thread_catalog WHERE host_id='remote-host' AND thread_id='thread-two'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "CodexPilot"
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local' AND thread_id='thread-two'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT observation_sequence FROM local_thread_catalog_sync_state WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        assert_eq!(
            catalog
                .query_row(
                    "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        drop(catalog);
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), config_before);
        assert_eq!(
            fs::read(home.join("session_index.jsonl")).unwrap(),
            index_before
        );
        assert_eq!(
            fs::read(home.join("sessions/2026/07/13/thread-two.jsonl")).unwrap(),
            rollout_before
        );
        let second = repair_at(&home, "openai", false, false).unwrap();
        assert_eq!(second.changed, 0);
        restore_backup_unchecked(&home, None).unwrap();
        assert_eq!(verify_at(&home, "custom").unwrap().remaining, 1);
        let restored = Connection::open(home.join("state_5.sqlite")).unwrap();
        let provider: String = restored
            .query_row(
                "SELECT model_provider FROM threads WHERE id='thread-one'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(provider, "custom");
        drop(restored);
        let restored_catalog = Connection::open(home.join("sqlite/codex-dev.db")).unwrap();
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT COUNT(*) FROM local_thread_catalog WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT observation_sequence FROM local_thread_catalog_sync_state WHERE host_id='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            restored_catalog
                .query_row(
                    "SELECT catalog_revision FROM local_thread_catalog_metadata WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        drop(restored_catalog);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn scan_and_dry_run_do_not_create_sqlite_sidecars() {
        let home = make_fixture();
        let databases = [
            home.join("state_5.sqlite"),
            home.join("sqlite/codex-dev.db"),
        ];
        for database in &databases {
            let connection = Connection::open(database).unwrap();
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            drop(connection);
            for suffix in ["-wal", "-shm"] {
                let sidecar = sidecar_path(database, suffix);
                if sidecar.exists() {
                    fs::remove_file(sidecar).unwrap();
                }
            }
        }

        scan_at(&home).unwrap();
        repair_at(&home, "openai", true, false).unwrap();

        for database in &databases {
            for suffix in ["-wal", "-shm"] {
                assert!(
                    !sidecar_path(database, suffix).exists(),
                    "read-only flow created {suffix} for {}",
                    database.display()
                );
            }
        }
        fs::remove_dir_all(home).unwrap();
    }
}
