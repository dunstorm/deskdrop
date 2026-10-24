//! Image conversions between clipboard formats (PNG is the wire format).

use image::{DynamicImage, ImageFormat};
use std::io::Cursor;

fn encode_png(img: DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).ok()?;
    Some(out)
}

/// Any image format `image` can sniff (TIFF from macOS, etc.) -> PNG.
#[allow(dead_code)]
pub fn to_png(data: &[u8]) -> Option<Vec<u8>> {
    encode_png(image::load_from_memory(data).ok()?)
}

/// Windows CF_DIB (BITMAPINFO + pixels, no file header) -> PNG.
#[allow(dead_code)]
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let hdr = u32_at(0) as usize;
    let bits = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    let compression = u32_at(16);
    let clr_used = u32_at(32) as usize;
    let masks = if hdr == 40 && compression == 3 { 12 } else { 0 };
    let palette = if clr_used > 0 {
        clr_used
    } else if bits <= 8 {
        1 << bits
    } else {
        0
    };
    let off = 14 + hdr + masks + palette * 4;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(off as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    let img = image::load_from_memory_with_format(&bmp, ImageFormat::Bmp).ok()?;
    // 32bpp DIBs from screenshots often carry an all-zero alpha channel.
    let img = match img {
        DynamicImage::ImageRgba8(mut buf) if buf.pixels().all(|p| p[3] == 0) => {
            buf.pixels_mut().for_each(|p| p[3] = 255);
            DynamicImage::ImageRgba8(buf)
        }
        other => other,
    };
    encode_png(img)
}

/// PNG -> Windows CF_DIB (32bpp bottom-up BI_RGB).
#[allow(dead_code)]
pub fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(w * h * 4).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y);
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use image::{ImageFormat, Rgba, RgbaImage};

    #[test]
    fn dib_roundtrip_and_zero_alpha() {
        let mut img = RgbaImage::new(3, 2);
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        img.put_pixel(2, 1, Rgba([0, 0, 255, 128]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let dib = super::png_to_dib(&png).unwrap();
        assert_eq!(dib.len(), 40 + 3 * 2 * 4);
        let back = image::load_from_memory(&super::dib_to_png(&dib).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(back.get_pixel(0, 0).0[..3], [255, 0, 0]);
        assert_eq!(back.get_pixel(2, 1).0[..3], [0, 0, 255]);
        // all-zero alpha (typical screenshot DIB) must come out opaque
        let mut z = dib.clone();
        for px in z[40..].chunks_mut(4) {
            px[3] = 0;
        }
        let zb = image::load_from_memory(&super::dib_to_png(&z).unwrap())
            .unwrap()
            .to_rgba8();
        assert!(zb.pixels().all(|p| p[3] == 255));
    }
}
