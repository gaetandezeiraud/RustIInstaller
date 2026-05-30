//! Stage 1: runs from `<install_dir>\uninstall.exe`. Shows confirm dialog,
//! then does the bulk of cleanup (files, shortcuts, registry, empty subdirs).
//! When done, copies itself into `%TEMP%` and spawns Stage 2, then exits so
//! Stage 2 can delete `uninstall.exe` and the install_dir without lock issues.

use crate::cleanup;
use crate::ui::{self, StepCounter, UninstallParams};
use anyhow::{Context, Result};
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const DETACHED_PROCESS: u32 = 0x00000008;

pub fn run(silent: bool) -> Result<()> {
    let install_dir = cleanup::current_install_dir()?;
    let info = cleanup::read_info(&install_dir)?;
    let manifest = cleanup::read_manifest(&install_dir)?;

    if silent {
        return run_silent(&install_dir, &info, &manifest);
    }

    let total_steps =
        manifest.files.len() as u64 + 3 /* shortcuts + state + registry */;

    let install_dir_owned = install_dir.clone();
    let info_owned = info.clone();
    let manifest_owned = manifest.clone();

    let params = UninstallParams {
        title: format!("Uninstall {}", info.product),
        subtitle: format!("Version {}", info.version),
        confirm_text: format!(
            "Are you sure you want to remove {} {} from:\n{}\n\nAll product files, the desktop shortcut, the Start Menu shortcut, and the Add/Remove Programs entry will be deleted.",
            info.product, info.version, info.install_dir
        ),
        worker: Box::new(move |progress: Arc<dyn Fn(u64, u64, &str) + Send + Sync>| {
            let counter = StepCounter::new(total_steps, progress);

            // 1. Payload files
            for rel in manifest_owned.files.keys() {
                let p = install_dir_owned.join(rel);
                let _ = fs::remove_file(&p);
                counter.step(&format!("Removing {}", rel));
            }

            // 2. Shortcuts
            cleanup::remove_shortcuts(&info_owned.product);
            counter.step("Removing shortcuts");

            // 3. State files (manifest, version.json, installer_info.json — installer_info kept
            //    until just before spawn so stage 2 can still locate things if it needs to).
            //    We remove version.json + installer_manifest.json now; installer_info.json stays
            //    until stage 2 finishes so the user can still inspect it if cleanup aborts.
            for extra in ["version.json", "installer_manifest.json"] {
                let _ = fs::remove_file(install_dir_owned.join(extra));
            }
            counter.step("Removing state files");

            // 4. Empty subdirectories
            cleanup::remove_empty_subdirs(&install_dir_owned);
            counter.report("Finalizing...");

            // 5. Registry — last so the entry stays visible in Add/Remove Programs
            //    until we know cleanup actually ran.
            cleanup::unregister(&info_owned.registry_key);

            // 6. Spawn Stage 2 (separate temp copy) to finish the job.
            if let Err(e) = spawn_stage2(&install_dir_owned, &info_owned.product) {
                ui::fatal(&format!("Failed to spawn finalize step: {e:#}"));
            }
        }),
        auto_start: false,
    };

    let _ = ui::run(params);
    Ok(())
}

fn run_silent(
    install_dir: &Path,
    info: &common::models::InstallInfo,
    manifest: &common::models::Manifest,
) -> Result<()> {
    let _ = cleanup::remove_payload_files(install_dir, manifest);
    cleanup::remove_shortcuts(&info.product);
    let _ = cleanup::remove_state_files(install_dir);
    cleanup::remove_empty_subdirs(install_dir);
    cleanup::unregister(&info.registry_key);
    spawn_stage2(install_dir, &info.product)
}

fn spawn_stage2(install_dir: &Path, product: &str) -> Result<()> {
    let self_exe = std::env::current_exe()?;
    let dest = staged_temp_path()?;
    fs::copy(&self_exe, &dest)
        .with_context(|| format!("copy stage2 to {}", dest.display()))?;

    Command::new(&dest)
        .arg("--stage2")
        .arg(install_dir)
        .arg(product)
        .arg(std::process::id().to_string())
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("spawn {}", dest.display()))?;
    Ok(())
}

fn staged_temp_path() -> Result<std::path::PathBuf> {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rustinst-uninstall-{}-{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    Ok(p)
}
