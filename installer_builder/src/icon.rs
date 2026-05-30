//! Copy the icon resources from the packaged exe into the installer and
//! uninstaller .exe so Explorer shows the right thumbnail.
//!
//! Strategy: read every `RT_GROUP_ICON` from the source (the user's exe) and
//! every `RT_ICON` it references via `LoadLibraryExW + LOAD_LIBRARY_AS_DATAFILE`,
//! then `BeginUpdateResourceW / UpdateResourceW / EndUpdateResourceW` to write
//! them into the target. RT_RCDATA (our payload) and RT_ICON are different
//! resource types so they don't collide.

#![cfg(windows)]

use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::path::Path;
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::LibraryLoader::{
    BeginUpdateResourceW, EndUpdateResourceW, EnumResourceNamesW, FindResourceW,
    LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW, LoadResource,
    LockResource, SizeofResource, UpdateResourceW,
};
use windows::core::PCWSTR;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const LANG_NEUTRAL: u16 = 0;
/// Group-icon resource id we write into the target. Explorer picks the
/// lowest-id RT_GROUP_ICON for the file's thumbnail, so we always write 1.
const TARGET_GROUP_ID: u16 = 1;

pub struct ExeIcons {
    pub group_bytes: Vec<u8>,
    pub icons: Vec<(u16, Vec<u8>)>,
}

/// Read the first RT_GROUP_ICON from `exe` plus every RT_ICON it references.
/// Returns `Ok(None)` if the source exe has no icons (still a success).
pub fn extract_from_exe(exe: &Path) -> Result<Option<ExeIcons>> {
    let wide: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let hmod = LoadLibraryExW(
            PCWSTR(wide.as_ptr()),
            None,
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
        .with_context(|| format!("LoadLibraryEx {}", exe.display()))?;
        if hmod.is_invalid() {
            bail!("LoadLibraryEx returned null for {}", exe.display());
        }

        let groups = enum_resource_ids(hmod, RT_GROUP_ICON);
        if groups.is_empty() {
            let _ = FreeLibrary(hmod);
            return Ok(None);
        }

        // Pick the lowest-numbered group (matches Explorer's heuristic for
        // which group to use as the file thumbnail).
        let mut sorted = groups;
        sorted.sort();
        let group_id = sorted[0];

        let group_bytes = load_res(hmod, RT_GROUP_ICON, group_id)?;
        let icon_ids = parse_group_icon_ids(&group_bytes);

        let mut icons = Vec::with_capacity(icon_ids.len());
        for id in icon_ids {
            match load_res(hmod, RT_ICON, id as u32) {
                Ok(b) => icons.push((id, b)),
                Err(_) => continue,
            }
        }

        let _ = FreeLibrary(hmod);
        Ok(Some(ExeIcons {
            group_bytes,
            icons,
        }))
    }
}

unsafe fn enum_resource_ids(hmod: HMODULE, rt: u16) -> Vec<u32> {
    thread_local! {
        static FOUND: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    }
    FOUND.with(|f| f.borrow_mut().clear());

    unsafe extern "system" fn cb(
        _hmod: HMODULE,
        _ty: PCWSTR,
        name: PCWSTR,
        _l: isize,
    ) -> BOOL {
        let v = name.0 as usize;
        // IS_INTRESOURCE: high word is 0 → name is an integer ID.
        if v >> 16 == 0 {
            FOUND.with(|f| f.borrow_mut().push(v as u32));
        }
        TRUE
    }

    let _ = unsafe {
        EnumResourceNamesW(
            Some(hmod),
            PCWSTR(rt as usize as *const u16),
            Some(cb),
            0,
        )
    };
    FOUND.with(|f| f.borrow().clone())
}

unsafe fn load_res(hmod: HMODULE, rt: u16, id: u32) -> Result<Vec<u8>> {
    unsafe {
        let hres = FindResourceW(
            Some(hmod.into()),
            PCWSTR(id as usize as *const u16),
            PCWSTR(rt as usize as *const u16),
        );
        if hres.is_invalid() {
            bail!("FindResource type={} id={} missing", rt, id);
        }
        let size = SizeofResource(Some(hmod.into()), hres);
        if size == 0 {
            bail!("SizeofResource id={} returned 0", id);
        }
        let hglobal = LoadResource(Some(hmod.into()), hres).context("LoadResource")?;
        let ptr = LockResource(hglobal);
        if ptr.is_null() {
            bail!("LockResource id={} returned null", id);
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, size as usize);
        Ok(slice.to_vec())
    }
}

/// Parse the GRPICONDIR header and return every nID it references.
fn parse_group_icon_ids(bytes: &[u8]) -> Vec<u16> {
    // GRPICONDIR: WORD reserved, WORD type, WORD count, GRPICONDIRENTRY entries[count]
    // GRPICONDIRENTRY = 14 bytes; last 2 bytes = nID (resource id of RT_ICON).
    if bytes.len() < 6 {
        return Vec::new();
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 6 + i * 14;
        if off + 14 > bytes.len() {
            break;
        }
        let id = u16::from_le_bytes([bytes[off + 12], bytes[off + 13]]);
        out.push(id);
    }
    out
}

pub fn embed_icons(target: &Path, icons: &ExeIcons) -> Result<()> {
    let wide: Vec<u16> = target
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let h = BeginUpdateResourceW(PCWSTR(wide.as_ptr()), false)
            .with_context(|| format!("BeginUpdateResource {}", target.display()))?;
        if h.is_invalid() {
            bail!("BeginUpdateResource invalid handle for {}", target.display());
        }

        for (id, bytes) in &icons.icons {
            UpdateResourceW(
                h,
                PCWSTR(RT_ICON as usize as *const u16),
                PCWSTR(*id as usize as *const u16),
                LANG_NEUTRAL,
                Some(bytes.as_ptr() as *const _),
                bytes.len() as u32,
            )
            .with_context(|| format!("UpdateResource RT_ICON id={}", id))?;
        }

        UpdateResourceW(
            h,
            PCWSTR(RT_GROUP_ICON as usize as *const u16),
            PCWSTR(TARGET_GROUP_ID as usize as *const u16),
            LANG_NEUTRAL,
            Some(icons.group_bytes.as_ptr() as *const _),
            icons.group_bytes.len() as u32,
        )
        .context("UpdateResource RT_GROUP_ICON")?;

        EndUpdateResourceW(h, false).context("EndUpdateResource")?;
    }
    Ok(())
}
