//! Stage 2: runs from `%TEMP%` after Stage 1 spawned us. Waits for the parent
//! process (Stage 1) to fully exit so the `uninstall.exe` lock is released,
//! then removes that file + the install_dir, and finally schedules our own
//! removal via `MoveFileExW(MOVEFILE_DELAY_UNTIL_REBOOT)` so Windows cleans
//! us up at next reboot. No `cmd.exe`, no console flash.

use crate::ui::{self, StepCounter, UninstallParams};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn run(install_dir: PathBuf, product: String, parent_pid: Option<u32>) -> Result<()> {
    let install_dir_for_worker = install_dir.clone();
    let product_for_worker = product.clone();

    let params = UninstallParams {
        title: format!("Removing {}", product),
        subtitle: "Finalizing uninstall…".to_string(),
        confirm_text: String::new(), // never shown — we auto-advance to Progress
        worker: Box::new(move |progress: Arc<dyn Fn(u64, u64, &str) + Send + Sync>| {
            // Wait for Stage 1 to exit so file locks release.
            if let Some(pid) = parent_pid {
                wait_for_pid(pid, Duration::from_secs(10));
            }

            // 6 logical steps: wait, delete uninstall.exe, delete install_dir
            // attempt (looped), schedule self-delete, done.
            let counter = StepCounter::new(5, progress);
            counter.step("Waiting for installer to exit");

            // Delete uninstall.exe with a retry loop in case the lock isn't
            // released immediately (AV scanner, Explorer thumb cache, etc.)
            let uninstall_exe = install_dir_for_worker.join("uninstall.exe");
            for _ in 0..50 {
                if !uninstall_exe.exists() {
                    break;
                }
                if fs::remove_file(&uninstall_exe).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            counter.step("Removing uninstaller");

            // Delete installer_info.json (Stage 1 left it on purpose).
            let _ = fs::remove_file(install_dir_for_worker.join("installer_info.json"));
            counter.step("Removing state");

            // Remove install_dir recursively, with retries.
            for _ in 0..30 {
                if !install_dir_for_worker.exists() {
                    break;
                }
                if fs::remove_dir_all(&install_dir_for_worker).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            counter.step("Removing install directory");

            // Schedule self for deletion on next reboot (no cmd, no flash).
            schedule_self_delete_on_reboot();
            counter.step("Done");

            // Brief pause so user sees the 100% bar.
            thread::sleep(Duration::from_millis(400));

            let _ = &product_for_worker;
        }),
        auto_start: true,
    };

    let _ = ui::run(params);
    Ok(())
}

fn wait_for_pid(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) if !h.is_invalid() => {
                let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
                let r = WaitForSingleObject(h, ms);
                let _ = CloseHandle(h);
                if r == WAIT_OBJECT_0 {
                    return;
                }
            }
            _ => {}
        }
    }
    // Fallback: short sleep so locks at least likely released.
    thread::sleep(Duration::from_millis(500));
}

fn schedule_self_delete_on_reboot() {
    use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
    use windows::core::PCWSTR;

    let Ok(self_exe) = std::env::current_exe() else {
        return;
    };
    let w: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(self_exe.as_os_str())
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MoveFileExW(PCWSTR(w.as_ptr()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
    }
}
