//! Atom-level DSF handling and the Laminar raster splice (design §6.2).
//!
//! A DSF is `XPLNEDSF` + version (u32) + atoms + MD5 (16 bytes) of everything
//! before it. Atoms are `tag (4, byte-reversed) + length (u32 LE, including the
//! 8-byte header) + payload`. We only ever touch two atoms: `DEFN/DEMN` (raster
//! layer names) and the top-level `DEMS` (raster headers + data).

use std::path::Path;

use md5::{Digest, Md5};

pub const MAGIC: &[u8; 8] = b"XPLNEDSF";
const HEADER_LEN: usize = 12;
const MD5_LEN: usize = 16;

// Tags as stored on disk (byte-reversed).
pub const DEFN: [u8; 4] = *b"NFED";
pub const DEMN: [u8; 4] = *b"NMED";
pub const DEMS: [u8; 4] = *b"SMED";
pub const DEMI: [u8; 4] = *b"IMED";

#[derive(Debug, thiserror::Error)]
pub enum DsfError {
    #[error("not a DSF file")]
    BadMagic,
    #[error("truncated or corrupt atom structure")]
    Corrupt,
    #[error("missing atom {0}")]
    Missing(&'static str),
    #[error("7z: {0}")]
    SevenZ(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Iterate `(tag, payload)` over consecutive atoms.
pub fn atoms(mut buf: &[u8]) -> impl Iterator<Item = Result<([u8; 4], &[u8]), DsfError>> {
    std::iter::from_fn(move || {
        if buf.is_empty() {
            return None;
        }
        if buf.len() < 8 {
            buf = &[];
            return Some(Err(DsfError::Corrupt));
        }
        let tag: [u8; 4] = buf[..4].try_into().unwrap();
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        if len < 8 || len > buf.len() {
            buf = &[];
            return Some(Err(DsfError::Corrupt));
        }
        let payload = &buf[8..len];
        buf = &buf[len..];
        Some(Ok((tag, payload)))
    })
}

fn push_atom(out: &mut Vec<u8>, tag: [u8; 4], payload: &[u8]) {
    out.extend_from_slice(&tag);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

/// Split a DSF into its 12-byte header and atom area (MD5 excluded).
pub fn split(data: &[u8]) -> Result<(&[u8], &[u8]), DsfError> {
    if data.len() < HEADER_LEN + MD5_LEN || &data[..8] != MAGIC {
        return Err(DsfError::BadMagic);
    }
    Ok((&data[..HEADER_LEN], &data[HEADER_LEN..data.len() - MD5_LEN]))
}

/// Does the DSF's trailing MD5 match its contents?
pub fn md5_ok(data: &[u8]) -> bool {
    if data.len() < HEADER_LEN + MD5_LEN {
        return false;
    }
    let (body, sum) = data.split_at(data.len() - MD5_LEN);
    Md5::digest(body).as_slice() == sum
}

/// Laminar raster layers extracted from a Global Scenery DSF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rasters {
    /// `DEMN` payload: NUL-separated layer names.
    pub demn: Vec<u8>,
    /// `DEMS` payload (DEMI/DEMD sub-atoms), with the bathymetry clamp applied.
    pub dems: Vec<u8>,
}

impl Rasters {
    /// Bytes the splice adds to a stripped DSF.
    pub fn extra_len(&self) -> u64 {
        (self.demn.len() + 8 + self.dems.len()) as u64
    }

    pub fn layer_names(&self) -> Vec<String> {
        self.demn
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }
}

/// Extract `DEMN` + `DEMS` from an (uncompressed) Global Scenery DSF and apply
/// Ortho4XP's clamp: the 2nd large raster (`sea_level`) is limited to
/// `elevation - 2` (Laminar's inland bathymetry is partial).
pub fn extract_rasters(gs: &[u8]) -> Result<Option<Rasters>, DsfError> {
    let (_, body) = split(gs)?;
    let mut demn = None;
    let mut dems_raw = None;
    for a in atoms(body) {
        let (tag, payload) = a?;
        if tag == DEFN {
            for s in atoms(payload) {
                let (t, p) = s?;
                if t == DEMN {
                    demn = Some(p.to_vec());
                }
            }
        } else if tag == DEMS {
            dems_raw = Some(payload);
        }
    }
    let (Some(demn), Some(dems_raw)) = (demn, dems_raw) else {
        return Ok(None);
    };
    let mut dems = Vec::with_capacity(dems_raw.len());
    let mut elevation: Option<&[u8]> = None;
    let mut large = 0;
    let mut clamped = Vec::new();
    for s in atoms(dems_raw) {
        let (t, p) = s?;
        // Ortho4XP's rule: sub-atoms longer than 100 bytes (incl. header) are data.
        if p.len() + 8 > 100 {
            large += 1;
            if large == 1 {
                elevation = Some(p);
            } else if large == 2 {
                let elev = elevation.ok_or(DsfError::Corrupt)?;
                if elev.len() != p.len() {
                    return Err(DsfError::Corrupt);
                }
                clamped.clear();
                clamped.reserve(p.len());
                for (b, e) in p.chunks_exact(2).zip(elev.chunks_exact(2)) {
                    let bathy = i16::from_le_bytes([b[0], b[1]]);
                    // numpy int16 arithmetic wraps; replicate exactly.
                    let safe = i16::from_le_bytes([e[0], e[1]]).wrapping_sub(2);
                    clamped.extend_from_slice(&bathy.min(safe).to_le_bytes());
                }
                push_atom(&mut dems, t, &clamped);
                continue;
            }
        }
        push_atom(&mut dems, t, p);
    }
    Ok(Some(Rasters { demn, dems }))
}

pub const TERT: [u8; 4] = *b"TRET";

/// Terrain definitions (`DEFN/TERT`), e.g. `terrain/24560_19344_XO16.ter`:
/// for an Ortho4XP mesh, the textures X-Plane will read for this tile.
/// Only the header part of the file is needed (`DEFN` precedes geometry).
pub fn terrain_names(dsf: &[u8]) -> Result<Vec<String>, DsfError> {
    if dsf.len() < HEADER_LEN || &dsf[..8] != MAGIC {
        return Err(DsfError::BadMagic);
    }
    // Walk atoms from the header; tolerate a missing MD5/tail (partial reads).
    for a in atoms(&dsf[HEADER_LEN..]) {
        let Ok((tag, payload)) = a else { break };
        if tag == DEFN {
            let mut out = Vec::new();
            for s in atoms(payload) {
                let (t, p) = s?;
                if t == TERT {
                    out.extend(
                        p.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()),
                    );
                }
            }
            return Ok(out);
        }
    }
    Err(DsfError::Missing("DEFN"))
}

/// Does this DSF carry raster data? Hosted DSFs must not (publish guard).
pub fn has_raster_data(dsf: &[u8]) -> Result<bool, DsfError> {
    let (_, body) = split(dsf)?;
    for a in atoms(body) {
        let (tag, payload) = a?;
        if tag == DEMS {
            return Ok(true);
        }
        if tag == DEFN {
            for s in atoms(payload) {
                let (t, p) = s?;
                if t == DEMN && !p.is_empty() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Splice `rasters` into a stripped DSF: replace `DEFN/DEMN`, drop any `DEMS`,
/// append a new `DEMS` after the last atom, and recompute the MD5. The output
/// is byte-identical to Ortho4XP writing the DSF with Global Scenery enabled.
pub fn splice(stripped: &[u8], rasters: &Rasters) -> Result<Vec<u8>, DsfError> {
    let (header, body) = split(stripped)?;
    let mut out = Vec::with_capacity(stripped.len() + rasters.extra_len() as usize);
    out.extend_from_slice(header);
    let mut saw_defn = false;
    for a in atoms(body) {
        let (tag, payload) = a?;
        if tag == DEMS {
            continue;
        }
        if tag == DEFN {
            saw_defn = true;
            let mut defn = Vec::with_capacity(payload.len() + rasters.demn.len());
            let mut saw_demn = false;
            for s in atoms(payload) {
                let (t, p) = s?;
                if t == DEMN {
                    saw_demn = true;
                    push_atom(&mut defn, t, &rasters.demn);
                } else {
                    push_atom(&mut defn, t, p);
                }
            }
            if !saw_demn {
                push_atom(&mut defn, DEMN, &rasters.demn);
            }
            push_atom(&mut out, tag, &defn);
        } else {
            push_atom(&mut out, tag, payload);
        }
    }
    if !saw_defn {
        return Err(DsfError::Missing("DEFN"));
    }
    push_atom(&mut out, DEMS, &rasters.dems);
    let sum = Md5::digest(&out);
    out.extend_from_slice(&sum);
    Ok(out)
}

/// Remove all raster data (empty `DEMN`, no `DEMS`) and recompute the MD5:
/// the inverse of [`splice`]. Used by the publisher so Laminar data can never
/// be uploaded even if a mesh was built with Global Scenery enabled.
pub fn strip_rasters(dsf: &[u8]) -> Result<Vec<u8>, DsfError> {
    let (header, body) = split(dsf)?;
    let mut out = Vec::with_capacity(dsf.len());
    out.extend_from_slice(header);
    for a in atoms(body) {
        let (tag, payload) = a?;
        if tag == DEMS {
            continue;
        }
        if tag == DEFN {
            let mut defn = Vec::with_capacity(payload.len());
            for s in atoms(payload) {
                let (t, p) = s?;
                push_atom(&mut defn, t, if t == DEMN { &[] } else { p });
            }
            push_atom(&mut out, tag, &defn);
        } else {
            push_atom(&mut out, tag, payload);
        }
    }
    let sum = Md5::digest(&out);
    out.extend_from_slice(&sum);
    Ok(out)
}

/// Size of `splice(stripped, rasters)` for a conforming stripped DSF (empty
/// `DEMN`, no `DEMS`), without doing the splice.
pub fn spliced_len(stripped_len: u64, rasters_extra: u64) -> u64 {
    stripped_len + rasters_extra
}

/// Read a DSF from disk, transparently decompressing 7z archives (as used by
/// X-Plane's Global Scenery).
pub fn read_dsf_file(path: &Path) -> Result<Vec<u8>, DsfError> {
    let data = std::fs::read(path)?;
    if data.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return un7z_single(&data);
    }
    Ok(data)
}

/// Decompress the single entry of a 7z archive held in memory.
pub fn un7z_single(data: &[u8]) -> Result<Vec<u8>, DsfError> {
    let mut reader = sevenz_rust2::ArchiveReader::new(std::io::Cursor::new(data), sevenz_rust2::Password::empty())
        .map_err(|e| DsfError::SevenZ(e.to_string()))?;
    let mut out = None;
    reader
        .for_each_entries(|entry, r| {
            if out.is_none() && !entry.is_directory() {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                r.read_to_end(&mut buf)?;
                out = Some(buf);
            } else {
                std::io::copy(r, &mut std::io::sink())?;
            }
            Ok(true)
        })
        .map_err(|e| DsfError::SevenZ(e.to_string()))?;
    out.ok_or_else(|| DsfError::SevenZ("empty archive".into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a tiny synthetic DSF with the given atoms.
    pub fn make_dsf(atoms_in: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&1u32.to_le_bytes());
        for (t, p) in atoms_in {
            push_atom(&mut out, *t, p);
        }
        let sum = Md5::digest(&out);
        out.extend_from_slice(&sum);
        out
    }

    fn defn(demn: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        push_atom(&mut d, *b"TRET", b"terrain/a.ter\0");
        push_atom(&mut d, DEMN, demn);
        d
    }

    pub fn stripped() -> Vec<u8> {
        make_dsf(&[
            (*b"DAEH", b"props".to_vec()),
            (DEFN, defn(b"")),
            (*b"DOEG", vec![1, 2, 3]),
            (*b"SDMC", vec![9; 10]),
        ])
    }

    pub fn global(elev: i16, bathy: i16) -> Vec<u8> {
        let n = 64; // > 100 bytes per data sub-atom
        let mut dems = Vec::new();
        push_atom(&mut dems, DEMI, &[0u8; 20]);
        push_atom(&mut dems, *b"DMED", &elev.to_le_bytes().repeat(n));
        push_atom(&mut dems, DEMI, &[0u8; 20]);
        push_atom(&mut dems, *b"DMED", &bathy.to_le_bytes().repeat(n));
        push_atom(&mut dems, DEMI, &[0u8; 20]);
        push_atom(&mut dems, *b"DMED", &[7u8; 16]); // small layer
        make_dsf(&[(*b"DAEH", b"gs".to_vec()), (DEFN, defn(b"elevation\0sea_level\0spr1\0")), (DEMS, dems)])
    }

    #[test]
    fn splice_roundtrip() {
        let s = stripped();
        assert!(md5_ok(&s));
        assert!(!has_raster_data(&s).unwrap());
        let r = extract_rasters(&global(100, 150)).unwrap().unwrap();
        assert_eq!(r.layer_names(), ["elevation", "sea_level", "spr1"]);
        let out = splice(&s, &r).unwrap();
        assert!(md5_ok(&out));
        assert!(has_raster_data(&out).unwrap());
        assert_eq!(out.len() as u64, spliced_len(s.len() as u64, r.extra_len()));
        // Clamp: bathy 150 -> elevation - 2 = 98.
        let again = extract_rasters(&out).unwrap().unwrap();
        let dems: Vec<_> = atoms(&again.dems).map(|a| a.unwrap()).collect();
        assert_eq!(i16::from_le_bytes([dems[3].1[0], dems[3].1[1]]), 98);
        // Splicing an already spliced file replaces, never duplicates.
        let twice = splice(&out, &r).unwrap();
        assert_eq!(twice.len(), out.len());
        // Strip is the exact inverse.
        assert_eq!(strip_rasters(&out).unwrap(), s);
    }

    #[test]
    fn lists_terrain_definitions() {
        let mut d = Vec::new();
        push_atom(
            &mut d,
            TERT,
            b"terrain/24560_19344_XO16.ter\0terrain/24560_19344_XO16_sea_overlay.ter\0lib/g8/x.ter\0",
        );
        push_atom(&mut d, DEMN, b"");
        let dsf = make_dsf(&[(*b"DAEH", b"p".to_vec()), (DEFN, d), (*b"DOEG", vec![0; 4])]);
        assert_eq!(
            terrain_names(&dsf).unwrap(),
            ["terrain/24560_19344_XO16.ter", "terrain/24560_19344_XO16_sea_overlay.ter", "lib/g8/x.ter"]
        );
        // Works on a prefix of the file (no MD5, truncated geometry).
        assert_eq!(terrain_names(&dsf[..dsf.len() - 20]).unwrap().len(), 3);
        assert!(terrain_names(&make_dsf(&[(*b"DAEH", vec![])])).is_err());
    }

    #[test]
    fn clamp_wraps_like_numpy() {
        let r = extract_rasters(&global(i16::MIN, 0)).unwrap().unwrap();
        let dems: Vec<_> = atoms(&r.dems).map(|a| a.unwrap()).collect();
        // -32768 - 2 wraps to 32766 in int16; min(0, 32766) = 0.
        assert_eq!(i16::from_le_bytes([dems[3].1[0], dems[3].1[1]]), 0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(split(b"nope"), Err(DsfError::BadMagic)));
        let mut bad = stripped();
        bad[16] = 0xff; // corrupt first atom length
        assert!(has_raster_data(&bad).is_err());
    }
}
