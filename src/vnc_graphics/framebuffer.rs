//! A bounds-checked BGRA framebuffer adapted from desktui 0.4.0.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let mut fb = Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        fb.resize(width, height);
        fb
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
    pub(crate) fn pixels_mut_copy_within(&mut self, range: std::ops::Range<usize>, target: usize) {
        self.pixels.copy_within(range, target);
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        let len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .expect("framebuffer dimensions exceed addressable memory");
        let mut next = vec![0; len];
        for px in next.as_chunks_mut::<4>().0 {
            px[3] = 0xff;
        }
        let copy_w = width.min(self.width) as usize * 4;
        for y in 0..height.min(self.height) as usize {
            let from = y * self.width as usize * 4;
            let to = y * width as usize * 4;
            next[to..to + copy_w].copy_from_slice(&self.pixels[from..from + copy_w]);
        }
        self.width = width;
        self.height = height;
        self.pixels = next;
    }

    fn clipped(&self, r: Rect) -> Option<Rect> {
        let x = r.x.min(self.width);
        let y = r.y.min(self.height);
        let w = r.width.min(self.width - x);
        let h = r.height.min(self.height - y);
        (w > 0 && h > 0).then_some(Rect::new(x, y, w, h))
    }

    /// Apply tightly packed BGRA pixels and return the clipped damage region.
    pub fn apply_bgra(&mut self, rect: Rect, src: &[u8]) -> Option<Rect> {
        let clipped = self.clipped(rect)?;
        let stride = rect.width as usize * 4;
        if src.len() < stride.checked_mul(rect.height as usize)? {
            return None;
        }
        for row in 0..clipped.height {
            let src_start = row as usize * stride;
            let dst_start =
                ((clipped.y + row) as usize * self.width as usize + clipped.x as usize) * 4;
            let count = clipped.width as usize * 4;
            self.pixels[dst_start..dst_start + count]
                .copy_from_slice(&src[src_start..src_start + count]);
            for px in self.pixels[dst_start..dst_start + count]
                .as_chunks_mut::<4>()
                .0
            {
                px[3] = 0xff;
            }
        }
        Some(clipped)
    }

    /// Apply packed RGB pixels, converting them to opaque BGRA.
    pub fn apply_rgb(&mut self, rect: Rect, src: &[u8]) -> Option<Rect> {
        let clipped = self.clipped(rect)?;
        let stride = (rect.width as usize).checked_mul(3)?;
        if src.len() < stride.checked_mul(rect.height as usize)? {
            return None;
        }
        for row in 0..clipped.height {
            let src_row = row as usize * stride;
            let dst = ((clipped.y + row) as usize * self.width as usize + clipped.x as usize) * 4;
            for (pixel, rgb) in self.pixels[dst..dst + clipped.width as usize * 4]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(
                    src[src_row..]
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .take(clipped.width as usize),
                )
            {
                pixel.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 0xff]);
            }
        }
        Some(clipped)
    }

    /// Append a rectangle as packed RGB; pixels beyond the right/bottom edge are black.
    #[allow(dead_code)] // Public framebuffer helper for standalone renderer callers.
    pub fn pack_rgb(&self, rect: Rect, output: &mut Vec<u8>) {
        for y in rect.y..rect.y.saturating_add(rect.height) {
            let inside = if y < self.height {
                rect.width.min(self.width.saturating_sub(rect.x))
            } else {
                0
            };
            if inside > 0 {
                let start = (y as usize * self.width as usize + rect.x as usize) * 4;
                for bgra in self.pixels[start..start + inside as usize * 4]
                    .as_chunks::<4>()
                    .0
                {
                    output.extend_from_slice(&[bgra[2], bgra[1], bgra[0]]);
                }
            }
            output.resize(output.len() + (rect.width - inside) as usize * 3, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_input_is_swizzled_and_opaque() {
        let mut fb = Framebuffer::new(2, 1);
        assert_eq!(
            fb.apply_rgb(Rect::new(0, 0, 1, 1), &[0xaa, 0xbb, 0xcc]),
            Some(Rect::new(0, 0, 1, 1))
        );
        assert_eq!(&fb.pixels()[..4], &[0xcc, 0xbb, 0xaa, 0xff]);
    }

    #[test]
    fn clips_updates_and_packs_outside_as_black() {
        let mut fb = Framebuffer::new(1, 1);
        assert_eq!(
            fb.apply_bgra(Rect::new(0, 0, 2, 1), &[1, 2, 3, 0, 4, 5, 6, 0]),
            Some(Rect::new(0, 0, 1, 1))
        );
        let mut packed = Vec::new();
        fb.pack_rgb(Rect::new(0, 0, 2, 1), &mut packed);
        assert_eq!(packed, [3, 2, 1, 0, 0, 0]);
    }

    #[test]
    fn resize_preserves_overlap_and_initializes_padding() {
        let mut fb = Framebuffer::new(1, 1);
        fb.apply_rgb(Rect::new(0, 0, 1, 1), &[7, 8, 9]);
        fb.resize(2, 1);
        assert_eq!(&fb.pixels()[..4], &[9, 8, 7, 0xff]);
        assert_eq!(&fb.pixels()[4..], &[0, 0, 0, 0xff]);
    }
}
