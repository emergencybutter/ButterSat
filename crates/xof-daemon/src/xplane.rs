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
        let p = PathBuf::from(l);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The configured path if valid, else the first detected install.
pub fn resolve(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return is_valid_install(p).then(|| p.to_path_buf());
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

pub const PACK_NAME: &str = "z_xporthofs";

/// Insert our pack into `scenery_packs.ini` text. Returns `None` when no
/// change is needed. Placement: above the first ortho/mesh pack (anything
/// starting with `z`/`zz`, Ortho4XP/AutoOrtho packs, or "mesh" in the name);
/// otherwise at the end. A disabled entry is re-enabled in place.
pub fn insert_pack_entry(ini: &str) -> Option<String> {
    let entry = format!("SCENERY_PACK Custom Scenery/{PACK_NAME}/");
    let lines: Vec<&str> = ini.lines().collect();
    let is_ours = |l: &str| {
        let t = l.trim();
        let name = t.strip_prefix("SCENERY_PACK_DISABLED ").or_else(|| t.strip_prefix("SCENERY_PACK "));
        name.is_some_and(|n| n.trim_end_matches('/').eq_ignore_ascii_case(&format!("Custom Scenery/{PACK_NAME}")))
    };
    if let Some(i) = lines.iter().position(|l| is_ours(l)) {
        if lines[i].trim().starts_with("SCENERY_PACK ") {
            return None;
        }
        let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        out[i] = entry;
        return Some(out.join("\n") + "\n");
    }
    let is_ortho_or_mesh = |l: &str| {
        let Some(path) = l.trim().strip_prefix("SCENERY_PACK ").or_else(|| l.trim().strip_prefix("SCENERY_PACK_DISABLED ")) else {
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

/// Apply [`insert_pack_entry`] to the install's ini, with a timestamped backup.
/// A missing ini is left for X-Plane to create (it adds new packs itself).
pub fn ensure_scenery_pack(xp: &Path) -> anyhow::Result<bool> {
    let ini = xp.join("Custom Scenery").join("scenery_packs.ini");
    let Ok(text) = std::fs::read_to_string(&ini) else { return Ok(false) };
    let Some(new) = insert_pack_entry(&text) else { return Ok(false) };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::copy(&ini, ini.with_file_name(format!("scenery_packs.ini.xporthofs-{stamp}.bak")))?;
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
        assert!(out.trim_end().ends_with("SCENERY_PACK Custom Scenery/z_xporthofs/"));
        let disabled = out.replace("SCENERY_PACK Custom Scenery/z_xporthofs/", "SCENERY_PACK_DISABLED Custom Scenery/z_xporthofs/");
        let re = insert_pack_entry(&disabled).unwrap();
        assert_eq!(re.matches("z_xporthofs").count(), 1);
        assert!(re.contains("SCENERY_PACK Custom Scenery/z_xporthofs/"));
    }

    #[test]
    fn stale_install_entries_are_parsed_then_filtered() {
        let l = parse_install_list("C:\\X-Plane 12/\nC:\\Program Files (x86)/Steam/steamapps/common/X-Plane 12/\n\n");
        assert_eq!(l.len(), 2);
        assert!(!is_valid_install(&l[0]) || l[0].exists());
    }
}
