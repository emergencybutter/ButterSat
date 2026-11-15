//! Cache location: the imagery store (`imagery.redb`) and the mesh cache
//! (`mesh/`) live under one cache folder, which the user can put on any
//! drive (`[storage] cache_path`). Relocation moves them (rename on the same
//! volume, copy + delete across volumes) or starts fresh at the new place.

use std::path::{Path, PathBuf};

use xof_proto::StorageInfo;

/// What lives in a cache folder.
pub const ITEMS: [&str; 2] = ["imagery.redb", "mesh"];

/// The closest existing ancestor (free space is a property of the volume).
fn existing_ancestor(p: &Path) -> Option<PathBuf> {
    let mut cur = Some(p);
    while let Some(c) = cur {
        if c.exists() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

pub fn space(p: &Path) -> (u64, u64) {
    match existing_ancestor(p) {
        Some(a) => (fs4::available_space(&a).unwrap_or(0), fs4::total_space(&a).unwrap_or(0)),
        None => (0, 0),
    }
}

/// Can we create files there? Creates the folder if needed (a fresh folder
/// is what the user asked for anyway) and writes a probe file.
pub fn writable(p: &Path) -> bool {
    if std::fs::create_dir_all(p).is_err() {
        return false;
    }
    let probe = p.join(".buttersat-write-test");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

pub fn size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| size(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

pub fn cache_size(root: &Path) -> u64 {
    ITEMS.iter().map(|i| size(&root.join(i))).sum()
}

pub fn info(candidate: &Path, current: &Path) -> StorageInfo {
    let (available_bytes, total_bytes) = space(candidate);
    StorageInfo {
        path: candidate.display().to_string(),
        exists: candidate.exists(),
        writable: false, // filled by the caller (creates the folder)
        available_bytes,
        total_bytes,
        current_cache_bytes: cache_size(current),
        current_path: current.display().to_string(),
    }
}

/// Reject targets that can't work before anything is touched.
pub fn validate(from: &Path, to: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(to.is_absolute(), "choose a full folder path");
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['/', '\\']).to_ascii_lowercase();
    let (f, t) = (norm(from), norm(to));
    anyhow::ensure!(f != t, "the cache is already there");
    let sep = |s: &str, prefix: &str| s.starts_with(prefix) && s[prefix.len()..].starts_with(['/', '\\']);
    anyhow::ensure!(!sep(&t, &f) && !sep(&f, &t), "the new folder can't be inside the current cache (or contain it)");
    for i in ITEMS {
        anyhow::ensure!(!to.join(i).exists(), "{} already contains a ButterSat cache ({i})", to.display());
    }
    Ok(())
}

/// Move `from/<item>` to `to/<item>` for every cache item. `progress(done,
/// total)` reports bytes. Renames when possible; otherwise copies everything
/// first and deletes the originals only once all copies succeeded.
pub fn move_cache(from: &Path, to: &Path, mut progress: impl FnMut(u64, u64)) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    let items: Vec<&str> = ITEMS.iter().copied().filter(|i| from.join(i).exists()).collect();
    let total: u64 = items.iter().map(|i| size(&from.join(i))).sum();
    // Same volume: renames are instant and atomic per item.
    let mut renamed = Vec::new();
    for i in &items {
        if std::fs::rename(from.join(i), to.join(i)).is_ok() {
            renamed.push(*i);
        } else {
            break;
        }
    }
    if renamed.len() == items.len() {
        progress(total, total);
        return Ok(());
    }
    // Different volume (rename failed): undo partial renames, then copy.
    for i in &renamed {
        std::fs::rename(to.join(i), from.join(i))?;
    }
    let (avail, _) = space(to);
    anyhow::ensure!(
        avail > total + (512 << 20),
        "not enough free space at {} ({:.1} GB free, {:.1} GB needed)",
        to.display(),
        avail as f64 / 1e9,
        total as f64 / 1e9
    );
    let mut done = 0u64;
    let copied: anyhow::Result<()> = (|| {
        for i in &items {
            copy_tree(&from.join(i), &to.join(i), &mut done, &mut |d| progress(d, total))?;
        }
        Ok(())
    })();
    if let Err(e) = copied {
        for i in &items {
            let _ = remove(&to.join(i));
        }
        return Err(e);
    }
    for i in &items {
        remove(&from.join(i))?;
    }
    progress(total, total);
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path, done: &mut u64, progress: &mut dyn FnMut(u64)) -> anyhow::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()), done, progress)?;
        }
    } else {
        use std::io::{Read, Write};
        let mut r = std::fs::File::open(src)?;
        let mut w = std::fs::File::create(dst)?;
        let mut buf = vec![0u8; 4 << 20];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            *done += n as u64;
            progress(*done);
        }
        w.sync_all()?;
    }
    Ok(())
}

fn remove(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else if p.exists() {
        std::fs::remove_file(p)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(root: &Path) {
        std::fs::create_dir_all(root.join("mesh/objects")).unwrap();
        std::fs::write(root.join("imagery.redb"), vec![1u8; 5000]).unwrap();
        std::fs::write(root.join("mesh/objects/a.dsf.zst"), vec![2u8; 3000]).unwrap();
        std::fs::write(root.join("unrelated.txt"), b"keep").unwrap();
    }

    #[test]
    fn moves_items_and_reports_progress() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("old"), t.path().join("new/cache"));
        fill(&from);
        assert_eq!(cache_size(&from), 8000);
        validate(&from, &to).unwrap();
        let mut last = (0, 0);
        move_cache(&from, &to, |d, n| last = (d, n)).unwrap();
        assert_eq!(last, (8000, 8000));
        assert_eq!(std::fs::read(to.join("mesh/objects/a.dsf.zst")).unwrap().len(), 3000);
        assert!(!from.join("imagery.redb").exists() && !from.join("mesh").exists());
        assert!(from.join("unrelated.txt").exists(), "only cache items move");
    }

    #[test]
    fn copy_path_matches_rename_path() {
        // Exercise the cross-volume branch directly.
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("a"), t.path().join("b"));
        fill(&from);
        let mut done = 0;
        copy_tree(&from.join("mesh"), &to.join("mesh"), &mut done, &mut |_| {}).unwrap();
        assert_eq!(done, 3000);
        assert_eq!(size(&to.join("mesh")), 3000);
    }

    #[test]
    fn rejects_bad_targets() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("cache");
        fill(&from);
        assert!(validate(&from, &from).is_err(), "same folder");
        assert!(validate(&from, &from.join("sub")).is_err(), "inside");
        assert!(validate(&from.join("sub"), &from).is_err(), "contains");
        assert!(validate(&from, Path::new("relative/dir")).is_err(), "relative");
        let other = t.path().join("other");
        fill(&other);
        assert!(validate(&from, &other).is_err(), "already holds a cache");
        assert!(validate(&from, &t.path().join("cache2")).is_ok(), "sibling with a common prefix is fine");
    }
}
