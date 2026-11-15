//! The platform-independent scenery tree (design §4.2): a merged view of
//! imported packs, the streamed mesh catalog, synthesized `.ter` files and
//! synthesized `.dds` textures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::runtime::Handle;
use xof_core::engine::{ReadCtx, ReaderClass, TextureEngine};
use xof_core::names::{ter_contents, MaskName, TerName, TerParams};
use xof_core::{dds, DsfTile, TextureId};
use xof_mesh::MeshStreamer;

/// `water_transition.png`, referenced by inland-water terrain (`BORDER_TEX`):
/// a 64×128 8-bit greyscale ramp, 0 at the top rising smoothly to ~253 at the
/// bottom, constant across each row. Generated here (our own asset).
pub fn water_transition_png() -> &'static [u8] {
    static PNG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PNG.get_or_init(|| grey_png(64, 128, water_ramp))
}

/// Ramp value for row `y` of 128: piecewise-linear through a smooth curve.
fn water_ramp(y: u32) -> u8 {
    const KNOTS: [u8; 17] = [0, 2, 4, 7, 10, 15, 22, 32, 46, 66, 92, 121, 153, 185, 213, 238, 253];
    if y >= 127 {
        return 253;
    }
    let seg = (y / 8) as usize;
    let t = (y % 8) as f32 / 8.0;
    let a = KNOTS[seg] as f32;
    let b = KNOTS[(seg + 1).min(16)] as f32;
    (a + (b - a) * t).round() as u8
}

/// Minimal PNG encoder (stored deflate) for an 8-bit greyscale image whose
/// rows are constant: `row(y)` gives the value of row `y`.
fn grey_png(w: u32, h: u32, row: fn(u32) -> u8) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn adler32(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &d in data {
            a = (a + d as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    }
    let mut raw = Vec::with_capacity(((w + 1) * h) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        raw.extend(std::iter::repeat(row(y)).take(w as usize));
    }
    // zlib stream with stored (uncompressed) deflate blocks of <= 65535 bytes.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, c) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8-bit greyscale
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// What a path resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Dir(String),
    /// A real file in an imported pack.
    Real(PathBuf),
    /// A streamed DSF.
    Dsf(DsfTile),
    /// A synthesized terrain file.
    Ter(TerName),
    /// A synthesized texture.
    Dds(TextureId),
    /// A streamed water mask.
    Mask(String),
    /// `textures/water_transition.png`.
    WaterTransition,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Dir(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: SystemTime,
}

/// Normalise a path relative to the mount root: `/` separators, no leading or
/// trailing slash, `.` removed.
pub fn normalize(path: &str) -> String {
    path.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/")
}

pub struct SceneryConfig {
    /// Imported Ortho4XP/AutoOrtho packs, highest priority first.
    pub packs: Vec<PathBuf>,
    pub ter: TerParams,
    /// Process names treated as X-Plane (case-insensitive).
    pub xplane_processes: Vec<String>,
}

pub struct Scenery {
    cfg: SceneryConfig,
    mesh: Option<Arc<MeshStreamer>>,
    engine: TextureEngine,
    rt: Handle,
    mount_time: SystemTime,
}

impl Scenery {
    pub fn new(cfg: SceneryConfig, mesh: Option<Arc<MeshStreamer>>, engine: TextureEngine, rt: Handle) -> Arc<Self> {
        Arc::new(Self { cfg, mesh, engine, rt, mount_time: SystemTime::now() })
    }

    pub fn engine(&self) -> &TextureEngine {
        &self.engine
    }

    pub fn mesh(&self) -> Option<&Arc<MeshStreamer>> {
        self.mesh.as_ref()
    }

    pub fn runtime(&self) -> &Handle {
        &self.rt
    }

    pub fn xplane_processes(&self) -> &[String] {
        &self.cfg.xplane_processes
    }

    /// Case-insensitive lookup of a real file in the imported packs.
    fn find_real(&self, rel: &str) -> Option<PathBuf> {
        for pack in &self.cfg.packs {
            let direct = pack.join(rel);
            if direct.exists() {
                return Some(direct);
            }
            if let Some(p) = find_ci(pack, rel) {
                return Some(p);
            }
        }
        None
    }

    fn streamed_tile(&self, folder: &str, file: &str) -> Option<DsfTile> {
        let mesh = self.mesh.as_ref()?;
        let name = strip_ext_ci(file, "dsf")?;
        let t = DsfTile::parse(name)?;
        if !t.folder().eq_ignore_ascii_case(folder) {
            return None;
        }
        mesh.entry(t)?;
        mesh.is_listed(t).then_some(t)
    }

    /// Resolve a mount-relative path.
    pub fn resolve(&self, path: &str) -> Option<Node> {
        let rel = normalize(path);
        let parts: Vec<&str> = if rel.is_empty() { vec![] } else { rel.split('/').collect() };
        let lower_owned: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
        let lower: Vec<&str> = lower_owned.iter().map(String::as_str).collect();
        match lower.as_slice() {
            [] => Some(Node::Dir(String::new())),
            ["earth nav data"] => Some(Node::Dir("Earth nav data".into())),
            ["terrain"] => Some(Node::Dir("terrain".into())),
            ["textures"] => Some(Node::Dir("textures".into())),
            ["earth nav data", folder] => {
                let found = self.list_dsf_folders().into_iter().find(|f| f.eq_ignore_ascii_case(folder));
                found.map(|f| Node::Dir(format!("Earth nav data/{f}")))
            }
            ["earth nav data", folder, file] => {
                if let Some(p) = self.find_real(&rel) {
                    return Some(Node::Real(p));
                }
                self.streamed_tile(folder, file).map(Node::Dsf)
            }
            ["terrain", _] => {
                if let Some(p) = self.find_real(&rel) {
                    return Some(Node::Real(p));
                }
                TerName::parse(parts[1]).map(Node::Ter)
            }
            ["textures", file] => {
                if let Some(p) = self.find_real(&rel) {
                    return Some(Node::Real(p));
                }
                if let Some(t) = TextureId::parse_dds(parts[1]) {
                    return Some(Node::Dds(t));
                }
                if *file == "water_transition.png" {
                    return Some(Node::WaterTransition);
                }
                if MaskName::parse(parts[1]).is_some() && self.mesh.as_ref().and_then(|m| m.mask(parts[1])).is_some() {
                    return Some(Node::Mask(parts[1].to_string()));
                }
                None
            }
            // Anything else only from imported packs (e.g. library.txt).
            _ => self.find_real(&rel).map(|p| if p.is_dir() { Node::Dir(rel.clone()) } else { Node::Real(p) }),
        }
    }

    fn list_dsf_folders(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        if let Some(m) = &self.mesh {
            for t in m.listed_tiles().iter() {
                set.insert(t.folder());
            }
        }
        for pack in &self.cfg.packs {
            if let Ok(rd) = std::fs::read_dir(pack.join("Earth nav data")) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        set.insert(e.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
        set.into_iter().collect()
    }

    /// Attributes of a node. May do bounded local work (raster index).
    pub fn attr(&self, node: &Node) -> Option<Attr> {
        let mtime = self.mount_time;
        let file = |size| Some(Attr { is_dir: false, size, mtime });
        match node {
            Node::Dir(_) => Some(Attr { is_dir: true, size: 0, mtime }),
            Node::Real(p) => {
                let md = std::fs::metadata(p).ok()?;
                Some(Attr { is_dir: md.is_dir(), size: md.len(), mtime: md.modified().unwrap_or(mtime) })
            }
            Node::Dsf(t) => file(self.mesh.as_ref()?.dsf_size(*t)?),
            Node::Ter(t) => file(ter_contents(t, &self.cfg.ter).len() as u64),
            Node::Dds(_) => file(dds::FILE_LEN),
            Node::Mask(n) => file(self.mesh.as_ref()?.mask(n)?.object.size),
            Node::WaterTransition => file(water_transition_png().len() as u64),
        }
    }

    /// Directory listing: (name, node). Synthesized `.ter`/`.dds` files are
    /// resolvable by name but not enumerated, except those of imported packs.
    pub fn list(&self, dir: &str) -> Vec<(String, Node)> {
        let rel = normalize(dir);
        let mut out: BTreeMap<String, Node> = BTreeMap::new();
        let lower = rel.to_ascii_lowercase();
        if lower.is_empty() {
            for d in ["Earth nav data", "terrain", "textures"] {
                out.insert(d.into(), Node::Dir(d.into()));
            }
        } else if lower == "earth nav data" {
            for f in self.list_dsf_folders() {
                out.insert(f.clone(), Node::Dir(format!("Earth nav data/{f}")));
            }
        } else if let Some(folder) = lower.strip_prefix("earth nav data/") {
            if let Some(m) = &self.mesh {
                for &t in m.listed_tiles().iter() {
                    if t.folder().eq_ignore_ascii_case(folder) {
                        out.insert(format!("{}.dsf", t.name()), Node::Dsf(t));
                    }
                }
            }
        } else if lower == "textures" {
            out.insert("water_transition.png".into(), Node::WaterTransition);
        }
        // Real files from imported packs (lower priority packs first so higher
        // priority ones overwrite).
        for pack in self.cfg.packs.iter().rev() {
            let dir_path = find_ci(pack, &rel).unwrap_or_else(|| pack.join(&rel));
            if let Ok(rd) = std::fs::read_dir(dir_path) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let path = e.path();
                    let node = if path.is_dir() {
                        Node::Dir(if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") })
                    } else {
                        Node::Real(path)
                    };
                    out.retain(|k, _| !k.eq_ignore_ascii_case(&name));
                    out.insert(name, node);
                }
            }
        }
        out.into_iter().collect()
    }

    /// Read file contents. Blocking: call from a filesystem worker thread,
    /// never from inside the tokio runtime.
    pub fn read_blocking(&self, node: &Node, off: u64, len: u64, class: ReaderClass) -> Result<Vec<u8>, String> {
        match node {
            Node::Dir(_) => Err("is a directory".into()),
            Node::Real(p) => read_file_range(p, off, len).map_err(|e| e.to_string()),
            Node::Ter(t) => Ok(slice(ter_contents(t, &self.cfg.ter).as_bytes(), off, len)),
            Node::WaterTransition => Ok(slice(water_transition_png(), off, len)),
            Node::Dsf(t) => {
                let mesh = self.mesh.clone().ok_or("no mesh")?;
                if class == ReaderClass::Other && !mesh.is_dsf_ready(*t) {
                    // Never trigger downloads for indexers/antivirus.
                    return Err("not cached".into());
                }
                let t = *t;
                self.rt.block_on(async move { mesh.read_dsf(t, off, len).await })
            }
            Node::Mask(n) => {
                let mesh = self.mesh.clone().ok_or("no mesh")?;
                let n = n.clone();
                self.rt.block_on(async move { mesh.read_mask(&n, off, len).await })
            }
            Node::Dds(t) => {
                let ctx = match class {
                    ReaderClass::XPlane => self.engine.live_ctx(),
                    ReaderClass::Other => ReadCtx { class: ReaderClass::Other, ..self.engine.live_ctx() },
                };
                let engine = self.engine.clone();
                let t = t.clone();
                self.rt.block_on(async move { engine.read(&t, off, len, ctx).await })
            }
        }
    }

    /// Hook for file opens (eager tail build for textures).
    pub fn on_open(&self, node: &Node, class: ReaderClass) {
        if let Node::Dds(t) = node {
            let _g = self.rt.enter();
            self.engine.on_open(t, class);
        }
    }

    /// Should the kernel cache this file's pages? Not while a texture has
    /// degraded parts, so the upgraded content is seen on the next open.
    pub fn cacheable(&self, node: &Node) -> bool {
        match node {
            Node::Dds(t) => !self.engine.is_degraded(t),
            _ => true,
        }
    }
}

fn strip_ext_ci<'a>(name: &'a str, ext: &str) -> Option<&'a str> {
    let dot = name.rfind('.')?;
    name[dot + 1..].eq_ignore_ascii_case(ext).then(|| &name[..dot])
}

fn slice(data: &[u8], off: u64, len: u64) -> Vec<u8> {
    let start = (off as usize).min(data.len());
    let end = (off.saturating_add(len) as usize).min(data.len());
    data[start..end].to_vec()
}

fn read_file_range(p: &Path, off: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let size = f.metadata()?.len();
    if off >= size {
        return Ok(Vec::new());
    }
    let n = len.min(size - off) as usize;
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Case-insensitive path lookup under `root`.
fn find_ci(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    if rel.is_empty() {
        return cur.exists().then_some(cur);
    }
    for comp in rel.split('/') {
        let direct = cur.join(comp);
        if direct.exists() {
            cur = direct;
            continue;
        }
        let rd = std::fs::read_dir(&cur).ok()?;
        let found = rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp))?;
        cur = found.path();
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("\\textures\\a.dds"), "textures/a.dds");
        assert_eq!(normalize("/Earth nav data/./+40-080/"), "Earth nav data/+40-080");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn water_transition_decodes() {
        let img = image::load_from_memory(water_transition_png()).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (64, 128));
        assert_eq!(img.get_pixel(0, 0).0[0], 0);
        assert_eq!(img.get_pixel(63, 64).0[0], 46);
        assert_eq!(img.get_pixel(10, 127).0[0], 253);
        for y in 1..128 {
            assert!(img.get_pixel(0, y).0[0] >= img.get_pixel(0, y - 1).0[0]);
        }
    }
}
