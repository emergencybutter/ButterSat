//! Scenery pack setup per variant. The original `scenery_packs.ini` is backed
//! up once and restored on drop (also after errors and Ctrl-C), and the
//! links we create in Custom Scenery are removed.

use std::path::{Path, PathBuf};

use xof_proto::xplane;

/// Prefix of the links the bench creates in Custom Scenery.
pub const LINK_PREFIX: &str = "z_xofbench_";

pub struct PackGuard {
    ini: PathBuf,
    custom: PathBuf,
    original: String,
    links: Vec<PathBuf>,
}

impl PackGuard {
    pub fn new(install: &Path) -> anyhow::Result<Self> {
        let custom = install.join("Custom Scenery");
        let ini = custom.join("scenery_packs.ini");
        let original = std::fs::read_to_string(&ini)
            .map_err(|e| anyhow::anyhow!("{}: {e} (start X-Plane once to create it)", ini.display()))?;
        let backup = custom.join("scenery_packs.ini.xofbench.bak");
        std::fs::write(&backup, &original)?;
        let g = Self { ini, custom, original, links: vec![] };
        g.remove_stale_links();
        Ok(g)
    }

    /// Links left behind by a crashed run.
    fn remove_stale_links(&self) {
        if let Ok(rd) = std::fs::read_dir(&self.custom) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(LINK_PREFIX) {
                    let _ = remove_link(&e.path());
                }
            }
        }
    }

    /// Configure packs for a variant: link `packs`, enable them and (if `xof`)
    /// our mount; disable the other bench packs, xporthofs when not under
    /// test, and `disable` (names, case-insensitive). With `isolate`, every
    /// other Custom Scenery pack is disabled too, including folders missing
    /// from the ini (X-Plane would add those enabled); `*GLOBAL_AIRPORTS*`
    /// stays.
    pub fn apply(
        &mut self,
        variant: &str,
        packs: &[PathBuf],
        xof: bool,
        disable: &[String],
        isolate: bool,
    ) -> anyhow::Result<()> {
        self.clear_links();
        let mut names = Vec::new();
        for (i, p) in packs.iter().enumerate() {
            let name = format!("{LINK_PREFIX}{variant}_{i}");
            let link = self.custom.join(&name);
            make_link(p, &link)?;
            self.links.push(link);
            names.push(name);
        }
        let mut base = self.original.clone();
        if isolate {
            base = add_unlisted_disabled(&base, &self.custom);
        }
        let mut ini = xplane::set_packs_enabled(
            &base,
            |n| {
                n.starts_with(LINK_PREFIX)
                    || (!xof && n.eq_ignore_ascii_case(xplane::PACK_NAME))
                    || disable.iter().any(|d| d.eq_ignore_ascii_case(n))
                    || (isolate && !n.eq_ignore_ascii_case(xplane::PACK_NAME))
            },
            false,
        );
        if xof {
            ini = xplane::insert_pack_entry(&ini).unwrap_or(ini);
        }
        // Insert in reverse so the first pack ends up highest.
        for n in names.iter().rev() {
            ini = xplane::insert_named_pack_entry(&ini, n).unwrap_or(ini);
        }
        write_atomic(&self.ini, &ini)
    }

    fn clear_links(&mut self) {
        for l in self.links.drain(..) {
            let _ = remove_link(&l);
        }
    }

    pub fn restore(&mut self) {
        self.clear_links();
        if let Err(e) = write_atomic(&self.ini, &self.original) {
            eprintln!("!! could not restore {}: {e} (backup: scenery_packs.ini.xofbench.bak)", self.ini.display());
        }
    }
}

impl Drop for PackGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Append a disabled line for every Custom Scenery folder the ini doesn't list.
fn add_unlisted_disabled(ini: &str, custom: &Path) -> String {
    let listed: Vec<String> = ini.lines().filter_map(xplane::pack_line_name).map(|n| n.to_ascii_lowercase()).collect();
    let mut out = ini.trim_end().to_string();
    out.push('\n');
    let mut extra: Vec<String> = std::fs::read_dir(custom)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with(LINK_PREFIX) && !listed.contains(&n.to_ascii_lowercase()))
                .collect()
        })
        .unwrap_or_default();
    extra.sort();
    for n in extra {
        out.push_str(&format!("SCENERY_PACK_DISABLED Custom Scenery/{n}/\n"));
    }
    out
}

fn write_atomic(p: &Path, text: &str) -> anyhow::Result<()> {
    let tmp = p.with_extension("ini.xofbench.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

#[cfg(windows)]
fn make_link(target: &Path, link: &Path) -> anyhow::Result<()> {
    // A junction needs no privileges (unlike a symlink).
    let win = |p: &Path| p.to_string_lossy().replace('/', "\\");
    anyhow::ensure!(target.is_absolute(), "link target {} is not absolute", target.display());
    let out =
        std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(win(link)).arg(win(target)).output()?;
    anyhow::ensure!(
        out.status.success(),
        "mklink /J {} {} failed: {}{}",
        win(link),
        win(target),
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(not(windows))]
fn make_link(target: &Path, link: &Path) -> anyhow::Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

fn remove_link(link: &Path) -> std::io::Result<()> {
    // Never recurse: removing the link must not touch the target.
    if cfg!(windows) {
        std::fs::remove_dir(link)
    } else {
        std::fs::remove_file(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_restore() {
        let root = std::env::temp_dir().join(format!("xofbench-packs-{}", std::process::id()));
        let custom = root.join("Custom Scenery");
        let pack = root.join("refpack");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::create_dir_all(pack.join("Earth nav data")).unwrap();
        let ini = "I\n1000 Version\nSCENERY\n\nSCENERY_PACK Custom Scenery/KJFK/\nSCENERY_PACK *GLOBAL_AIRPORTS*\nSCENERY_PACK Custom Scenery/nyc/\nSCENERY_PACK Custom Scenery/z_xporthofs/\nSCENERY_PACK Custom Scenery/zzz_mesh/\n";
        std::fs::write(custom.join("scenery_packs.ini"), ini).unwrap();
        {
            let mut g = PackGuard::new(&root).unwrap();
            g.apply("ref", &[pack.clone()], false, &["NYC".into()], false).unwrap();
            let now = std::fs::read_to_string(custom.join("scenery_packs.ini")).unwrap();
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/nyc/"));
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/z_xporthofs/"));
            let lines: Vec<&str> = now.lines().collect();
            let ours = lines.iter().position(|l| *l == "SCENERY_PACK Custom Scenery/z_xofbench_ref_0/").unwrap();
            let mesh = lines.iter().position(|l| l.contains("zzz_mesh")).unwrap();
            assert!(ours < mesh);
            assert!(custom.join("z_xofbench_ref_0/Earth nav data").is_dir());

            g.apply("xof", &[], true, &[], false).unwrap();
            let now = std::fs::read_to_string(custom.join("scenery_packs.ini")).unwrap();
            assert!(now.contains("SCENERY_PACK Custom Scenery/z_xporthofs/"));
            assert!(!custom.join("z_xofbench_ref_0").exists());

            // Isolation: everything else off, unlisted folders added disabled.
            std::fs::create_dir_all(custom.join("Unlisted Pack")).unwrap();
            g.apply("ref", &[pack.clone()], false, &[], true).unwrap();
            let now = std::fs::read_to_string(custom.join("scenery_packs.ini")).unwrap();
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/KJFK/"));
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/zzz_mesh/"));
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/Unlisted Pack/"));
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/z_xporthofs/"));
            assert!(now.contains("SCENERY_PACK *GLOBAL_AIRPORTS*"));
            assert!(now.contains("SCENERY_PACK Custom Scenery/z_xofbench_ref_0/"));
            g.apply("xof", &[], true, &[], true).unwrap();
            let now = std::fs::read_to_string(custom.join("scenery_packs.ini")).unwrap();
            assert!(now.contains("SCENERY_PACK Custom Scenery/z_xporthofs/"));
            assert!(now.contains("SCENERY_PACK_DISABLED Custom Scenery/KJFK/"));
        }
        assert_eq!(std::fs::read_to_string(custom.join("scenery_packs.ini")).unwrap(), ini);
        assert!(pack.join("Earth nav data").is_dir(), "link removal must not touch the target");
        let _ = std::fs::remove_dir_all(root);
    }
}
