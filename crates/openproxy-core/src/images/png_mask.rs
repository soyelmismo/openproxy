//! Minimal PNG chunk parser, unfilter, and alpha mask extraction.

pub(crate) fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let a_i = i32::from(a);
    let b_i = i32::from(b);
    let c_i = i32::from(c);
    let p = a_i + b_i - c_i;
    let pa = (p - a_i).abs();
    let pb = (p - b_i).abs();
    let pc = (p - c_i).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

pub(crate) fn png_crc32_chunk(chunk_type: [u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in chunk_type.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

pub(crate) fn write_png_chunk(buf: &mut Vec<u8>, chunk_type: [u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&chunk_type);
    buf.extend_from_slice(data);
    let crc = png_crc32_chunk(chunk_type, data);
    buf.extend_from_slice(&crc.to_be_bytes());
}

/// Extract an inpainting mask from the transparent alpha channel of a PNG image.
/// Returns `Some(mask_png_bytes)` if transparency was found, or `None` if the image
/// has no transparency, is opaque, or is not a valid 8-bit RGBA/GA PNG.
struct PngHeader {
    width: u32,
    height: u32,
    color_type: u8,
    idat_data: Vec<u8>,
}

fn is_valid_ihdr_format(
    bit_depth: u8,
    compression: u8,
    filter: u8,
    interlace: u8,
    color_type: u8,
) -> bool {
    bit_depth == 8
        && compression == 0
        && filter == 0
        && interlace == 0
        && matches!(color_type, 4 | 6)
}

fn parse_ihdr_chunk(data: &[u8]) -> Option<(u32, u32, u8)> {
    if data.len() < 13 {
        return None;
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let bit_depth = data[8];
    let color_type = data[9];
    let compression = data[10];
    let filter = data[11];
    let interlace = data[12];

    if !is_valid_ihdr_format(bit_depth, compression, filter, interlace, color_type) {
        return None;
    }
    Some((width, height, color_type))
}

fn parse_png_chunks(image_bytes: &[u8]) -> Option<PngHeader> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !image_bytes.starts_with(&PNG_SIG) {
        return None;
    }

    let mut offset = 8;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut color_type = 0u8;
    let mut idat_data = Vec::new();

    while offset + 8 <= image_bytes.len() {
        let chunk_len = u32::from_be_bytes([
            image_bytes[offset],
            image_bytes[offset + 1],
            image_bytes[offset + 2],
            image_bytes[offset + 3],
        ]) as usize;
        let chunk_type = &image_bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start + chunk_len;
        if data_end + 4 > image_bytes.len() {
            return None;
        }

        if chunk_type == b"IHDR" {
            let (w, h, ct) = parse_ihdr_chunk(&image_bytes[data_start..data_end])?;
            width = w;
            height = h;
            color_type = ct;
        } else if chunk_type == b"IDAT" {
            idat_data.extend_from_slice(&image_bytes[data_start..data_end]);
        } else if chunk_type == b"IEND" {
            break;
        }

        offset = data_end + 4; // chunk data + 4 bytes CRC
    }

    if width == 0 || height == 0 || idat_data.is_empty() {
        return None;
    }

    Some(PngHeader {
        width,
        height,
        color_type,
        idat_data,
    })
}

fn unfilter_png_scanline(
    filter_type: u8,
    raw_data: &[u8],
    prev_row: &[u8],
    curr_row: &mut [u8],
    bpp: usize,
) {
    for (i, &x) in raw_data.iter().enumerate() {
        let a = if i >= bpp { curr_row[i - bpp] } else { 0 };
        let b = prev_row[i];
        let c = if i >= bpp { prev_row[i - bpp] } else { 0 };

        curr_row[i] = match filter_type {
            0 => x,
            1 => x.wrapping_add(a),
            2 => x.wrapping_add(b),
            3 => x.wrapping_add(u16::midpoint(u16::from(a), u16::from(b)) as u8),
            4 => x.wrapping_add(paeth_predictor(a, b, c)),
            _ => x,
        };
    }
}

fn extract_mask_scanlines(
    decompressed: &[u8],
    width: u32,
    height: u32,
    color_type: u8,
) -> Option<Vec<u8>> {
    let bpp: usize = if color_type == 6 { 4 } else { 2 };
    let line_len = (width as usize).checked_mul(bpp)?;
    let stride = 1 + line_len;
    let expected_len = (height as usize).checked_mul(stride)?;

    if decompressed.len() != expected_len {
        return None;
    }

    let mut prev_row = vec![0u8; line_len];
    let mut curr_row = vec![0u8; line_len];
    let mut mask_scanlines = Vec::with_capacity(height as usize * (1 + width as usize));
    let mut has_transparency = false;

    for y in 0..height as usize {
        let filter_type = decompressed[y * stride];
        let raw_data = &decompressed[y * stride + 1..(y + 1) * stride];
        unfilter_png_scanline(filter_type, raw_data, &prev_row, &mut curr_row, bpp);

        mask_scanlines.push(0);
        for px in 0..width as usize {
            let alpha = if color_type == 6 {
                curr_row[px * 4 + 3]
            } else {
                curr_row[px * 2 + 1]
            };

            if alpha < 255 {
                has_transparency = true;
                mask_scanlines.push(255);
            } else {
                mask_scanlines.push(0);
            }
        }
        prev_row.copy_from_slice(&curr_row);
    }

    if has_transparency {
        Some(mask_scanlines)
    } else {
        None
    }
}

fn encode_grayscale_png(width: u32, height: u32, mask_scanlines: &[u8]) -> Vec<u8> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let compressed_mask = miniz_oxide::deflate::compress_to_vec_zlib(mask_scanlines, 6);
    let mut out = Vec::with_capacity(33 + compressed_mask.len() + 12);
    out.extend_from_slice(&PNG_SIG);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth 8
    ihdr.push(0); // color type 0 (Grayscale)
    ihdr.push(0); // compression method (zlib)
    ihdr.push(0); // filter method (adaptive)
    ihdr.push(0); // interlace method (none)
    write_png_chunk(&mut out, *b"IHDR", &ihdr);
    write_png_chunk(&mut out, *b"IDAT", &compressed_mask);
    write_png_chunk(&mut out, *b"IEND", &[]);
    out
}

/// Extract an inpainting mask from the transparent alpha channel of a PNG image.
/// Returns `Some(mask_png_bytes)` if transparency was found, or `None` if the image
/// has no transparency, is opaque, or is not a valid 8-bit RGBA/GA PNG.
pub fn extract_png_alpha_mask(image_bytes: &[u8]) -> Option<Vec<u8>> {
    let header = parse_png_chunks(image_bytes)?;
    let decompressed = miniz_oxide::inflate::decompress_to_vec_zlib(&header.idat_data).ok()?;
    let mask_scanlines = extract_mask_scanlines(
        &decompressed,
        header.width,
        header.height,
        header.color_type,
    )?;
    Some(encode_grayscale_png(
        header.width,
        header.height,
        &mask_scanlines,
    ))
}
