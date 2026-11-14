//! Kitty direct-transfer encoder adapted from desktui 0.4.0.

use std::io::Write as _;

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const CHUNK: usize = 4096;
const PLACEMENT_ID: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub image_id: u32,
    /// Pane-local, zero-based cell position.
    pub col: u16,
    pub row: u16,
    pub width_px: u32,
    pub height_px: u32,
    pub cell_cols: u16,
    pub cell_rows: u16,
}

/// Upload packed RGB pixels, then place them without shared-memory dependencies.
pub fn place_rgb(out: &mut Vec<u8>, placement: Placement, rgb: &[u8]) {
    place_rgb_at_z(out, placement, rgb, -1);
}

/// Upload and place an image at an explicit layer relative to terminal text.
pub fn place_rgb_at_z(out: &mut Vec<u8>, placement: Placement, rgb: &[u8], z: i32) {
    if placement.width_px == 0 || placement.height_px == 0 {
        return;
    }
    let expected = placement.width_px as usize * placement.height_px as usize * 3;
    assert_eq!(
        rgb.len(),
        expected,
        "packed RGB length must match placement dimensions"
    );
    let payload = encode_base64(&compress(rgb));
    let _ = write!(
        out,
        "\x1b_Ga=d,d=i,i={},p={PLACEMENT_ID},q=2\x1b\\",
        placement.image_id
    );
    for (index, chunk) in payload.chunks(CHUNK).enumerate() {
        let more = usize::from(index + 1 < payload.len().div_ceil(CHUNK));
        if index == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=t,q=2,f=24,o=z,i={},s={},v={}",
                placement.image_id, placement.width_px, placement.height_px
            );
            if payload.len() > CHUNK {
                let _ = write!(out, ",m={more}");
            }
        } else {
            let _ = write!(out, "\x1b_Gm={more},q=2");
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    place_existing_at_z(
        out,
        placement.image_id,
        placement.col,
        placement.row,
        placement.cell_cols,
        placement.cell_rows,
        z,
    );
}

/// Encode an image with Kitty's virtual-placement mode. Its image is visible only
/// under the Unicode placeholders emitted by the pane's normal text renderer.
pub fn encode_virtual_rgb(
    out: &mut Vec<u8>,
    image_id: u32,
    width: u32,
    height: u32,
    cell_size: (u32, u32),
    rgb: &[u8],
) -> (u16, u16) {
    if width == 0 || height == 0 || cell_size.0 == 0 || cell_size.1 == 0 {
        return (0, 0);
    }
    assert_eq!(rgb.len(), width as usize * height as usize * 3);
    let cols = width.div_ceil(cell_size.0);
    let rows = height.div_ceil(cell_size.1);
    assert!(
        cols <= 128 && rows <= 128,
        "virtual image tiles are limited to 128 cells per side"
    );
    let padded_width = cols * cell_size.0;
    let padded_height = rows * cell_size.1;
    // Opaque RGB padded with black to whole cells: a quarter fewer bytes than
    // RGBA before compression, and the padding compresses to almost nothing.
    let row_bytes = width as usize * 3;
    let padded_row = padded_width as usize * 3;
    let mut padded = vec![0; padded_row * padded_height as usize];
    for (dst, src) in padded
        .chunks_exact_mut(padded_row)
        .zip(rgb.chunks_exact(row_bytes))
    {
        dst[..row_bytes].copy_from_slice(src);
    }
    let payload = encode_base64(&compress(&padded));
    let chunks = payload.chunks(CHUNK).collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = usize::from(index + 1 < chunks.len());
        if index == 0 {
            let _ = write!(
                out,
                "\x1b_Ga=T,U=1,q=2,f=24,o=z,i={image_id},p={PLACEMENT_ID},s={padded_width},v={padded_height},c={cols},r={rows},m={more};"
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more},q=2;");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    (cols as u16, rows as u16)
}

/// Unicode placeholder used by Kitty virtual placement, with image-id RGB and
/// Kitty's row/column/ID diacritics. Tile dimensions must not exceed 128 cells.
pub fn placeholder_glyph(image_id: u32, row: u16, col: u16) -> Option<String> {
    if row >= 128 || col >= 128 {
        return None;
    }
    let mut glyph = String::from("\u{10eeee}");
    glyph.push(DIACRITICS[row as usize]);
    glyph.push(DIACRITICS[col as usize]);
    glyph.push(DIACRITICS[(image_id >> 24) as usize]);
    Some(glyph)
}

pub fn placeholder_foreground(image_id: u32) -> (u8, u8, u8) {
    (
        (image_id >> 16) as u8,
        (image_id >> 8) as u8,
        image_id as u8,
    )
}

/// Place previously transmitted image data beneath terminal text.
pub fn place_existing(out: &mut Vec<u8>, image_id: u32, col: u16, row: u16, cols: u16, rows: u16) {
    place_existing_at_z(out, image_id, col, row, cols, rows, -1);
}

/// Place previously transmitted image data at an explicit layer.
pub fn place_existing_at_z(
    out: &mut Vec<u8>,
    image_id: u32,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    z: i32,
) {
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b_Ga=p,q=2,z={z},i={image_id},p={PLACEMENT_ID},c={cols},r={rows}\x1b\\",
        u32::from(row) + 1,
        u32::from(col) + 1
    );
}

/// Delete one placement while retaining the uploaded image data.
pub fn delete_placement(out: &mut Vec<u8>, image_id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=i,i={image_id},p={PLACEMENT_ID},q=2\x1b\\");
}

/// Delete one image and its data.
pub fn delete_image(out: &mut Vec<u8>, image_id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\");
}

const DIACRITICS: [char; 128] = [
    '\u{305}', '\u{30d}', '\u{30e}', '\u{310}', '\u{312}', '\u{33d}', '\u{33e}', '\u{33f}',
    '\u{346}', '\u{34a}', '\u{34b}', '\u{34c}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35b}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36a}', '\u{36b}', '\u{36c}', '\u{36d}', '\u{36e}', '\u{36f}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59c}', '\u{59d}', '\u{59e}', '\u{59f}', '\u{5a0}', '\u{5a1}',
    '\u{5a8}', '\u{5a9}', '\u{5ab}', '\u{5ac}', '\u{5af}', '\u{5c4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
    '\u{659}', '\u{65a}', '\u{65b}', '\u{65d}', '\u{65e}', '\u{6d6}', '\u{6d7}', '\u{6d8}',
    '\u{6d9}', '\u{6da}', '\u{6db}', '\u{6dc}', '\u{6df}', '\u{6e0}', '\u{6e1}', '\u{6e2}',
    '\u{6e4}', '\u{6e7}', '\u{6e8}', '\u{6eb}', '\u{6ec}', '\u{730}', '\u{732}', '\u{733}',
    '\u{735}', '\u{736}', '\u{73a}', '\u{73d}', '\u{73f}', '\u{740}', '\u{741}', '\u{743}',
    '\u{745}', '\u{747}', '\u{749}', '\u{74a}', '\u{7eb}', '\u{7ec}', '\u{7ed}', '\u{7ee}',
    '\u{7ef}', '\u{7f0}', '\u{7f1}', '\u{7f3}', '\u{816}', '\u{817}', '\u{818}', '\u{819}',
    '\u{81b}', '\u{81c}', '\u{81d}', '\u{81e}', '\u{81f}', '\u{820}', '\u{821}', '\u{822}',
    '\u{823}', '\u{825}', '\u{826}', '\u{827}', '\u{829}', '\u{82a}', '\u{82b}', '\u{82c}',
];

/// Zlib-compress pixel data for Kitty's `o=z`. Console pictures are mostly
/// flat colour, so this shrinks a frame many times over; the fastest level
/// keeps the encoder cheap next to the bytes it saves on the terminal path.
fn compress(bytes: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    let mut encoder = ZlibEncoder::new(
        Vec::with_capacity(bytes.len() / 8 + 64),
        Compression::fast(),
    );
    // Writing to a Vec cannot fail.
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

fn encode_base64(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(BASE64[(a >> 2) as usize]);
        out.push(BASE64[(((a & 3) << 4) | (b >> 4)) as usize]);
        out.push(if chunk.len() > 1 {
            BASE64[(((b & 15) << 2) | (c >> 6)) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            BASE64[(c & 63) as usize]
        } else {
            b'='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_direct_transfer_at_pane_local_cursor() {
        let mut out = Vec::new();
        place_rgb(
            &mut out,
            Placement {
                image_id: 7,
                col: 2,
                row: 3,
                width_px: 1,
                height_px: 1,
                cell_cols: 1,
                cell_rows: 1,
            },
            &[0xff, 0, 0],
        );
        assert!(out.windows(6).any(|w| w == b"\x1b[4;3H"));
        assert!(out.windows(6).any(|w| w == b"\x1b_Ga=t"));
        assert!(out.windows(6).any(|w| w == b"\x1b_Ga=p"));
        assert!(!out.windows(6).any(|w| w == b"\x1b_Ga=T"));
        assert!(out.windows(9).any(|w| w == b"f=24,o=z,"));
        let payload = out
            .split(|b| *b == b';')
            .nth(1)
            .and_then(|rest| rest.split(|b| *b == 0x1b).next())
            .unwrap();
        assert_eq!(inflate(&decode_base64(payload)), vec![0xff, 0, 0]);
    }

    #[test]
    fn multipart_direct_transfer_places_only_after_upload_completes() {
        let mut out = Vec::new();
        // Noise, so the compressed payload still spans several chunks.
        let mut seed = 0x1234_5678_u32;
        let pixels = (0..64 * 64 * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect::<Vec<_>>();
        place_rgb(
            &mut out,
            Placement {
                image_id: 7,
                col: 2,
                row: 3,
                width_px: 64,
                height_px: 64,
                cell_cols: 8,
                cell_rows: 4,
            },
            &pixels,
        );

        let placement = out
            .windows(b"\x1b_Ga=p".len())
            .position(|bytes| bytes == b"\x1b_Ga=p")
            .unwrap();
        let cup = out[..placement]
            .windows(2)
            .rposition(|bytes| bytes == b"\x1b[")
            .unwrap();
        assert!(out[..cup].ends_with(b"\x1b\\"));
        assert!(
            out[..placement]
                .windows(b"\x1b_Gm=".len())
                .filter(|bytes| *bytes == b"\x1b_Gm=")
                .count()
                >= 2
        );
        assert!(!out[..placement].windows(4).any(|bytes| bytes == b"\x1b_G,"));
    }

    #[test]
    fn explicit_placement_layer_is_encoded_for_ordered_overlays() {
        let mut out = Vec::new();
        place_rgb_at_z(
            &mut out,
            Placement {
                image_id: 7,
                col: 2,
                row: 3,
                width_px: 1,
                height_px: 1,
                cell_cols: 1,
                cell_rows: 1,
            },
            &[0xff, 0, 0],
            1,
        );
        assert!(
            out.windows(b"a=p,q=2,z=1".len())
                .any(|w| w == b"a=p,q=2,z=1")
        );
    }

    #[test]
    fn virtual_transfer_uses_virtual_placements_and_cell_tiles() {
        let mut out = Vec::new();
        let cells = encode_virtual_rgb(&mut out, 0x7600, 1, 1, (2, 2), &[1, 2, 3]);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(cells, (1, 1));
        assert!(text.starts_with("\x1b_Ga=T,U=1,q=2,f=24,o=z,i=30208,p=1,s=2,v=2,c=1,r=1"));
        let payload = text.split_once(';').unwrap().1.trim_end_matches("\x1b\\");
        // The 1x1 picture padded with black to the 2x2 cell.
        assert_eq!(
            inflate(&decode_base64(payload.as_bytes())),
            vec![1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert!(!text.contains("a=d"));
        assert!(text.ends_with("\x1b\\"));
    }

    #[test]
    fn placeholder_identifies_image_and_row_column() {
        assert_eq!(placeholder_foreground(0x123456), (0x12, 0x34, 0x56));
        let glyph = placeholder_glyph(0x7600, 1, 2).unwrap();
        assert!(glyph.starts_with("\u{10eeee}"));
        assert!(placeholder_glyph(0x7600, 128, 0).is_none());
    }

    fn decode_base64(text: &[u8]) -> Vec<u8> {
        let value = |c: u8| BASE64.iter().position(|b| *b == c).map(|v| v as u32);
        let mut out = Vec::new();
        for quad in text.chunks(4) {
            let digits = quad.iter().filter_map(|c| value(*c)).collect::<Vec<_>>();
            let bits =
                digits.iter().fold(0u32, |acc, d| (acc << 6) | d) << (6 * (4 - digits.len()));
            out.extend_from_slice(&bits.to_be_bytes()[1..digits.len()]);
        }
        out
    }

    fn inflate(bytes: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(bytes)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn flat_console_frames_compress_many_times_over() {
        // Black screen with a line of text: the common console picture.
        let mut rgb = vec![0u8; 1024 * 768 * 3];
        for x in 0..600 {
            rgb[(20 * 1024 + x) * 3..][..3].copy_from_slice(&[0xaa, 0xaa, 0xaa]);
        }
        assert!(compress(&rgb).len() * 50 < rgb.len());
    }

    #[test]
    fn chunks_payload_at_protocol_limit() {
        let bytes = vec![0x5a; CHUNK];
        let encoded = encode_base64(&bytes);
        assert!(encoded.len() > CHUNK);
        assert!(encoded.len().is_multiple_of(4));
    }
}
