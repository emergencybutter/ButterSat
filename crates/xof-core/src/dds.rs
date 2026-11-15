//! Layout of the synthesized DDS files: BC1, 4096², full mip chain.
//!
//! ```text
//! [ header 128 ][ mip0 8 MiB ][ mip1 2 MiB ][ mip2 512 KiB ][ mip3 128 KiB ][ mip4 .. mip12 ]
//! ```
//! Mips 0–3 are split into *bands*: one band per row of 256-px source tiles
//! (64 BC1 block rows). Mips 4–12 (one source tile and its downsamples) form a
//! single *tail* segment. Bands and the tail are the units of work and caching.

pub const DIM: u32 = 4096;
pub const MIP_COUNT: u8 = 13;
pub const HEADER_LEN: u64 = 128;
/// Mips `0..BANDED_MIPS` are split into bands.
pub const BANDED_MIPS: u8 = 4;
/// Total size of a synthesized texture file.
pub const FILE_LEN: u64 = 11_184_952;

/// Edge length of mip `level`.
pub const fn mip_dim(level: u8) -> u32 {
    let d = DIM >> level;
    if d == 0 {
        1
    } else {
        d
    }
}

/// Byte length of BC1 mip `level`.
pub const fn mip_len(level: u8) -> u64 {
    let blocks = {
        let b = mip_dim(level) / 4;
        if b == 0 {
            1
        } else {
            b
        }
    } as u64;
    blocks * blocks * 8
}

/// File offset of mip `level`.
pub const fn mip_offset(level: u8) -> u64 {
    let mut off = HEADER_LEN;
    let mut l = 0;
    while l < level {
        off += mip_len(l);
        l += 1;
    }
    off
}

/// Number of bands in banded mip `level`.
pub const fn bands_in_mip(level: u8) -> u32 {
    16 >> level
}

/// Byte length of one band of banded mip `level` (64 block rows).
pub const fn band_len(level: u8) -> u64 {
    // (W/4 blocks per row) * 8 bytes * 64 block rows
    (mip_dim(level) as u64 / 4) * 8 * 64
}

pub const TAIL_OFFSET: u64 = mip_offset(BANDED_MIPS);
pub const TAIL_LEN: u64 = FILE_LEN - TAIL_OFFSET;

/// A unit of a DDS file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Segment {
    Header,
    /// One band of a banded mip.
    Band { mip: u8, band: u8 },
    /// Mips 4..=12.
    Tail,
}

impl Segment {
    pub fn offset(self) -> u64 {
        match self {
            Segment::Header => 0,
            Segment::Band { mip, band } => mip_offset(mip) + band as u64 * band_len(mip),
            Segment::Tail => TAIL_OFFSET,
        }
    }

    pub fn len(self) -> u64 {
        match self {
            Segment::Header => HEADER_LEN,
            Segment::Band { mip, .. } => band_len(mip),
            Segment::Tail => TAIL_LEN,
        }
    }

    /// All segments in file order.
    pub fn all() -> impl Iterator<Item = Segment> {
        std::iter::once(Segment::Header)
            .chain((0..BANDED_MIPS).flat_map(|mip| {
                (0..bands_in_mip(mip) as u8).map(move |band| Segment::Band { mip, band })
            }))
            .chain(std::iter::once(Segment::Tail))
    }

    /// The segment containing file offset `off` (`off < FILE_LEN`).
    pub fn at(off: u64) -> Segment {
        debug_assert!(off < FILE_LEN);
        if off < HEADER_LEN {
            return Segment::Header;
        }
        if off >= TAIL_OFFSET {
            return Segment::Tail;
        }
        let mut mip = 0;
        while off >= mip_offset(mip + 1) {
            mip += 1;
        }
        let band = ((off - mip_offset(mip)) / band_len(mip)) as u8;
        Segment::Band { mip, band }
    }
}

/// A piece of a read request: copy `len` bytes from `segment` starting at
/// `seg_off` into the output at `out_off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub segment: Segment,
    pub seg_off: u64,
    pub out_off: u64,
    pub len: u64,
}

/// Split the read `[off, off+len)` (clamped to the file) into segment pieces.
pub fn pieces(off: u64, len: u64) -> Vec<Piece> {
    let end = (off.saturating_add(len)).min(FILE_LEN);
    let mut out = Vec::new();
    let mut pos = off;
    while pos < end {
        let seg = Segment::at(pos);
        let seg_end = seg.offset() + seg.len();
        let take = seg_end.min(end) - pos;
        out.push(Piece { segment: seg, seg_off: pos - seg.offset(), out_off: pos - off, len: take });
        pos += take;
    }
    out
}

/// The 128-byte DDS header (`DDS ` magic + DDS_HEADER) for our layout.
pub fn header() -> [u8; HEADER_LEN as usize] {
    const DDSD_CAPS: u32 = 0x1;
    const DDSD_HEIGHT: u32 = 0x2;
    const DDSD_WIDTH: u32 = 0x4;
    const DDSD_PIXELFORMAT: u32 = 0x1000;
    const DDSD_MIPMAPCOUNT: u32 = 0x2_0000;
    const DDSD_LINEARSIZE: u32 = 0x8_0000;
    const DDPF_FOURCC: u32 = 0x4;
    const DDSCAPS_COMPLEX: u32 = 0x8;
    const DDSCAPS_TEXTURE: u32 = 0x1000;
    const DDSCAPS_MIPMAP: u32 = 0x40_0000;

    let mut h = [0u8; HEADER_LEN as usize];
    let mut put = |idx: usize, v: u32| h[idx..idx + 4].copy_from_slice(&v.to_le_bytes());
    put(0, u32::from_le_bytes(*b"DDS "));
    put(4, 124); // dwSize
    put(
        8,
        DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PIXELFORMAT | DDSD_MIPMAPCOUNT | DDSD_LINEARSIZE,
    );
    put(12, DIM); // height
    put(16, DIM); // width
    put(20, mip_len(0) as u32); // linear size of the top level
    put(24, 0); // depth
    put(28, MIP_COUNT as u32);
    // 11 reserved dwords at 32..76
    put(76, 32); // ddspf.dwSize
    put(80, DDPF_FOURCC);
    put(84, u32::from_le_bytes(*b"DXT1"));
    // rgb bit count + masks stay 0 (88..108)
    put(108, DDSCAPS_COMPLEX | DDSCAPS_TEXTURE | DDSCAPS_MIPMAP);
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn sizes() {
        assert_eq!(mip_len(0), 8_388_608);
        assert_eq!(mip_len(4), 32_768);
        assert_eq!(mip_len(11), 8); // 2x2
        assert_eq!(mip_len(12), 8); // 1x1
        assert_eq!(mip_offset(MIP_COUNT), FILE_LEN);
        assert_eq!(band_len(0), 512 * 1024);
        assert_eq!(band_len(0) * 16, mip_len(0));
        assert_eq!(band_len(3) * 2, mip_len(3));
        assert_eq!(TAIL_LEN, 32_768 + 10_936);
    }

    #[test]
    fn segments_tile_the_file() {
        let mut pos = 0;
        for s in Segment::all() {
            assert_eq!(s.offset(), pos, "{s:?}");
            assert_eq!(Segment::at(pos), s);
            assert_eq!(Segment::at(pos + s.len() - 1), s);
            pos += s.len();
        }
        assert_eq!(pos, FILE_LEN);
        assert_eq!(Segment::all().count(), 1 + 16 + 8 + 4 + 2 + 1);
    }

    #[test]
    fn header_layout() {
        let h = header();
        assert_eq!(&h[0..4], b"DDS ");
        assert_eq!(&h[84..88], b"DXT1");
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 13);
    }

    proptest! {
        #[test]
        fn pieces_cover_request(off in 0u64..FILE_LEN + 10, len in 0u64..3_000_000) {
            let ps = pieces(off, len);
            let expect = FILE_LEN.saturating_sub(off).min(len);
            prop_assert_eq!(ps.iter().map(|p| p.len).sum::<u64>(), expect);
            let mut out = 0;
            for p in &ps {
                prop_assert_eq!(p.out_off, out);
                prop_assert!(p.seg_off + p.len <= p.segment.len());
                prop_assert_eq!(p.segment.offset() + p.seg_off, off + p.out_off);
                out += p.len;
            }
        }
    }
}
