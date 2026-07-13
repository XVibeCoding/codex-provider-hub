pub mod core;

use core::{BackupResult, RepairResult, ScanResult, VerifyResult};
use std::path::PathBuf;

fn home() -> PathBuf {
    core::default_codex_home()
}

#[tauri::command]
fn scan_codex() -> Result<ScanResult, String> {
    core::scan_at(&home())
}

#[tauri::command]
fn create_backup() -> Result<BackupResult, String> {
    core::create_backup_safe_at(&home())
}

#[tauri::command]
fn repair_indexes(target_provider: String, dry_run: bool) -> Result<RepairResult, String> {
    core::repair_at(&home(), &target_provider, dry_run, true)
}

#[tauri::command]
fn verify_codex(target_provider: String) -> Result<VerifyResult, String> {
    core::verify_at(&home(), &target_provider)
}

#[tauri::command]
fn rollback_latest() -> Result<VerifyResult, String> {
    core::restore_latest_at(&home())
}

#[tauri::command]
fn restore_backup(backup_path: Option<String>) -> Result<VerifyResult, String> {
    let codex_home = home();
    let requested = backup_path.as_deref().map(std::path::Path::new);
    core::restore_backup_at(&codex_home, requested)?;
    let scan = core::scan_at(&codex_home)?;
    core::verify_at(&codex_home, &scan.current_provider)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            scan_codex,
            create_backup,
            repair_indexes,
            verify_codex,
            rollback_latest,
            restore_backup
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

pub fn run_cli() -> i32 {
    core::run_cli()
}
