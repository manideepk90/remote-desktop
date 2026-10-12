//! Rectangle decoders: Raw, RRE, Hextile and ZRLE, plus the cursor pseudo-encoding.
//!
//! The client always asks for 32bpp little-endian true colour with red in the
//! lowest byte, so a pixel arrives as `[r, g, b, x]` and a ZRLE "compressed
//! pixel" as `[r, g, b]`.

use std::io::{self, Read};

use egui::Color32;
use flate2::{Decompress, FlushDecompress, Status};

use super::framebuffer::{Cursor, Rect};

/// The pixel format sent in SetPixelFormat (see the module docs).
pub const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0];

/// Largest zlib payload or clipboard text accepted from a server.
pub const MAX_PAYLOAD: usize = 64 << 20;

pub fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub trait ReadExt: Read {
    fn u8(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact(&mut b)?;
        Ok(b[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        let mut b = [0u8; 2];
        self.read_exact(&mut b)?;
        Ok(u16::from_be_bytes(b))
    }
    fn u32(&mut self) -> io::Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn read_vec(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.read_exact(&mut v)?;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> io::Result<()> {
        io::copy(&mut self.take(n as u64), &mut io::sink())
            .and_then(|got| if got as usize == n { Ok(()) } else { Err(io::ErrorKind::UnexpectedEof.into()) })
    }
    fn pixel(&mut self) -> io::Result<Color32> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(Color32::from_rgb(b[0], b[1], b[2]))
    }
    /// A length-prefixed string (DesktopName, failure reasons). Invalid UTF-8 is replaced.
    fn string(&mut self, max: usize) -> io::Result<String> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(invalid(format!("string of {len} bytes is too long")));
        }
        Ok(String::from_utf8_lossy(&self.read_vec(len)?).into_owned())
    }
}

impl<R: Read + ?Sized> ReadExt for R {}

fn convert(raw: &[u8]) -> Vec<Color32> {
    raw.as_chunks::<4>().0.iter().map(|p| Color32::from_rgb(p[0], p[1], p[2])).collect()
}

pub fn raw(r: &mut impl Read, rect: Rect) -> io::Result<Vec<Color32>> {
    Ok(convert(&r.read_vec(rect.w * rect.h * 4)?))
}

fn fill(buf: &mut [Color32], stride: usize, x: usize, y: usize, w: usize, h: usize, c: Color32) {
    for row in y..y + h {
        buf[row * stride + x..row * stride + x + w].fill(c);
    }
}

pub fn rre(r: &mut impl Read, rect: Rect) -> io::Result<Vec<Color32>> {
    let n = r.u32()?;
    let bg = r.pixel()?;
    let mut buf = vec![bg; rect.w * rect.h];
    for _ in 0..n {
        let c = r.pixel()?;
        let (x, y, w, h) = (r.u16()? as usize, r.u16()? as usize, r.u16()? as usize, r.u16()? as usize);
        if x + w > rect.w || y + h > rect.h {
            return Err(invalid("RRE subrectangle out of bounds"));
        }
        fill(&mut buf, rect.w, x, y, w, h, c);
    }
    Ok(buf)
}

pub fn hextile(r: &mut impl Read, rect: Rect) -> io::Result<Vec<Color32>> {
    const RAW: u8 = 1;
    const BACKGROUND: u8 = 2;
    const FOREGROUND: u8 = 4;
    const ANY_SUBRECTS: u8 = 8;
    const COLOURED: u8 = 16;

    let mut buf = vec![Color32::BLACK; rect.w * rect.h];
    let (mut bg, mut fg) = (Color32::BLACK, Color32::WHITE);
    for ty in (0..rect.h).step_by(16) {
        for tx in (0..rect.w).step_by(16) {
            let (tw, th) = ((rect.w - tx).min(16), (rect.h - ty).min(16));
            let flags = r.u8()?;
            if flags & RAW != 0 {
                let px = raw(r, Rect::new(0, 0, tw, th))?;
                for row in 0..th {
                    let at = (ty + row) * rect.w + tx;
                    buf[at..at + tw].copy_from_slice(&px[row * tw..(row + 1) * tw]);
                }
                continue;
            }
            if flags & BACKGROUND != 0 {
                bg = r.pixel()?;
            }
            fill(&mut buf, rect.w, tx, ty, tw, th, bg);
            if flags & FOREGROUND != 0 {
                fg = r.pixel()?;
            }
            if flags & ANY_SUBRECTS == 0 {
                continue;
            }
            for _ in 0..r.u8()? {
                let c = if flags & COLOURED != 0 { r.pixel()? } else { fg };
                let (xy, wh) = (r.u8()?, r.u8()?);
                let (sx, sy) = ((xy >> 4) as usize, (xy & 15) as usize);
                let (sw, sh) = ((wh >> 4) as usize + 1, (wh & 15) as usize + 1);
                if sx + sw > tw || sy + sh > th {
                    return Err(invalid("Hextile subrectangle out of bounds"));
                }
                fill(&mut buf, rect.w, tx + sx, ty + sy, sw, sh, c);
            }
        }
    }
    Ok(buf)
}

/// Cursor pseudo-encoding: `w*h` pixels followed by a 1bpp transparency mask.
pub fn cursor(r: &mut impl Read, rect: Rect) -> io::Result<Cursor> {
    let pixels = raw(r, rect)?;
    let row_bytes = rect.w.div_ceil(8);
    let mask = r.read_vec(row_bytes * rect.h)?;
    let pixels = pixels
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let (x, y) = (i % rect.w, i / rect.w);
            if mask[y * row_bytes + x / 8] & (0x80 >> (x % 8)) != 0 { c } else { Color32::TRANSPARENT }
        })
        .collect();
    Ok(Cursor { width: rect.w, height: rect.h, hotspot: (rect.x, rect.y), pixels })
}

/// Reads bytes out of an inflated ZRLE payload.
struct Bytes<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let out = self.buf.get(self.pos..self.pos + n).ok_or_else(|| invalid("ZRLE data ended early"))?;
        self.pos += n;
        Ok(out)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn cpixel(&mut self) -> io::Result<Color32> {
        let p = self.take(3)?;
        Ok(Color32::from_rgb(p[0], p[1], p[2]))
    }
    fn run_length(&mut self) -> io::Result<usize> {
        let mut len = 1;
        loop {
            let b = self.u8()?;
            len += b as usize;
            if b != 255 {
                return Ok(len);
            }
        }
    }
}

/// ZRLE keeps one zlib stream for the whole connection.
pub struct Zrle {
    zlib: Decompress,
    inflated: Vec<u8>,
}

impl Default for Zrle {
    fn default() -> Self {
        Zrle { zlib: Decompress::new(true), inflated: Vec::new() }
    }
}

impl Zrle {
    pub fn decode(&mut self, r: &mut impl Read, rect: Rect) -> io::Result<Vec<Color32>> {
        let len = r.u32()? as usize;
        if len > MAX_PAYLOAD {
            return Err(invalid("ZRLE rectangle too large"));
        }
        let compressed = r.read_vec(len)?;
        self.inflate(&compressed, rect)?;
        let mut data = Bytes { buf: &self.inflated, pos: 0 };
        let mut buf = vec![Color32::BLACK; rect.w * rect.h];
        for ty in (0..rect.h).step_by(64) {
            for tx in (0..rect.w).step_by(64) {
                let (tw, th) = ((rect.w - tx).min(64), (rect.h - ty).min(64));
                tile(&mut data, &mut buf, rect.w, tx, ty, tw, th)?;
            }
        }
        Ok(buf)
    }

    fn inflate(&mut self, input: &[u8], rect: Rect) -> io::Result<()> {
        // Worst case is every tile sent raw: one subencoding byte plus 3 bytes a pixel.
        let limit = rect.w * rect.h * 3 + rect.w.div_ceil(64) * rect.h.div_ceil(64) + 1024;
        self.inflated.clear();
        let mut used = 0;
        loop {
            if self.inflated.capacity() - self.inflated.len() < 32 * 1024 {
                self.inflated.reserve(256 * 1024);
            }
            let spare = self.inflated.capacity() - self.inflated.len();
            let (in_before, out_before) = (self.zlib.total_in(), self.zlib.total_out());
            let status = self
                .zlib
                .decompress_vec(&input[used..], &mut self.inflated, FlushDecompress::Sync)
                .map_err(|e| invalid(format!("ZRLE: {e}")))?;
            used += (self.zlib.total_in() - in_before) as usize;
            let produced = (self.zlib.total_out() - out_before) as usize;
            if self.inflated.len() > limit {
                return Err(invalid("ZRLE rectangle inflates past its size"));
            }
            let output_full = produced == spare;
            if status == Status::StreamEnd || (used == input.len() && !output_full) {
                return Ok(());
            }
            if produced == 0 && !output_full && used < input.len() && self.zlib.total_in() == in_before {
                return Err(invalid("ZRLE stream stalled"));
            }
        }
    }
}

fn tile(
    d: &mut Bytes,
    buf: &mut [Color32],
    stride: usize,
    tx: usize,
    ty: usize,
    tw: usize,
    th: usize,
) -> io::Result<()> {
    let at = |x: usize, y: usize| (ty + y) * stride + tx + x;
    let sub = d.u8()?;
    match sub {
        0 => {
            for y in 0..th {
                for x in 0..tw {
                    buf[at(x, y)] = d.cpixel()?;
                }
            }
        }
        1 => {
            let c = d.cpixel()?;
            fill(buf, stride, tx, ty, tw, th, c);
        }
        2..=16 => {
            let palette = (0..sub).map(|_| d.cpixel()).collect::<io::Result<Vec<_>>>()?;
            let bits = match sub {
                2 => 1,
                3..=4 => 2,
                _ => 4,
            };
            let row_bytes = (tw * bits).div_ceil(8);
            for y in 0..th {
                let row = d.take(row_bytes)?;
                for x in 0..tw {
                    let bit = x * bits;
                    let idx = (row[bit / 8] >> (8 - bits - bit % 8)) & ((1 << bits) - 1);
                    buf[at(x, y)] = *palette.get(idx as usize).ok_or_else(|| invalid("ZRLE palette index"))?;
                }
            }
        }
        128 => {
            let mut i = 0;
            while i < tw * th {
                let c = d.cpixel()?;
                let n = d.run_length()?;
                if i + n > tw * th {
                    return Err(invalid("ZRLE run too long"));
                }
                for p in i..i + n {
                    buf[at(p % tw, p / tw)] = c;
                }
                i += n;
            }
        }
        130..=255 => {
            let palette = (0..sub - 128).map(|_| d.cpixel()).collect::<io::Result<Vec<_>>>()?;
            let mut i = 0;
            while i < tw * th {
                let b = d.u8()?;
                let c = *palette.get((b & 127) as usize).ok_or_else(|| invalid("ZRLE palette index"))?;
                let n = if b & 128 != 0 { d.run_length()? } else { 1 };
                if i + n > tw * th {
                    return Err(invalid("ZRLE run too long"));
                }
                for p in i..i + n {
                    buf[at(p % tw, p / tw)] = c;
                }
                i += n;
            }
        }
        _ => return Err(invalid(format!("ZRLE subencoding {sub}"))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    const RED: Color32 = Color32::from_rgb(255, 0, 0);
    const GREEN: Color32 = Color32::from_rgb(0, 255, 0);
    const BLUE: Color32 = Color32::from_rgb(0, 0, 255);

    fn px(c: Color32) -> [u8; 4] {
        [c.r(), c.g(), c.b(), 0]
    }
    fn cpx(c: Color32) -> [u8; 3] {
        [c.r(), c.g(), c.b()]
    }

    /// Compresses one rectangle's tile data the way a server does: one shared stream, sync-flushed.
    fn zrle_rect(z: &mut Compress, tiles: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(tiles.len() + 1024);
        z.compress_vec(tiles, &mut out, FlushCompress::Sync).unwrap();
        let mut msg = (out.len() as u32).to_be_bytes().to_vec();
        msg.extend(out);
        msg
    }

    #[test]
    fn raw_pixels_are_rgbx() {
        let data = [px(RED), px(GREEN)].concat();
        assert_eq!(raw(&mut &data[..], Rect::new(0, 0, 2, 1)).unwrap(), vec![RED, GREEN]);
    }

    #[test]
    fn rre_fills_subrects() {
        let mut d = 1u32.to_be_bytes().to_vec();
        d.extend(px(BLUE));
        d.extend(px(RED));
        for v in [1u16, 0, 1, 2] {
            d.extend(v.to_be_bytes());
        }
        let out = rre(&mut &d[..], Rect::new(0, 0, 3, 2)).unwrap();
        assert_eq!(out, vec![BLUE, RED, BLUE, BLUE, RED, BLUE]);
    }

    #[test]
    fn hextile_background_subrects_and_raw() {
        // 20x2: tile 1 (16x2) solid blue with one green subrect, tile 2 (4x2) raw red.
        let mut d = vec![2 | 4 | 8];
        d.extend(px(BLUE));
        d.extend(px(GREEN));
        d.push(1);
        d.push((3 << 4) | 1); // x=3, y=1
        d.push(1 << 4); // w=2, h=1
        d.push(1);
        for _ in 0..8 {
            d.extend(px(RED));
        }
        let out = hextile(&mut &d[..], Rect::new(0, 0, 20, 2)).unwrap();
        assert_eq!(out[0], BLUE);
        assert_eq!(out[20 + 3], GREEN);
        assert_eq!(out[20 + 4], GREEN);
        assert_eq!(out[20 + 5], BLUE);
        assert_eq!(out[16], RED);
        assert_eq!(out[39], RED);
    }

    #[test]
    fn zrle_all_subencodings_across_rects() {
        let mut z = Compress::new(Compression::default(), true);
        let mut dec = Zrle::default();

        // Rect 1: 66x2 -> tile A (64x2) solid red, tile B (2x2) raw.
        let mut t = vec![1];
        t.extend(cpx(RED));
        t.push(0);
        for c in [RED, GREEN, BLUE, RED] {
            t.extend(cpx(c));
        }
        let msg = zrle_rect(&mut z, &t);
        let out = dec.decode(&mut &msg[..], Rect::new(0, 0, 66, 2)).unwrap();
        assert!(out[..64].iter().all(|&c| c == RED));
        assert_eq!(&out[64..66], &[RED, GREEN]);
        assert_eq!(&out[130..132], &[BLUE, RED]);

        // Rect 2 (same zlib stream): 3x2 packed palette of 2 colours, 1 bit each.
        let mut t = vec![2];
        t.extend(cpx(GREEN));
        t.extend(cpx(BLUE));
        t.push(0b0100_0000); // row 0: G B G
        t.push(0b1010_0000); // row 1: B G B
        let msg = zrle_rect(&mut z, &t);
        let out = dec.decode(&mut &msg[..], Rect::new(0, 0, 3, 2)).unwrap();
        assert_eq!(out, vec![GREEN, BLUE, GREEN, BLUE, GREEN, BLUE]);

        // Rect 3: 5x1 plain RLE: 3 red then 2 blue.
        let mut t = vec![128];
        t.extend(cpx(RED));
        t.push(2);
        t.extend(cpx(BLUE));
        t.push(1);
        let msg = zrle_rect(&mut z, &t);
        let out = dec.decode(&mut &msg[..], Rect::new(0, 0, 5, 1)).unwrap();
        assert_eq!(out, vec![RED, RED, RED, BLUE, BLUE]);

        // Rect 4: 300x5 is five tiles across. Tile 1 is palette RLE: one green pixel,
        // then a 319-long blue run (1 + 255 + 63, so the length needs a 255 byte).
        let mut tiles = vec![130];
        tiles.extend(cpx(GREEN));
        tiles.extend(cpx(BLUE));
        tiles.push(0);
        tiles.push(128 | 1);
        tiles.extend([255, 63]);
        for _ in 0..3 {
            tiles.push(1);
            tiles.extend(cpx(BLUE));
        }
        tiles.push(1);
        tiles.extend(cpx(RED)); // the last tile is 44 wide
        let msg = zrle_rect(&mut z, &tiles);
        let out = dec.decode(&mut &msg[..], Rect::new(0, 0, 300, 5)).unwrap();
        assert_eq!(out[0], GREEN);
        for y in 0..5 {
            let row = &out[y * 300..(y + 1) * 300];
            assert!(row[usize::from(y == 0)..256].iter().all(|&c| c == BLUE), "row {y}");
            assert!(row[256..].iter().all(|&c| c == RED), "row {y}");
        }
    }

    #[test]
    fn zrle_rejects_runs_past_the_tile() {
        let mut z = Compress::new(Compression::default(), true);
        let mut t = vec![128];
        t.extend(cpx(RED));
        t.push(9);
        let msg = zrle_rect(&mut z, &t);
        assert!(Zrle::default().decode(&mut &msg[..], Rect::new(0, 0, 2, 2)).is_err());
    }

    #[test]
    fn cursor_mask_makes_transparent_pixels() {
        let mut d = Vec::new();
        for c in [RED, GREEN, BLUE, RED] {
            d.extend(px(c));
        }
        d.extend([0b1000_0000, 0b0100_0000]);
        let c = cursor(&mut &d[..], Rect::new(1, 0, 2, 2)).unwrap();
        assert_eq!(c.hotspot, (1, 0));
        assert_eq!(c.pixels, vec![RED, Color32::TRANSPARENT, Color32::TRANSPARENT, RED]);
    }
}
