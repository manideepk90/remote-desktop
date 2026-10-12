//! The client-side copy of the remote screen, shared between the network thread and the UI.

use std::sync::Arc;

use egui::Color32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn new(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn union(self, o: Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let right = (self.x + self.w).max(o.x + o.w);
        let bottom = (self.y + self.h).max(o.y + o.h);
        Rect { x, y, w: right - x, h: bottom - y }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn fits(&self, width: usize, height: usize) -> bool {
        self.x + self.w <= width && self.y + self.h <= height
    }
}

/// A cursor shape sent by the server, drawn locally at the pointer position.
pub struct Cursor {
    pub width: usize,
    pub height: usize,
    pub hotspot: (usize, usize),
    pub pixels: Vec<Color32>,
}

pub struct Framebuffer {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Color32>,
    /// Area changed since the viewer last copied it.
    pub dirty: Option<Rect>,
    /// The size changed since the viewer last copied it, so it needs a full upload.
    pub resized: bool,
    pub name: String,
    /// `None` until the server sends a shape (or if it never does).
    pub cursor: Option<Arc<Cursor>>,
    /// Bumped on every cursor change so the viewer knows to re-upload it.
    pub cursor_serial: u64,
}

impl Framebuffer {
    pub fn new(width: usize, height: usize, name: String) -> Framebuffer {
        Framebuffer {
            width,
            height,
            pixels: vec![Color32::BLACK; width * height],
            dirty: None,
            resized: true,
            name,
            cursor: None,
            cursor_serial: 0,
        }
    }

    /// Changes the size, keeping whatever overlaps the old image.
    pub fn resize(&mut self, width: usize, height: usize) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let mut pixels = vec![Color32::BLACK; width * height];
        let (cw, ch) = (width.min(self.width), height.min(self.height));
        for y in 0..ch {
            pixels[y * width..y * width + cw].copy_from_slice(&self.pixels[y * self.width..y * self.width + cw]);
        }
        self.width = width;
        self.height = height;
        self.pixels = pixels;
        self.dirty = None;
        self.resized = true;
    }

    fn mark(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        self.dirty = Some(self.dirty.map_or(r, |d| d.union(r)));
    }

    /// Copies a rectangle of pixels (row-major, `r.w * r.h`) into place.
    pub fn blit(&mut self, r: Rect, src: &[Color32]) {
        debug_assert!(r.fits(self.width, self.height) && src.len() == r.w * r.h);
        for row in 0..r.h {
            let dst = (r.y + row) * self.width + r.x;
            self.pixels[dst..dst + r.w].copy_from_slice(&src[row * r.w..(row + 1) * r.w]);
        }
        self.mark(r);
    }

    /// CopyRect: moves an area of the screen to `dst`. Source and destination may overlap.
    pub fn copy_within(&mut self, src_x: usize, src_y: usize, dst: Rect) {
        let rows: Vec<usize> = if src_y < dst.y { (0..dst.h).rev().collect() } else { (0..dst.h).collect() };
        for row in rows {
            let from = (src_y + row) * self.width + src_x;
            let to = (dst.y + row) * self.width + dst.x;
            self.pixels.copy_within(from..from + dst.w, to);
        }
        self.mark(dst);
    }

    /// The pixels of `r`, row-major.
    pub fn region(&self, r: Rect) -> Vec<Color32> {
        let mut out = Vec::with_capacity(r.w * r.h);
        for row in 0..r.h {
            let start = (r.y + row) * self.width + r.x;
            out.extend_from_slice(&self.pixels[start..start + r.w]);
        }
        out
    }

    pub fn set_cursor(&mut self, cursor: Option<Cursor>) {
        self.cursor = cursor.map(Arc::new);
        self.cursor_serial += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fb_with_rows(w: usize, h: usize) -> Framebuffer {
        let mut fb = Framebuffer::new(w, h, String::new());
        for (i, p) in fb.pixels.iter_mut().enumerate() {
            *p = Color32::from_rgb(i as u8, 0, 0);
        }
        fb
    }

    #[test]
    fn copy_within_handles_overlap_downwards() {
        let mut fb = fb_with_rows(4, 4);
        let before = fb.pixels.clone();
        fb.copy_within(0, 0, Rect::new(0, 1, 4, 3));
        for y in 1..4 {
            for x in 0..4 {
                assert_eq!(fb.pixels[y * 4 + x], before[(y - 1) * 4 + x]);
            }
        }
        assert_eq!(fb.dirty, Some(Rect::new(0, 1, 4, 3)));
    }

    #[test]
    fn copy_within_handles_overlap_upwards() {
        let mut fb = fb_with_rows(4, 4);
        let before = fb.pixels.clone();
        fb.copy_within(1, 1, Rect::new(0, 0, 3, 3));
        for y in 0..3 {
            for x in 0..3 {
                assert_eq!(fb.pixels[y * 4 + x], before[(y + 1) * 4 + x + 1]);
            }
        }
    }

    #[test]
    fn resize_keeps_overlap() {
        let mut fb = fb_with_rows(4, 4);
        fb.resized = false;
        let before = fb.pixels.clone();
        fb.resize(2, 6);
        assert!(fb.resized);
        assert_eq!(fb.pixels.len(), 12);
        assert_eq!(fb.pixels[2], before[4]);
        assert_eq!(fb.pixels[11], Color32::BLACK);
    }

    #[test]
    fn dirty_area_is_the_union() {
        let mut fb = Framebuffer::new(10, 10, String::new());
        fb.blit(Rect::new(1, 1, 1, 1), &[Color32::RED]);
        fb.blit(Rect::new(5, 7, 2, 1), &[Color32::RED; 2]);
        assert_eq!(fb.dirty, Some(Rect::new(1, 1, 6, 7)));
    }
}
