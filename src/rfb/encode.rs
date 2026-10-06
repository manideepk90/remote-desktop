//! Pixel formats and framebuffer encodings (Raw and ZRLE).

use flate2::{Compress, Compression, FlushCompress};

use crate::frame::Frame;

/// An RFB pixel format. The server's native format is 32bpp little-endian
/// 0x00RRGGBB, which matches the captured BGRX bytes exactly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelFormat {
    pub bpp: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub max: [u16; 3],
    pub shift: [u8; 3],
}

impl Default for PixelFormat {
    fn default() -> Self {
        PixelFormat { bpp: 32, depth: 24, big_endian: false, true_color: true, max: [255; 3], shift: [16, 8, 0] }
    }
}

impl PixelFormat {
    pub fn parse(b: &[u8; 16]) -> PixelFormat {
        let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        PixelFormat {
            bpp: b[0],
            depth: b[1],
            big_endian: b[2] != 0,
            true_color: b[3] != 0,
            max: [u16_at(4), u16_at(6), u16_at(8)],
            shift: [b[10], b[11], b[12]],
        }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend([self.bpp, self.depth, self.big_endian as u8, self.true_color as u8]);
        for m in self.max {
            out.extend(m.to_be_bytes());
        }
        out.extend(self.shift);
        out.extend([0; 3]);
    }

    pub fn is_supported(&self) -> bool {
        self.true_color && matches!(self.bpp, 8 | 16 | 32) && self.max.iter().all(|&m| m > 0)
    }

    fn is_native(&self) -> bool {
        *self == PixelFormat::default() || (self.bpp == 32 && !self.big_endian && self.true_color && self.max == [255; 3] && self.shift == [16, 8, 0])
    }

    fn bytes(&self) -> usize {
        self.bpp as usize / 8
    }

    /// Converts a source pixel (0x00RRGGBB) to this format's pixel value.
    #[inline]
    fn pack(&self, src: u32) -> u32 {
        let c = [(src >> 16) & 0xff, (src >> 8) & 0xff, src & 0xff];
        let mut v = 0;
        for i in 0..3 {
            let m = self.max[i] as u32;
            v |= ((c[i] * m + 127) / 255) << self.shift[i];
        }
        v
    }

    #[inline]
    fn put(&self, v: u32, out: &mut Vec<u8>) {
        match (self.bpp, self.big_endian) {
            (8, _) => out.push(v as u8),
            (16, false) => out.extend((v as u16).to_le_bytes()),
            (16, true) => out.extend((v as u16).to_be_bytes()),
            (_, false) => out.extend(v.to_le_bytes()),
            (_, true) => out.extend(v.to_be_bytes()),
        }
    }

    /// ZRLE "compressed pixel": 32bpp pixels whose colour bits fit in three bytes
    /// are sent as three bytes. Returns the index of the dropped byte, if any.
    fn cpixel_drop(&self) -> Option<usize> {
        if self.bpp != 32 || self.depth > 24 || !self.true_color {
            return None;
        }
        let mask = (0..3).fold(0u32, |m, i| m | ((self.max[i] as u32) << self.shift[i]));
        if mask & 0xff00_0000 == 0 {
            Some(if self.big_endian { 0 } else { 3 })
        } else if mask & 0x0000_00ff == 0 {
            Some(if self.big_endian { 3 } else { 0 })
        } else {
            None
        }
    }
}

#[inline]
fn src_pixel(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], 0])
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Per-connection encoder state (the ZRLE zlib stream must persist).
pub struct Encoder {
    pub pf: PixelFormat,
    zlib: Compress,
    scratch: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder { pf: PixelFormat::default(), zlib: Compress::new(Compression::new(1), true), scratch: Vec::new() }
    }

    pub fn raw(&self, f: &Frame, r: Rect, out: &mut Vec<u8>) {
        let pf = self.pf;
        out.reserve(r.w as usize * r.h as usize * pf.bytes());
        for y in r.y..r.y + r.h {
            let row = f.row(y, r.x, r.w);
            if pf.is_native() {
                out.extend_from_slice(row);
            } else {
                for p in row.chunks_exact(4) {
                    pf.put(pf.pack(src_pixel(p)), out);
                }
            }
        }
    }

    /// Writes a ZRLE rectangle body (length-prefixed zlib data).
    pub fn zrle(&mut self, f: &Frame, r: Rect, out: &mut Vec<u8>) {
        let pf = self.pf;
        let drop = pf.cpixel_drop();
        let cp = if drop.is_some() { 3 } else { pf.bytes() };
        let put_c = |v: u32, buf: &mut Vec<u8>| {
            let at = buf.len();
            pf.put(pf.pack(v), buf);
            if let Some(i) = drop {
                buf.remove(at + i);
            }
        };

        let mut raw = std::mem::take(&mut self.scratch);
        raw.clear();
        let mut tile: Vec<u32> = Vec::with_capacity(64 * 64);
        let mut palette: Vec<u32> = Vec::with_capacity(128);
        let mut ty = r.y;
        while ty < r.y + r.h {
            let th = 64.min(r.y + r.h - ty);
            let mut tx = r.x;
            while tx < r.x + r.w {
                let tw = 64.min(r.x + r.w - tx);
                tile.clear();
                for y in ty..ty + th {
                    tile.extend(f.row(y, tx, tw).chunks_exact(4).map(src_pixel));
                }
                encode_tile(&tile, tw as usize, th as usize, cp, &mut palette, &mut raw, &put_c);
                tx += tw;
            }
            ty += th;
        }

        let mut compressed = Vec::with_capacity(raw.len() / 2 + 64);
        let mut input = &raw[..];
        loop {
            compressed.reserve(input.len() / 2 + 4096);
            let before = self.zlib.total_in();
            self.zlib.compress_vec(input, &mut compressed, FlushCompress::Sync).expect("zlib");
            input = &input[(self.zlib.total_in() - before) as usize..];
            if input.is_empty() && compressed.len() < compressed.capacity() {
                break;
            }
        }
        out.extend((compressed.len() as u32).to_be_bytes());
        out.extend_from_slice(&compressed);
        self.scratch = raw;
    }
}

fn run_len_bytes(len: usize) -> usize {
    (len - 1) / 255 + 1
}

fn put_run_len(len: usize, out: &mut Vec<u8>) {
    let mut n = len - 1;
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// Picks the smallest ZRLE sub-encoding for one tile.
fn encode_tile(
    tile: &[u32],
    w: usize,
    h: usize,
    cp: usize,
    palette: &mut Vec<u32>,
    out: &mut Vec<u8>,
    put_c: &dyn Fn(u32, &mut Vec<u8>),
) {
    palette.clear();
    let (mut runs_plain, mut runs_pal) = (0usize, 0usize);
    let mut last_idx = 0usize;
    let mut i = 0;
    while i < tile.len() {
        let c = tile[i];
        let mut j = i + 1;
        while j < tile.len() && tile[j] == c {
            j += 1;
        }
        let len = j - i;
        runs_plain += cp + run_len_bytes(len);
        runs_pal += 1 + if len > 1 { run_len_bytes(len) } else { 0 };
        if palette.len() <= 127 && !(palette.get(last_idx) == Some(&c)) {
            match palette.iter().position(|&p| p == c) {
                Some(k) => last_idx = k,
                None => {
                    palette.push(c);
                    last_idx = palette.len() - 1;
                }
            }
        }
        i = j;
    }

    let n = palette.len();
    if n == 1 {
        out.push(1);
        put_c(palette[0], out);
        return;
    }
    let raw_size = tile.len() * cp;
    let packed_bits = match n {
        2 => 1,
        3..=4 => 2,
        5..=16 => 4,
        _ => 0,
    };
    let packed_size = if packed_bits > 0 { n * cp + (w * packed_bits).div_ceil(8) * h } else { usize::MAX };
    let pal_rle_size = if n <= 127 { n * cp + runs_pal } else { usize::MAX };
    let best = raw_size.min(packed_size).min(pal_rle_size).min(runs_plain);

    let index_of = |c: u32| palette.iter().position(|&p| p == c).unwrap() as u8;
    if best == raw_size {
        out.push(0);
        for &c in tile {
            put_c(c, out);
        }
    } else if best == packed_size {
        out.push(n as u8);
        for &c in palette.iter() {
            put_c(c, out);
        }
        let mut cache = (u32::MAX, 0u8);
        for row in tile.chunks_exact(w) {
            let (mut byte, mut nbits) = (0u8, 0);
            for &c in row {
                if cache.0 != c {
                    cache = (c, index_of(c));
                }
                byte = (byte << packed_bits) | cache.1;
                nbits += packed_bits;
                if nbits == 8 {
                    out.push(byte);
                    (byte, nbits) = (0, 0);
                }
            }
            if nbits > 0 {
                out.push(byte << (8 - nbits));
            }
        }
    } else if best == pal_rle_size {
        out.push(128 + n as u8);
        for &c in palette.iter() {
            put_c(c, out);
        }
        for_each_run(tile, |c, len| {
            let idx = index_of(c);
            if len == 1 {
                out.push(idx);
            } else {
                out.push(idx | 128);
                put_run_len(len, out);
            }
        });
    } else {
        out.push(128);
        for_each_run(tile, |c, len| {
            put_c(c, out);
            put_run_len(len, out);
        });
    }
}

fn for_each_run(tile: &[u32], mut f: impl FnMut(u32, usize)) {
    let mut i = 0;
    while i < tile.len() {
        let c = tile[i];
        let mut j = i + 1;
        while j < tile.len() && tile[j] == c {
            j += 1;
        }
        f(c, j - i);
        i = j;
    }
}

/// Returns rectangles (in 64px tiles, merged horizontally) that differ between frames.
pub fn dirty_rects(old: &Frame, new: &Frame) -> Vec<Rect> {
    const T: u32 = 64;
    let mut rects = Vec::new();
    let mut ty = 0;
    while ty < new.height {
        let th = T.min(new.height - ty);
        let mut run: Option<Rect> = None;
        let mut tx = 0;
        while tx < new.width {
            let tw = T.min(new.width - tx);
            let changed = (ty..ty + th).any(|y| old.row(y, tx, tw) != new.row(y, tx, tw));
            match (&mut run, changed) {
                (Some(r), true) => r.w += tw,
                (None, true) => run = Some(Rect { x: tx, y: ty, w: tw, h: th }),
                (Some(_), false) => rects.push(run.take().unwrap()),
                (None, false) => {}
            }
            tx += tw;
        }
        rects.extend(run);
        ty += th;
    }
    // Merge vertically adjacent rows that span the same columns.
    let mut merged: Vec<Rect> = Vec::with_capacity(rects.len());
    for r in rects {
        if let Some(m) = merged.iter_mut().rev().find(|m| m.x == r.x && m.w == r.w && m.y + m.h == r.y) {
            m.h += r.h;
        } else {
            merged.push(r);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, f: impl Fn(u32, u32) -> u32) -> Frame {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend(f(x, y).to_le_bytes());
            }
        }
        Frame { width: w, height: h, data, seq: 1 }
    }

    #[test]
    fn cpixel_layout() {
        assert_eq!(PixelFormat::default().cpixel_drop(), Some(3));
        let be = PixelFormat { big_endian: true, ..Default::default() };
        assert_eq!(be.cpixel_drop(), Some(0));
        let hi = PixelFormat { shift: [24, 16, 8], ..Default::default() };
        assert_eq!(hi.cpixel_drop(), Some(0));
    }

    #[test]
    fn pack_16bpp() {
        let pf = PixelFormat { bpp: 16, depth: 16, max: [31, 63, 31], shift: [11, 5, 0], ..Default::default() };
        assert_eq!(pf.pack(0xffffff), 0xffff);
        assert_eq!(pf.pack(0xff0000), 0xf800);
    }

    #[test]
    fn dirty_detection_merges() {
        let a = frame(200, 200, |_, _| 0);
        let b = frame(200, 200, |x, y| if x < 100 && y < 100 { 0xff } else { 0 });
        let r = dirty_rects(&a, &b);
        assert_eq!(r, vec![Rect { x: 0, y: 0, w: 128, h: 128 }]);
        assert!(dirty_rects(&a, &a).is_empty());
    }

    /// Decodes our ZRLE output with an independent minimal decoder.
    #[test]
    fn zrle_roundtrip() {
        let f = frame(150, 70, |x, y| match (x / 10 + y / 7) % 4 {
            0 => 0x112233,
            1 => 0x445566,
            2 => (x * 7 + y * 13) & 0xffffff,
            _ => 0xabcdef,
        });
        let mut enc = Encoder::new();
        let mut out = Vec::new();
        let r = Rect { x: 0, y: 0, w: 150, h: 70 };
        enc.zrle(&f, r, &mut out);
        let len = u32::from_be_bytes(out[..4].try_into().unwrap()) as usize;
        let mut d = flate2::Decompress::new(true);
        let mut data = Vec::with_capacity(1 << 20);
        d.decompress_vec(&out[4..4 + len], &mut data, flate2::FlushDecompress::Sync).unwrap();
        let decoded = decode_zrle(&data, 150, 70);
        let expect: Vec<u32> = f.data.chunks_exact(4).map(src_pixel).collect();
        assert_eq!(decoded, expect);
    }

    fn decode_zrle(mut d: &[u8], w: usize, h: usize) -> Vec<u32> {
        let mut px = vec![0u32; w * h];
        let cpix = |d: &mut &[u8]| {
            let v = u32::from_le_bytes([d[0], d[1], d[2], 0]);
            *d = &d[3..];
            v
        };
        let rl = |d: &mut &[u8]| {
            let mut n = 1;
            loop {
                let b = d[0];
                *d = &d[1..];
                n += b as usize;
                if b != 255 {
                    return n;
                }
            }
        };
        for ty in (0..h).step_by(64) {
            for tx in (0..w).step_by(64) {
                let (tw, th) = (64.min(w - tx), 64.min(h - ty));
                let mut tile = Vec::with_capacity(tw * th);
                let sub = d[0];
                d = &d[1..];
                match sub {
                    0 => (0..tw * th).for_each(|_| tile.push(cpix(&mut d))),
                    1 => tile.resize(tw * th, cpix(&mut d)),
                    2..=16 => {
                        let pal: Vec<u32> = (0..sub).map(|_| cpix(&mut d)).collect();
                        let bits = match sub { 2 => 1, 3..=4 => 2, _ => 4 };
                        for _ in 0..th {
                            let nbytes = (tw * bits).div_ceil(8);
                            let row = &d[..nbytes];
                            for x in 0..tw {
                                let bit = x * bits;
                                let v = (row[bit / 8] >> (8 - bits - bit % 8)) & ((1 << bits) - 1);
                                tile.push(pal[v as usize]);
                            }
                            d = &d[nbytes..];
                        }
                    }
                    128 => while tile.len() < tw * th {
                        let c = cpix(&mut d);
                        let n = rl(&mut d);
                        tile.extend(std::iter::repeat_n(c, n));
                    },
                    130..=255 => {
                        let pal: Vec<u32> = (0..sub - 128).map(|_| cpix(&mut d)).collect();
                        while tile.len() < tw * th {
                            let i = d[0];
                            d = &d[1..];
                            let n = if i & 128 != 0 { rl(&mut d) } else { 1 };
                            tile.extend(std::iter::repeat_n(pal[(i & 127) as usize], n));
                        }
                    }
                    s => panic!("bad subencoding {s}"),
                }
                for y in 0..th {
                    px[(ty + y) * w + tx..(ty + y) * w + tx + tw].copy_from_slice(&tile[y * tw..(y + 1) * tw]);
                }
            }
        }
        px
    }
}
