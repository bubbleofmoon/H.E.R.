//! Synthetic media only; shared by the normalizer and public MCP harness.
#![allow(dead_code)]
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use std::io::Cursor;

pub fn evidence(name: &str, bytes: &[u8]) {
    if let Some(dir) = std::env::var_os("TICKET19_MEDIA_EVIDENCE") {
        let dir = crate::install_root::development_root(std::path::PathBuf::from(dir));
        let file = crate::install_root::development_root(dir.join(name));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(file, bytes).unwrap();
    }
}

pub fn still(format: ImageFormat, w: u32, h: u32, alpha: bool) -> Vec<u8> {
    let img = if alpha {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(w, h, |x, y| {
            Rgba([
                (x % 256) as u8,
                (y % 256) as u8,
                70,
                if x < w / 2 { 0 } else { 180 },
            ])
        }))
    } else {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        }))
    };
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

pub fn gif(count: usize, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 30);
        for i in 0..count {
            let color = Rgba([
                (i % 256) as u8,
                ((i * 7) % 256) as u8,
                ((i * 13) % 256) as u8,
                255,
            ]);
            encoder
                .encode_frame(image::Frame::from_parts(
                    RgbaImage::from_pixel(w, h, color),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(50, 1),
                ))
                .unwrap();
        }
    }
    out
}

/// Hand-built animated WebP container containing lossless, full-canvas frames.
pub fn webp_animation(count: usize) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, name: &[u8; 4], data: &[u8]) {
        out.extend(name);
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        if data.len() % 2 != 0 {
            out.push(0);
        }
    }
    let mut chunks = Vec::new();
    chunk(&mut chunks, b"VP8X", &[2, 0, 0, 0, 3, 0, 0, 3, 0, 0]);
    chunk(&mut chunks, b"ANIM", &[0; 6]);
    for i in 0..count {
        let mut still = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(
            4,
            4,
            Rgb([i as u8, (i * 7) as u8, (i * 13) as u8]),
        ))
        .write_to(&mut still, ImageFormat::WebP)
        .unwrap();
        let mut frame = vec![0; 16];
        frame[6] = 3;
        frame[9] = 3;
        frame[12] = 50;
        frame[15] = 2;
        frame.extend(&still.get_ref()[12..]);
        chunk(&mut chunks, b"ANMF", &frame);
    }
    let mut out = b"RIFF".to_vec();
    out.extend(((chunks.len() + 4) as u32).to_le_bytes());
    out.extend(b"WEBP");
    out.extend(chunks);
    out
}
