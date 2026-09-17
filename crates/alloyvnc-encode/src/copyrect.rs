//! CopyRect (encoding 1): "the pixels you already have at (x, y)". Four
//! bytes for a moved window or a scrolled page, however large.

/// Append the source position of a CopyRect rectangle to `out`.
pub fn encode(src_x: u16, src_y: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&src_x.to_be_bytes());
    out.extend_from_slice(&src_y.to_be_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn four_bytes() {
        let mut out = Vec::new();
        super::encode(0x0102, 0x0304, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }
}
