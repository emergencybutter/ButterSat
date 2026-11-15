//! Checks against the real NYC pilot outputs. Skipped unless
//! XOF_PILOT_DIR (containing a-stripped/ and b-reference/) and XOF_GS_DSF
//! (X-Plane's Global Scenery +40-074.dsf) are set.

use std::path::PathBuf;

use xof_mesh::dsf;

#[test]
fn rust_splice_matches_ortho4xp() {
    let (Ok(dir), Ok(gs)) = (std::env::var("XOF_PILOT_DIR"), std::env::var("XOF_GS_DSF")) else {
        eprintln!("skipped: XOF_PILOT_DIR / XOF_GS_DSF not set");
        return;
    };
    let dir = PathBuf::from(dir);
    let stripped = std::fs::read(dir.join("a-stripped/+40-074.dsf")).unwrap();
    let reference = std::fs::read(dir.join("b-reference/+40-074.dsf")).unwrap();
    assert!(!dsf::has_raster_data(&stripped).unwrap());
    let t = std::time::Instant::now();
    let gs = dsf::read_dsf_file(gs.as_ref()).unwrap();
    let rasters = dsf::extract_rasters(&gs).unwrap().unwrap();
    let out = dsf::splice(&stripped, &rasters).unwrap();
    eprintln!("un7z + extract + splice: {:?}", t.elapsed());
    assert_eq!(out.len() as u64, dsf::spliced_len(stripped.len() as u64, rasters.extra_len()));
    assert!(out == reference, "spliced output differs from Ortho4XP reference");
    assert_eq!(rasters.layer_names().len(), 11);
}

#[test]
fn strip_recovers_stripped_from_reference() {
    let Ok(dir) = std::env::var("XOF_PILOT_DIR") else {
        eprintln!("skipped: XOF_PILOT_DIR not set");
        return;
    };
    let dir = PathBuf::from(dir);
    let stripped = std::fs::read(dir.join("a-stripped/+40-074.dsf")).unwrap();
    let reference = std::fs::read(dir.join("b-reference/+40-074.dsf")).unwrap();
    assert!(dsf::strip_rasters(&reference).unwrap() == stripped);
}

#[test]
fn reference_dsf_lists_its_textures() {
    let Ok(dir) = std::env::var("XOF_PILOT_DIR") else {
        eprintln!("skipped: XOF_PILOT_DIR not set");
        return;
    };
    let dsf = std::fs::read(std::path::Path::new(&dir).join("b-reference/+40-074.dsf")).unwrap();
    let names = xof_mesh::dsf::terrain_names(&dsf).unwrap();
    let tex: std::collections::BTreeSet<_> = names
        .iter()
        .filter_map(|n| xof_core::TerName::parse(n.rsplit('/').next().unwrap()))
        .map(|t| t.texture.stem())
        .collect();
    // The Ortho4XP build of this tile has 130 textures (its textures/ folder).
    assert_eq!(tex.len(), 130, "{tex:?}");
    assert!(tex.iter().any(|t| t.ends_with("XO17")), "ZL17 airport textures listed");
}
