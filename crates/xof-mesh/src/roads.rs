//! X-Plane's roads from a Global Scenery DSF, for the night textures
//! (design §5.6).
//!
//! Roads are network chains in the DSF command stream (`CMDS`), their points
//! in the 32-bit coordinate pools (`GEOD/PO32`, scaled by `GEOD/SC32`: lon,
//! lat, elevation, junction id). The road subtype is set by command 6 before
//! the chains; network definitions (`DEFN/NETW`) name the `.net` file.
//! Everything else in the command stream is stepped over.

use xof_core::night::{RoadClass, Roads};

use crate::dsf::{atoms, split, DsfError, DEFN};

const GEOD: [u8; 4] = *b"DOEG";
const PO32: [u8; 4] = *b"23OP";
const SC32: [u8; 4] = *b"23CS";
const NETW: [u8; 4] = *b"WTEN";
const CMDS: [u8; 4] = *b"SDMC";

/// Decode a `PO32` atom: `n` points × `planes`, plane-major (`[plane][i]`).
/// Each plane is raw, differenced (bit 0) and/or run-length encoded (bit 1).
fn read_pool32(p: &[u8]) -> Result<(usize, Vec<Vec<u32>>), DsfError> {
    let bad = || DsfError::Corrupt;
    let n = u32::from_le_bytes(p.get(0..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let planes = *p.get(4).ok_or_else(bad)? as usize;
    let mut pos = 5;
    let rd = |pos: usize| -> Result<u32, DsfError> {
        Ok(u32::from_le_bytes(p.get(pos..pos + 4).ok_or(DsfError::Corrupt)?.try_into().unwrap()))
    };
    let mut out = Vec::with_capacity(planes);
    for _ in 0..planes {
        let enc = *p.get(pos).ok_or_else(bad)?;
        pos += 1;
        let mut col = Vec::with_capacity(n);
        if enc & 2 != 0 {
            while col.len() < n {
                let run = *p.get(pos).ok_or_else(bad)?;
                pos += 1;
                let count = (run & 0x7f) as usize;
                if col.len() + count > n {
                    return Err(bad());
                }
                if run & 0x80 != 0 {
                    let v = rd(pos)?;
                    pos += 4;
                    col.extend(std::iter::repeat(v).take(count));
                } else {
                    for _ in 0..count {
                        col.push(rd(pos)?);
                        pos += 4;
                    }
                }
            }
        } else {
            for _ in 0..n {
                col.push(rd(pos)?);
                pos += 4;
            }
        }
        if enc & 1 != 0 {
            let mut acc = 0u32;
            for v in &mut col {
                acc = acc.wrapping_add(*v);
                *v = acc;
            }
        }
        out.push(col);
    }
    Ok((n, out))
}

/// A decoded 32-bit pool: lon/lat of each point.
struct Pool {
    lonlat: Vec<[f64; 2]>,
}

fn scaled_pool(po: &[u8], sc: &[u8]) -> Result<Pool, DsfError> {
    let (n, planes) = read_pool32(po)?;
    if n == 0 {
        return Ok(Pool { lonlat: Vec::new() }); // empty pools exist (no scale)
    }
    if planes.len() < 2 || sc.len() < 16 {
        return Err(DsfError::Corrupt);
    }
    let f = |i: usize| f32::from_le_bytes(sc[i * 4..i * 4 + 4].try_into().unwrap()) as f64;
    let (s0, o0, s1, o1) = (f(0), f(1), f(2), f(3));
    let k = 1.0 / 4_294_967_295.0;
    let lonlat = (0..n).map(|i| [o0 + planes[0][i] as f64 * s0 * k, o1 + planes[1][i] as f64 * s1 * k]).collect();
    Ok(Pool { lonlat })
}

/// Is a network definition one of X-Plane's road nets (not e.g. a custom
/// power-line or rail-only net)?
fn is_road_net(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with("roads.net") || n.ends_with("roads_eu.net")
}

/// Extract the roads of an (uncompressed) DSF.
pub fn extract_roads(dsf: &[u8]) -> Result<Roads, DsfError> {
    let (_, body) = split(dsf)?;
    let mut nets: Vec<String> = Vec::new();
    let mut po: Vec<&[u8]> = Vec::new();
    let mut sc: Vec<&[u8]> = Vec::new();
    let mut cmds: Option<&[u8]> = None;
    for a in atoms(body) {
        let (tag, payload) = a?;
        match tag {
            DEFN => {
                for s in atoms(payload) {
                    let (t, p) = s?;
                    if t == NETW {
                        nets = p
                            .split(|&b| b == 0)
                            .filter(|s| !s.is_empty())
                            .map(|s| String::from_utf8_lossy(s).into_owned())
                            .collect();
                    }
                }
            }
            GEOD => {
                for s in atoms(payload) {
                    let (t, p) = s?;
                    if t == PO32 {
                        po.push(p);
                    } else if t == SC32 {
                        sc.push(p);
                    }
                }
            }
            CMDS => cmds = Some(payload),
            _ => {}
        }
    }
    let mut roads = Roads::default();
    let Some(cmds) = cmds else { return Ok(roads) };
    if po.len() != sc.len() {
        return Err(DsfError::Missing("SC32 per PO32"));
    }
    let pools: Vec<Pool> = po
        .iter()
        .zip(&sc)
        .map(|(p, s)| scaled_pool(p, s))
        .collect::<Result<_, _>>()
        .map_err(|_| DsfError::Missing("valid PO32"))?;
    let road_net: Vec<bool> = nets.iter().map(|n| is_road_net(n)).collect();
    walk(cmds, &pools, &road_net, &mut roads)?;
    Ok(roads)
}

fn walk(c: &[u8], pools: &[Pool], road_net: &[bool], out: &mut Roads) -> Result<(), DsfError> {
    let bad = || DsfError::Corrupt;
    let u8_at = |i: usize| c.get(i).copied().ok_or(DsfError::Corrupt);
    let u16_at = |i: usize| -> Result<usize, DsfError> {
        Ok(u16::from_le_bytes(c.get(i..i + 2).ok_or(DsfError::Corrupt)?.try_into().unwrap()) as usize)
    };
    let u32_at = |i: usize| -> Result<usize, DsfError> {
        Ok(u32::from_le_bytes(c.get(i..i + 4).ok_or(DsfError::Corrupt)?.try_into().unwrap()) as usize)
    };
    let (mut pos, mut pool, mut def, mut subtype, mut offset) = (0usize, 0usize, 0usize, 0u32, 0usize);
    let mut chain = |pool: usize, def: usize, subtype: u32, idx: &mut dyn Iterator<Item = usize>| {
        if !road_net.get(def).copied().unwrap_or(false) {
            return;
        }
        let Some((class, urban)) = RoadClass::from_subtype(subtype) else { return };
        let Some(p) = pools.get(pool) else { return };
        let pts: Vec<[f64; 2]> = idx.filter_map(|i| p.lonlat.get(i).copied()).collect();
        out.push(class, urban, pts);
    };
    while pos < c.len() {
        let cmd = c[pos];
        pos += 1;
        match cmd {
            1 => {
                pool = u16_at(pos)?;
                pos += 2;
            }
            2 => {
                offset = u32_at(pos)?;
                pos += 4;
            }
            3 => {
                def = u8_at(pos)? as usize;
                pos += 1;
            }
            4 => {
                def = u16_at(pos)?;
                pos += 2;
            }
            5 => {
                def = u32_at(pos)?;
                pos += 4;
            }
            6 => {
                subtype = u8_at(pos)? as u32;
                pos += 1;
            }
            7 => pos += 2,
            8 | 25 | 28 | 31 => pos += 4,
            9 => {
                let n = u8_at(pos)? as usize;
                let mut idx = Vec::with_capacity(n);
                for k in 0..n {
                    idx.push(u16_at(pos + 1 + 2 * k)? + offset);
                }
                pos += 1 + 2 * n;
                chain(pool, def, subtype, &mut idx.into_iter());
            }
            10 => {
                let (first, last) = (u16_at(pos)?, u16_at(pos + 2)?);
                pos += 4;
                chain(pool, def, subtype, &mut (first + offset..last + offset));
            }
            11 => {
                let n = u8_at(pos)? as usize;
                let mut idx = Vec::with_capacity(n);
                for k in 0..n {
                    idx.push(u32_at(pos + 1 + 4 * k)? + offset);
                }
                pos += 1 + 4 * n;
                chain(pool, def, subtype, &mut idx.into_iter());
            }
            12 => pos += 3 + 2 * u8_at(pos + 2)? as usize,
            13 => pos += 6,
            14 => {
                let windings = u8_at(pos + 2)?;
                pos += 3;
                for _ in 0..windings {
                    pos += 1 + 2 * u8_at(pos)? as usize;
                }
            }
            15 => pos += 3 + 2 * (u8_at(pos + 2)? as usize + 1),
            16 => {}
            17 => pos += 1,
            18 => pos += 9,
            23 | 26 | 29 => pos += 1 + 2 * u8_at(pos)? as usize,
            24 | 27 | 30 => pos += 1 + 4 * u8_at(pos)? as usize,
            32 => pos += 1 + u8_at(pos)? as usize,
            33 => pos += 2 + u16_at(pos)?,
            34 => pos += 4 + u32_at(pos)?,
            _ => {
                tracing::debug!("DSF command {cmd} at {}", pos - 1);
                return Err(bad());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsf::tests::make_dsf;

    fn atom(tag: [u8; 4], p: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend_from_slice(&((p.len() + 8) as u32).to_le_bytes());
        v.extend_from_slice(p);
        v
    }

    /// A PO32 pool of 4 planes: lon/lat raw, elevation RLE, junction diff+RLE.
    fn pool(lon: &[u32], lat: &[u32]) -> Vec<u8> {
        let n = lon.len();
        let mut p = (n as u32).to_le_bytes().to_vec();
        p.push(4);
        p.push(0);
        lon.iter().for_each(|v| p.extend_from_slice(&v.to_le_bytes()));
        p.push(1); // differenced
        let mut prev = 0u32;
        for v in lat {
            p.extend_from_slice(&v.wrapping_sub(prev).to_le_bytes());
            prev = *v;
        }
        p.push(2); // RLE: one run of n zeros
        p.push(0x80 | n as u8);
        p.extend_from_slice(&0u32.to_le_bytes());
        p.push(3); // RLE + differenced: literal run
        p.push(n as u8);
        (0..n).for_each(|_| p.extend_from_slice(&1u32.to_le_bytes()));
        p
    }

    fn sc32(lon0: f32, lat0: f32) -> Vec<u8> {
        let mut s = Vec::new();
        for v in [1.0f32, lon0, 1.0, lat0, 1.0, 0.0, 1.0, 0.0] {
            s.extend_from_slice(&v.to_le_bytes());
        }
        s
    }

    #[test]
    fn decodes_chains() {
        let q = |f: f64| (f * 4_294_967_295.0) as u32;
        let lon = [q(0.1), q(0.2), q(0.3), q(0.4), q(0.5)];
        let lat = [q(0.5), q(0.5), q(0.6), q(0.7), q(0.9)];
        let mut geod = atom(PO32, &pool(&lon, &lat));
        geod.extend(atom(SC32, &sc32(-74.0, 40.0)));
        let defn = atom(NETW, b"lib/g10/roads.net\0lib/g10/power.net\0");
        let mut cmds = Vec::new();
        cmds.extend([1, 0, 0]); // pool 0
        cmds.extend([3, 0]); // definition 0
        cmds.extend([32, 2, b'h', b'i']); // comment
        cmds.extend([6, 54]); // local with sidewalks
        cmds.extend([10, 0, 0, 3, 0]); // chain range 0..3
        cmds.extend([6, 110]); // motorway
        cmds.extend([2, 1, 0, 0, 0]); // junction offset 1
        cmds.extend([9, 2, 2, 0, 3, 0]); // chain 3, 4
        cmds.extend([7, 9, 0]); // object
        cmds.extend([6, 152]); // railway: skipped
        cmds.extend([10, 0, 0, 2, 0]);
        cmds.extend([3, 1, 6, 50, 10, 0, 0, 2, 0]); // power net: skipped
        cmds.extend([16, 23, 3, 0, 0, 1, 0, 2, 0]); // patch + triangles
        let dsf = make_dsf(&[(*b"DAEH", vec![]), (DEFN, defn), (GEOD, geod), (CMDS, cmds)]);
        let r = extract_roads(&dsf).unwrap();
        assert_eq!(r.roads.len(), 2);
        assert_eq!((r.roads[0].class, r.roads[0].urban), (RoadClass::Local, true));
        assert_eq!(r.points(&r.roads[0]).len(), 3);
        let p = r.points(&r.roads[0])[2];
        assert!((p[0] - -73.7).abs() < 1e-6 && (p[1] - 40.6).abs() < 1e-6, "{p:?}");
        assert_eq!(r.roads[1].class, RoadClass::Motorway);
        let m = r.points(&r.roads[1]);
        assert!((m[0][0] - -73.6).abs() < 1e-6 && (m[1][1] - 40.9).abs() < 1e-6, "{m:?}");
    }

    #[test]
    fn unknown_command_is_an_error() {
        let dsf = make_dsf(&[(*b"DAEH", vec![]), (CMDS, vec![99])]);
        assert!(extract_roads(&dsf).is_err());
    }

    /// Against the real Global Scenery when available:
    /// `XOF_GS_DSF=".../+40-074.dsf" cargo test -p xof-mesh real_gs -- --nocapture`
    #[test]
    fn real_gs() {
        let Some(p) = std::env::var_os("XOF_GS_DSF") else { return };
        let t = std::time::Instant::now();
        let data = crate::dsf::read_dsf_file(std::path::Path::new(&p)).unwrap();
        let t1 = t.elapsed();
        let r = extract_roads(&data).unwrap();
        let mut by: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
        for road in &r.roads {
            let e = by.entry(format!("{:?}/{}", road.class, road.urban)).or_default();
            e.0 += 1;
            e.1 += r.road_length_m(road) / 1000.0;
        }
        println!(
            "read {t1:?}, parsed {:?}: {} roads, {} pts, {:.0} km",
            t.elapsed() - t1,
            r.roads.len(),
            r.pts.len(),
            r.length_m() / 1000.0
        );
        for (k, (n, km)) in by {
            println!("  {k}: {n} chains, {km:.0} km");
        }
        assert!(!r.roads.is_empty());
    }
}

#[cfg(test)]
mod render_check {
    /// Render real night textures to PNG for a visual check:
    /// `XOF_GS_DSF=<dsf> XOF_NIGHT_OUT=<dir> XOF_NIGHT_AT=lat,lon cargo test -p xof-mesh real_render -- --nocapture`
    #[test]
    fn real_render() {
        let (Some(p), Some(out)) = (std::env::var_os("XOF_GS_DSF"), std::env::var_os("XOF_NIGHT_OUT")) else { return };
        let at = std::env::var("XOF_NIGHT_AT").unwrap_or_else(|_| "40.758,-73.985".into());
        let (lat, lon) = at.split_once(',').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())).unwrap();
        // XOF_GS_DSF may list several DSFs separated by ';'.
        let all: Vec<xof_core::night::Roads> = p
            .to_string_lossy()
            .split(';')
            .map(|f| super::extract_roads(&crate::dsf::read_dsf_file(std::path::Path::new(f)).unwrap()).unwrap())
            .collect();
        let refs: Vec<&xof_core::night::Roads> = all.iter().collect();
        for zl in [16u8, 17, 18] {
            let t = xof_core::TileCoord::from_latlon(lat, lon, zl);
            let tex = xof_core::TextureId::new(t.y & !15, t.x & !15, zl, "XO");
            let t0 = std::time::Instant::now();
            let dds = xof_core::night::render(&tex, &refs);
            println!("{} rendered in {:?}", tex, t0.elapsed());
            let img = xof_core::bc1::decode(&dds[128..128 + xof_core::night::lit_mip_len(0) as usize], 1024, 1024);
            image::save_buffer(
                std::path::Path::new(&out).join(format!("{}_LIT.png", tex)),
                &img.data,
                1024,
                1024,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        }
    }
}
