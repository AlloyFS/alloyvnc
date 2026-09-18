use alloyvnc_region::Rect;

/// The picture as captured: tightly packed 32-bit BGRX rows, the layout
/// DXGI and X11 produce and the one [`PixelFormat::bgrx32`] describes on
/// the wire. The fourth byte is padding and carries nothing.
///
/// [`PixelFormat::bgrx32`]: alloyvnc_proto::PixelFormat::bgrx32
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Framebuffer {
    pub const BYTES_PER_PIXEL: usize = 4;

    /// A black picture of the given size.
    pub fn new(width: u32, height: u32) -> Framebuffer {
        let data = vec![0; width as usize * height as usize * Self::BYTES_PER_PIXEL];
        Framebuffer { width, height, data }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row.
    pub fn stride(&self) -> usize {
        self.width as usize * Self::BYTES_PER_PIXEL
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// Reallocate to a new size; the picture is black afterwards.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width != self.width || height != self.height {
            *self = Framebuffer::new(width, height);
        }
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let stride = self.stride();
        &self.data[y as usize * stride..][..stride]
    }

    /// `width` pixels of row `y` starting at column `x`.
    pub fn row_span(&self, y: u32, x: u32, width: u32) -> &[u8] {
        let start = y as usize * self.stride() + x as usize * Self::BYTES_PER_PIXEL;
        &self.data[start..start + width as usize * Self::BYTES_PER_PIXEL]
    }

    pub fn row_span_mut(&mut self, y: u32, x: u32, width: u32) -> &mut [u8] {
        let start = y as usize * self.stride() + x as usize * Self::BYTES_PER_PIXEL;
        &mut self.data[start..start + width as usize * Self::BYTES_PER_PIXEL]
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let s = self.row_span(y, x, 1);
        [s[0], s[1], s[2], s[3]]
    }

    pub fn put_pixel(&mut self, x: u32, y: u32, px: [u8; 4]) {
        self.row_span_mut(y, x, 1).copy_from_slice(&px);
    }

    /// Fill `rect`, clipped to the picture, with one pixel.
    pub fn fill(&mut self, rect: Rect, px: [u8; 4]) {
        let r = rect.intersection(&self.bounds());
        if r.is_empty() {
            return;
        }
        for y in r.y1..r.y2 {
            for dst in self
                .row_span_mut(y as u32, r.x1 as u32, r.width() as u32)
                .as_chunks_mut::<4>()
                .0
            {
                *dst = px;
            }
        }
    }

    /// Copy tightly packed BGRX rows into the picture at `(x, y)`.
    ///
    /// The block must fit; a capture backend clips before it gets here.
    pub fn blit(&mut self, x: u32, y: u32, width: u32, height: u32, src: &[u8]) {
        let row_bytes = width as usize * Self::BYTES_PER_PIXEL;
        assert!(
            src.len() >= row_bytes * height as usize,
            "blit source shorter than the block"
        );
        assert!(
            x + width <= self.width && y + height <= self.height,
            "blit outside the picture"
        );
        for (i, row) in src.chunks_exact(row_bytes).take(height as usize).enumerate() {
            self.row_span_mut(y + i as u32, x, width).copy_from_slice(row);
        }
    }

    /// The client side of CopyRect: move the block at `(src_x, src_y)` of
    /// `dst`'s size onto `dst`. Overlap is handled by choosing the row order.
    pub fn copy_within(&mut self, src_x: u32, src_y: u32, dst: Rect) {
        let dst = dst.intersection(&self.bounds());
        if dst.is_empty() {
            return;
        }
        let src = Rect::new(src_x as i32, src_y as i32, dst.width(), dst.height());
        assert!(self.bounds().contains(&src), "copy source outside the picture");
        let stride = self.stride();
        let row_bytes = dst.width() as usize * Self::BYTES_PER_PIXEL;
        let rows = dst.height();
        let downwards = (src_y as i32) < dst.y1;
        for k in 0..rows {
            let i = if downwards { rows - 1 - k } else { k };
            let from = (src_y as usize + i as usize) * stride + src_x as usize * Self::BYTES_PER_PIXEL;
            let to = (dst.y1 as usize + i as usize) * stride + dst.x1 as usize * Self::BYTES_PER_PIXEL;
            self.data.copy_within(from..from + row_bytes, to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_blit_and_copy() {
        let mut fb = Framebuffer::new(8, 8);
        fb.fill(Rect::new(2, 2, 3, 3), [1, 2, 3, 0]);
        assert_eq!(fb.pixel(2, 2), [1, 2, 3, 0]);
        assert_eq!(fb.pixel(4, 4), [1, 2, 3, 0]);
        assert_eq!(fb.pixel(5, 5), [0, 0, 0, 0]);
        // Clipped, not panicking.
        fb.fill(Rect::new(6, 6, 10, 10), [9, 9, 9, 0]);
        assert_eq!(fb.pixel(7, 7), [9, 9, 9, 0]);

        let block = [[7u8, 7, 7, 0]; 4].concat();
        fb.blit(0, 0, 2, 2, &block);
        assert_eq!(fb.pixel(1, 1), [7, 7, 7, 0]);
        assert_eq!(fb.pixel(2, 0), [0, 0, 0, 0]);

        // Overlapping move downwards keeps the source rows intact.
        let mut fb = Framebuffer::new(4, 6);
        for y in 0..6 {
            fb.fill(Rect::new(0, y, 4, 1), [y as u8, 0, 0, 0]);
        }
        fb.copy_within(0, 0, Rect::new(0, 2, 4, 4));
        let rows: Vec<u8> = (0..6).map(|y| fb.pixel(0, y)[0]).collect();
        assert_eq!(rows, [0, 1, 0, 1, 2, 3]);
        fb.copy_within(0, 2, Rect::new(0, 0, 4, 4));
        let rows: Vec<u8> = (0..6).map(|y| fb.pixel(0, y)[0]).collect();
        assert_eq!(rows, [0, 1, 2, 3, 2, 3]);
    }
}
