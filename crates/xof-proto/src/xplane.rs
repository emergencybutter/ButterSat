//! X-Plane 12 discovery and `scenery_packs.ini` management (design §11).

use std::path::{Path, PathBuf};

/// Where X-Plane writes `x-plane_install_12.txt`.
pub fn install_list_file() -> Option<PathBuf> {
    if cfg!(windows) {
        dirs::data_local_dir().map(|d| d.join("x-plane_install_12.txt"))
    } else if cfg!(target_os = "macos") {
        dirs::home_dir().map(|h| h.join("Library/Preferences/x-plane_install_12.txt"))
    } else {
        dirs::home_dir().map(|h| h.join(".x-plane/x-plane_install_12.txt"))
    }
}

/// Is `p` a usable X-Plane 12 installation?
pub fn is_valid_install(p: &Path) -> bool {
    let exe = ["X-Plane.exe", "X-Plane.app", "X-Plane-x86_64", "X-Plane"].iter().any(|e| p.join(e).exists());
    exe && p.join("Custom Scenery").is_dir() && p.join("Resources").is_dir()
}

/// All valid installs listed by X-Plane (stale entries are skipped).
pub fn detect_installs() -> Vec<PathBuf> {
    let Some(list) = install_list_file() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(list) else { return vec![] };
    parse_install_list(&text).into_iter().filter(|p| is_valid_install(p)).collect()
}

pub fn parse_install_list(text: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let l = line.trim().trim_end_matches(['/', '\\']);
        if l.is_empty() {
            continue;
        }
        let p = native_path(Path::new(l));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Use the platform's separator throughout. X-Plane writes its install
/// list with forward slashes on Windows (`C:\Program Files (x86)/Steam/...`);
/// joined with `\` parts that gave mixed paths in the UI.
pub fn native_path(p: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(p.to_string_lossy().replace('/', "\\"))
    } else {
        p.to_path_buf()
    }
}

/// The configured path if valid, else the first detected install.
pub fn resolve(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        let p = native_path(p);
        return is_valid_install(&p).then_some(p);
    }
    detect_installs().into_iter().next()
}

pub fn global_scenery(xp: &Path) -> Option<PathBuf> {
    let p = xp.join("Global Scenery").join("X-Plane 12 Global Scenery");
    p.join("Earth nav data").is_dir().then_some(p)
}

pub fn default_mount_point(xp: &Path) -> PathBuf {
    xp.join("Custom Scenery").join(PACK_NAME)
}

pub const PACK_NAME: &str = "z_buttersat";

/// Insert our pack into `scenery_packs.ini` text. Returns `None` when no
/// change is needed. Placement: above the first ortho/mesh pack (anything
/// starting with `z`/`zz`, Ortho4XP/AutoOrtho packs, or "mesh" in the name);
/// otherwise at the end. A disabled entry is re-enabled in place.
pub fn insert_pack_entry(ini: &str) -> Option<String> {
    insert_named_pack_entry(ini, PACK_NAME)
}

/// [`insert_pack_entry`] for any folder of `Custom Scenery`.
pub fn insert_named_pack_entry(ini: &str, pack: &str) -> Option<String> {
    let entry = format!("SCENERY_PACK Custom Scenery/{pack}/");
    let lines: Vec<&str> = ini.lines().collect();
    let is_ours = |l: &str| pack_line_name(l).is_some_and(|n| n.eq_ignore_ascii_case(pack));
    if let Some(i) = lines.iter().position(|l| is_ours(l)) {
        if lines[i].trim().starts_with("SCENERY_PACK ") {
            return None;
        }
        let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        out[i] = entry;
        return Some(out.join("\n") + "\n");
    }
    let is_ortho_or_mesh = |l: &str| {
        let Some(path) =
            l.trim().strip_prefix("SCENERY_PACK ").or_else(|| l.trim().strip_prefix("SCENERY_PACK_DISABLED "))
        else {
            return false;
        };
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        name.starts_with('z') || name.contains("ortho4xp") || name.starts_with("z_ao") || name.contains("mesh")
    };
    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    match lines.iter().position(|l| is_ortho_or_mesh(l)) {
        Some(i) => out.insert(i, entry),
        None => {
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            out.push(entry);
        }
    }
    Some(out.join("\n") + "\n")
}

/// Folder name of a `Custom Scenery/<name>/` pack line (enabled or disabled).
pub fn pack_line_name(line: &str) -> Option<&str> {
    let t = line.trim();
    let path = t.strip_prefix("SCENERY_PACK_DISABLED ").or_else(|| t.strip_prefix("SCENERY_PACK "))?;
    path.trim_end_matches(['/', '\\']).strip_prefix("Custom Scenery/")
}

/// Enable or disable every pack line whose folder name satisfies `pred`.
pub fn set_packs_enabled(ini: &str, pred: impl Fn(&str) -> bool, enabled: bool) -> String {
    let mut out = String::with_capacity(ini.len());
    for l in ini.lines() {
        match pack_line_name(l) {
            Some(name) if pred(name) => {
                let kw = if enabled { "SCENERY_PACK" } else { "SCENERY_PACK_DISABLED" };
                out.push_str(&format!("{kw} Custom Scenery/{name}/"));
            }
            _ => out.push_str(l),
        }
        out.push('\n');
    }
    out
}

/// Apply [`insert_pack_entry`] to the install's ini, with a timestamped backup.
/// A missing ini is left for X-Plane to create (it adds new packs itself).
pub fn ensure_scenery_pack(xp: &Path) -> anyhow::Result<bool> {
    let ini = xp.join("Custom Scenery").join("scenery_packs.ini");
    let Ok(text) = std::fs::read_to_string(&ini) else { return Ok(false) };
    let Some(new) = insert_pack_entry(&text) else { return Ok(false) };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::copy(&ini, ini.with_file_name(format!("scenery_packs.ini.buttersat-{stamp}.bak")))?;
    let tmp = ini.with_extension("ini.tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(tmp, &ini)?;
    Ok(true)
}

/// Remove our pack line from `scenery_packs.ini` text. Returns `None` when not
/// present.
pub fn remove_pack_entry(ini: &str) -> Option<String> {
    remove_named_pack_entry(ini, PACK_NAME)
}

/// [`remove_pack_entry`] for any folder of `Custom Scenery`.
pub fn remove_named_pack_entry(ini: &str, pack: &str) -> Option<String> {
    let is_ours = |l: &str| pack_line_name(l).is_some_and(|n| n.eq_ignore_ascii_case(pack));
    if !ini.lines().any(is_ours) {
        return None;
    }
    let out: Vec<&str> = ini.lines().filter(|l| !is_ours(l)).collect();
    Some(out.join("\n") + "\n")
}

/// Remove our pack line from the install's `scenery_packs.ini` (with a
/// timestamped backup) and remove an empty `Custom Scenery/z_buttersat` folder
/// if left behind.
pub fn remove_scenery_pack(xp: &Path) -> anyhow::Result<bool> {
    let mp = default_mount_point(xp);
    let _ = std::fs::remove_dir(&mp);
    let ini = xp.join("Custom Scenery").join("scenery_packs.ini");
    let Ok(text) = std::fs::read_to_string(&ini) else { return Ok(false) };
    let Some(new) = remove_pack_entry(&text) else { return Ok(false) };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::copy(&ini, ini.with_file_name(format!("scenery_packs.ini.buttersat-{stamp}.bak")))?;
    let tmp = ini.with_extension("ini.tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(tmp, &ini)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "I\n1000 Version\nSCENERY\n\nSCENERY_PACK Custom Scenery/KJFK Airport/\nSCENERY_PACK *GLOBAL_AIRPORTS*\nSCENERY_PACK Custom Scenery/simHeaven_X-World_America/\nSCENERY_PACK Custom Scenery/zOrtho4XP_+40-074/\nSCENERY_PACK Custom Scenery/zzz_UHD_mesh/\n";

    #[test]
    fn inserts_above_first_ortho() {
        let out = insert_pack_entry(INI).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let ours = lines.iter().position(|l| l.contains(PACK_NAME)).unwrap();
        let ortho = lines.iter().position(|l| l.contains("zOrtho4XP")).unwrap();
        let overlay = lines.iter().position(|l| l.contains("simHeaven")).unwrap();
        assert!(overlay < ours && ours < ortho);
        assert!(insert_pack_entry(&out).is_none(), "idempotent");
    }

    #[test]
    fn appends_when_no_ortho_and_reenables() {
        let ini = "I\n1000 Version\nSCENERY\n\nSCENERY_PACK *GLOBAL_AIRPORTS*\n\n";
        let out = insert_pack_entry(ini).unwrap();
        assert!(out.trim_end().ends_with("SCENERY_PACK Custom Scenery/z_buttersat/"));
        let disabled = out
            .replace("SCENERY_PACK Custom Scenery/z_buttersat/", "SCENERY_PACK_DISABLED Custom Scenery/z_buttersat/");
        let re = insert_pack_entry(&disabled).unwrap();
        assert_eq!(re.matches("z_buttersat").count(), 1);
        assert!(re.contains("SCENERY_PACK Custom Scenery/z_buttersat/"));
    }

    #[test]
    #[cfg(windows)]
    fn install_paths_use_native_separators() {
        let l = parse_install_list("C:\\Program Files (x86)/Steam/steamapps/common/X-Plane 12/\r\n");
        assert_eq!(l, [PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\X-Plane 12")]);
        let mp = default_mount_point(&l[0]);
        assert!(!mp.to_string_lossy().contains('/'), "{}", mp.display());
    }

    #[test]
    fn stale_install_entries_are_parsed_then_filtered() {
        let l = parse_install_list("C:\\X-Plane 12/\nC:\\Program Files (x86)/Steam/steamapps/common/X-Plane 12/\n\n");
        assert_eq!(l.len(), 2);
        assert!(!is_valid_install(&l[0]) || l[0].exists());
    }

    #[test]
    fn removes_pack_entry_and_is_idempotent() {
        let inserted = insert_pack_entry(INI).unwrap();
        assert!(inserted.contains(PACK_NAME));
        let removed = remove_pack_entry(&inserted).unwrap();
        assert!(!removed.contains(PACK_NAME));
        assert_eq!(removed, INI);
        assert!(remove_pack_entry(&removed).is_none());
    }
}
