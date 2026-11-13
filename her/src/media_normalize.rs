//! Shared, bounded image preparation at every model delivery boundary.
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat};
use std::io::Cursor;

pub const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 96 * 1024;
const PASS_BYTES: usize = 64 * 1024;
const MAX_FRAMES: usize = 256;
const MAX_FRAME_PIXELS: u64 = 128_000_000;
const WORK_MEMORY: u64 = 192 * 1024 * 1024;
// Serialize decoder/encoder working sets within a process, including concurrent HTTP calls.
static DECODE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

#[derive(Debug)]
pub struct Normalized {
    pub images: Vec<EncodedImage>,
    pub note: Option<String>,
    pub original_mime: &'static str,
}

pub fn normalize(bytes: &[u8], max_pixels: u64) -> Result<Normalized, ()> {
    let _gate = DECODE_GATE.lock().unwrap_or_else(|p| p.into_inner());
    // Decoder failures never cross a public boundary with paths or library diagnostics.
    std::panic::catch_unwind(|| normalize_inner(bytes, max_pixels)).unwrap_or(Err(()))
}

fn normalize_inner(bytes: &[u8], max_pixels: u64) -> Result<Normalized, ()> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(());
    }
    let format = image::guess_format(bytes).map_err(|_| ())?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return Err(()),
    };
    // Header-only checks precede construction of pixel decoders and all frame buffers.
    let (w, h) = crate::safe_fetch::image_dimensions(bytes).ok_or(())?;
    let pixels = w.checked_mul(h).ok_or(())?;
    if w == 0 || h == 0 || w > 16_384 || h > 16_384 || pixels > max_pixels.min(64_000_000) {
        return Err(());
    }
    let count = match format {
        ImageFormat::Gif => gif_frame_count(bytes, w, h)?,
        ImageFormat::WebP => webp_frame_count(bytes, w, h)?,
        _ => 1,
    };
    let animated = count > 1;
    // Includes full canvas, previous/disposal canvas and conversion/resizing buffers.
    let factor = if matches!(format, ImageFormat::Gif | ImageFormat::WebP) {
        16
    } else {
        12
    };
    // Single-group VP8L tables/cache and its bounded transform-map tables need
    // a fixed allowance independent of the canvas, in addition to pixel storage.
    let entropy_memory = if format == ImageFormat::WebP {
        1024 * 1024
    } else {
        0
    };
    if pixels
        .checked_mul(factor)
        .and_then(|n| n.checked_add(entropy_memory))
        .ok_or(())?
        > WORK_MEMORY
        || pixels.checked_mul(count as u64).ok_or(())? > MAX_FRAME_PIXELS
    {
        return Err(());
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut images = Vec::new();
    if format == ImageFormat::Gif {
        let mut decoder =
            image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).map_err(|_| ())?;
        if decoder.dimensions() != (w as u32, h as u32) {
            return Err(());
        }
        decoder.set_limits(limits).map_err(|_| ())?;
        images = sample_frames(decoder.into_frames(), count)?;
    } else if format == ImageFormat::WebP && webp_is_animated(bytes) {
        let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|_| ())?;
        if decoder.dimensions() != (w as u32, h as u32) {
            return Err(());
        }
        // WebP's frame decoder composites disposal/blending; structural preflight bounds
        // every ANMF rectangle as well as the full canvas before its allocations.
        images = sample_frames(decoder.into_frames(), count)?;
    } else {
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|_| ())?;
        if decoded.width() != w as u32 || decoded.height() != h as u32 {
            return Err(());
        }
        // Decode even pass-through candidates, so a plausible header cannot bypass validation.
        // Animated PNG is flattened rather than accidentally passing an animation onward.
        let apng = format == ImageFormat::Png && bytes.windows(4).any(|b| b == b"acTL");
        if w <= 1024 && h <= 1024 && bytes.len() <= PASS_BYTES && !apng {
            images.push(EncodedImage {
                bytes: bytes.to_vec(),
                mime,
            });
        } else {
            images.push(encode_compact(decoded)?);
        }
    }
    Ok(Normalized {
        images,
        note: animated.then(|| format!("动图，共 {count} 帧，已均匀抽取 {} 帧", count.min(8))),
        original_mime: mime,
    })
}

fn sample_frames(frames: image::Frames<'_>, count: usize) -> Result<Vec<EncodedImage>, ()> {
    let sample_count = count.min(8);
    let selected: Vec<usize> = if sample_count == 1 {
        vec![0]
    } else {
        (0..sample_count)
            .map(|i| i * (count - 1) / (sample_count - 1))
            .collect()
    };
    let mut out = Vec::with_capacity(sample_count);
    let mut actual = 0;
    // Stream every frame (including unselected frames) to apply disposal/composition.
    // Retain only compressed samples, never all decoded frames.
    for frame in frames {
        if actual >= count {
            return Err(());
        }
        let frame = frame.map_err(|_| ())?;
        if selected.contains(&actual) {
            out.push(encode_compact(DynamicImage::ImageRgba8(
                frame.into_buffer(),
            ))?);
        }
        actual += 1;
    }
    if actual != count || out.len() != sample_count {
        return Err(());
    }
    Ok(out)
}

fn encode_compact(mut img: DynamicImage) -> Result<EncodedImage, ()> {
    if img.width().max(img.height()) > 1024 {
        img = img.resize(1024, 1024, image::imageops::FilterType::Triangle);
    }
    loop {
        let rgba = img.to_rgba8();
        let alpha = rgba.pixels().any(|p| p[3] != 255);
        let out = if alpha {
            let mut png = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba.clone())
                .write_to(&mut png, ImageFormat::Png)
                .map_err(|_| ())?;
            let mut webp = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
                .encode(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|_| ())?;
            if webp.len() < png.get_ref().len() {
                EncodedImage {
                    bytes: webp,
                    mime: "image/webp",
                }
            } else {
                EncodedImage {
                    bytes: png.into_inner(),
                    mime: "image/png",
                }
            }
        } else {
            let rgb = img.to_rgb8();
            let mut bytes = Vec::new();
            for quality in [75, 60, 45, 35, 25] {
                bytes.clear();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
                    .encode(
                        &rgb,
                        rgb.width(),
                        rgb.height(),
                        image::ExtendedColorType::Rgb8,
                    )
                    .map_err(|_| ())?;
                if bytes.len() <= MAX_OUTPUT_BYTES {
                    break;
                }
            }
            EncodedImage {
                bytes,
                mime: "image/jpeg",
            }
        };
        if out.bytes.len() <= MAX_OUTPUT_BYTES {
            return Ok(out);
        }
        if img.width() == 1 && img.height() == 1 {
            return Err(());
        }
        // High-entropy transparent images may need a smaller canvas to meet the byte budget.
        img = img.resize(
            (img.width() * 3 / 4).max(1),
            (img.height() * 3 / 4).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
}

fn skip_subblocks(bytes: &[u8], pos: &mut usize) -> Result<(), ()> {
    loop {
        let n = *bytes.get(*pos).ok_or(())? as usize;
        *pos += 1;
        if n == 0 {
            return Ok(());
        }
        *pos = pos.checked_add(n).filter(|p| *p <= bytes.len()).ok_or(())?;
    }
}

fn gif_frame_count(bytes: &[u8], w: u64, h: u64) -> Result<usize, ()> {
    let packed = *bytes.get(10).ok_or(())?;
    let mut pos = 13
        + if packed & 128 != 0 {
            3usize << ((packed & 7) + 1)
        } else {
            0
        };
    let mut count = 0;
    loop {
        let tag = *bytes.get(pos).ok_or(())?;
        pos += 1;
        match tag {
            0x3b => return if count > 0 { Ok(count) } else { Err(()) },
            0x21 => {
                pos += 1;
                skip_subblocks(bytes, &mut pos)?;
            }
            0x2c => {
                let desc = bytes.get(pos..pos + 9).ok_or(())?;
                let n = |i| u16::from_le_bytes([desc[i], desc[i + 1]]) as u64;
                let (fw, fh) = (n(4), n(6));
                if fw == 0 || fh == 0 || n(0) + fw > w || n(2) + fh > h {
                    return Err(());
                }
                pos += 9 + if desc[8] & 128 != 0 {
                    3usize << ((desc[8] & 7) + 1)
                } else {
                    0
                };
                pos += 1; // LZW minimum code size; data subblocks remain compressed here.
                skip_subblocks(bytes, &mut pos)?;
                count += 1;
                if count > MAX_FRAMES || w * h * count as u64 > MAX_FRAME_PIXELS {
                    return Err(());
                }
            }
            _ => return Err(()),
        }
    }
}

fn webp_is_animated(bytes: &[u8]) -> bool {
    bytes.get(12..16) == Some(b"VP8X") && bytes.get(20).map(|b| b & 2 != 0).unwrap_or(false)
}

// image-webp 0.2.4 does not budget entropy tables. Accept only a bounded
// lossless subset: one main Huffman group and constant transform maps. Parsing
// this prefix requires no allocation or pixel decoding. General transform maps
// and meta-Huffman maps are disabled until the dependency can budget them.
fn webp_lossless_preflight(data: &[u8]) -> Result<(), ()> {
    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
    }
    impl Bits<'_> {
        fn read(&mut self, n: usize) -> Result<u32, ()> {
            let mut v = 0;
            for i in 0..n {
                v |=
                    u32::from((*self.data.get(self.pos / 8).ok_or(())? >> (self.pos % 8)) & 1) << i;
                self.pos += 1;
            }
            Ok(v)
        }
        fn cache(&mut self) -> Result<(), ()> {
            if self.read(1)? != 0 && !(1..=11).contains(&self.read(4)?) {
                return Err(());
            }
            Ok(())
        }
        fn constant_map(&mut self) -> Result<(), ()> {
            self.cache()?;
            // Nested image streams have no transforms or meta-Huffman flag.
            // Single literal trees consume zero pixel bits, regardless of map size.
            for _ in 0..5 {
                if self.read(1)? != 1 || self.read(1)? != 0 {
                    return Err(());
                }
                let wide = self.read(1)?;
                self.read(if wide == 0 { 1 } else { 8 })?;
            }
            Ok(())
        }
    }
    if data.len() < 5 || data[0] != 0x2f || data[4] & 0xe0 != 0 {
        return Err(());
    }
    let mut bits = Bits {
        data: &data[5..],
        pos: 0,
    };
    let mut seen = 0u32;
    while bits.read(1)? != 0 {
        let kind = bits.read(2)?;
        if seen & (1 << kind) != 0 {
            return Err(());
        }
        seen |= 1 << kind;
        match kind {
            0 | 1 => {
                bits.read(3)?;
                bits.constant_map()?;
            }
            2 => {}
            3 => {
                bits.read(8)?;
                bits.constant_map()?;
            }
            _ => return Err(()),
        }
    }
    bits.cache()?;
    if bits.read(1)? != 0 {
        return Err(());
    }
    Ok(())
}

fn webp_alpha_preflight(tag: &[u8], data: &[u8]) -> Result<(), ()> {
    // Compressed ALPH also invokes the lossless decoder, without a VP8L header.
    // Refuse it rather than letting a second entropy entry point bypass admission.
    if tag == b"ALPH" && data.first().ok_or(())? & 3 != 0 {
        return Err(());
    }
    Ok(())
}

fn webp_bitstream_dimensions(tag: &[u8], data: &[u8]) -> Result<(u64, u64), ()> {
    match tag {
        b"VP8L" if data.len() >= 5 && data[0] == 0x2f => {
            webp_lossless_preflight(data)?;
            let bits = u32::from_le_bytes(data[1..5].try_into().map_err(|_| ())?);
            Ok((
                u64::from(bits & 0x3fff) + 1,
                u64::from((bits >> 14) & 0x3fff) + 1,
            ))
        }
        b"VP8 " if data.len() >= 10 && data[3..6] == [0x9d, 0x01, 0x2a] => Ok((
            u64::from(u16::from_le_bytes([data[6], data[7]]) & 0x3fff),
            u64::from(u16::from_le_bytes([data[8], data[9]]) & 0x3fff),
        )),
        _ => Err(()),
    }
}

fn webp_frame_bitstream(bytes: &[u8], width: u64, height: u64) -> Result<(), ()> {
    let mut pos = 0usize;
    let mut found = false;
    while pos < bytes.len() {
        let header = bytes.get(pos..pos + 8).ok_or(())?;
        let len = u32::from_le_bytes(header[4..8].try_into().map_err(|_| ())?) as usize;
        let start = pos + 8;
        let end = start
            .checked_add(len)
            .filter(|p| *p <= bytes.len())
            .ok_or(())?;
        webp_alpha_preflight(&header[..4], &bytes[start..end])?;
        if &header[..4] == b"VP8L" || &header[..4] == b"VP8 " {
            if found
                || webp_bitstream_dimensions(&header[..4], &bytes[start..end])? != (width, height)
            {
                return Err(());
            }
            found = true;
        }
        pos = end
            .checked_add(len % 2)
            .filter(|p| *p <= bytes.len())
            .ok_or(())?;
    }
    if found {
        Ok(())
    } else {
        Err(())
    }
}

fn webp_frame_count(bytes: &[u8], w: u64, h: u64) -> Result<usize, ()> {
    if bytes.len() < 12 {
        return Err(());
    }
    let size = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| ())?) as usize;
    if size.checked_add(8) != Some(bytes.len()) {
        return Err(());
    }
    let mut pos = 12;
    let mut count = 0;
    while pos < bytes.len() {
        let header = bytes.get(pos..pos + 8).ok_or(())?;
        let len = u32::from_le_bytes(header[4..8].try_into().map_err(|_| ())?) as usize;
        let start = pos + 8;
        let end = start
            .checked_add(len)
            .filter(|p| *p <= bytes.len())
            .ok_or(())?;
        webp_alpha_preflight(&header[..4], &bytes[start..end])?;
        if &header[..4] == b"ANMF" {
            let d = bytes.get(start..end).filter(|d| d.len() >= 16).ok_or(())?;
            let n = |i| u64::from(d[i]) | (u64::from(d[i + 1]) << 8) | (u64::from(d[i + 2]) << 16);
            if n(0) * 2 + n(6) + 1 > w || n(3) * 2 + n(9) + 1 > h {
                return Err(());
            }
            // VP8/VP8L carries its own dimensions. Check them before a codec can
            // allocate from a forged inner size inconsistent with the ANMF rectangle.
            webp_frame_bitstream(&d[16..], n(6) + 1, n(9) + 1)?;
            count += 1;
            if count > MAX_FRAMES || w * h * count as u64 > MAX_FRAME_PIXELS {
                return Err(());
            }
        } else if &header[..4] == b"VP8L" || &header[..4] == b"VP8 " {
            if webp_bitstream_dimensions(&header[..4], &bytes[start..end])? != (w, h) {
                return Err(());
            }
        }
        pos = end
            .checked_add(len % 2)
            .filter(|p| *p <= bytes.len())
            .ok_or(())?;
    }
    if webp_is_animated(bytes) && count == 0 {
        return Err(());
    }
    Ok(count.max(1))
}

#[cfg(test)]
#[path = "../tests/fixtures/media.rs"]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_lossless_entropy_groups_rejected_before_pixel_decoder() {
        // 1x1 VP8L: no transforms/cache; meta image's sole red/green value is
        // 0xffff, requesting 65,536 groups independent of the canvas size.
        let mut bits = Vec::new();
        let mut put = |value: u32, n: usize| {
            for i in 0..n {
                bits.push(((value >> i) & 1) as u8);
            }
        };
        put(0, 1);
        put(0, 1);
        put(1, 1);
        put(0, 3);
        put(0, 1);
        for value in [255, 255, 0, 0, 0] {
            put(1, 1);
            put(0, 1);
            put(1, 1);
            put(value, 8);
        }
        let mut payload = vec![0x2f, 0, 0, 0, 0];
        for chunk in bits.chunks(8) {
            payload.push(chunk.iter().enumerate().fold(0, |b, (i, v)| b | (v << i)));
        }
        let mut input = b"RIFF".to_vec();
        input.extend((12 + payload.len() as u32 + payload.len() as u32 % 2).to_le_bytes());
        input.extend(b"WEBPVP8L");
        input.extend((payload.len() as u32).to_le_bytes());
        input.extend(&payload);
        if payload.len() % 2 != 0 {
            input.push(0);
        }
        assert_eq!(crate::safe_fetch::image_dimensions(&input), Some((1, 1)));
        // This preflight has no pixel/Huffman allocations. Never run an OOM probe.
        assert!(webp_frame_count(&input, 1, 1).is_err());
        assert!(normalize(&input, 64_000_000).is_err());
        let mut animation = fixtures::webp_animation(1);
        let offset = animation.windows(4).position(|b| b == b"VP8L").unwrap();
        // Same unsafe entropy prefix under a full-canvas ANMF.
        payload[1..5].copy_from_slice(&(3u32 | (3 << 14)).to_le_bytes());
        animation.truncate(offset);
        animation.extend(b"VP8L");
        animation.extend((payload.len() as u32).to_le_bytes());
        animation.extend(&payload);
        if payload.len() % 2 != 0 {
            animation.push(0);
        }
        let anmf = animation.windows(4).position(|b| b == b"ANMF").unwrap();
        let len = (animation.len() - anmf - 8) as u32;
        animation[anmf + 4..anmf + 8].copy_from_slice(&len.to_le_bytes());
        let len = (animation.len() - 8) as u32;
        animation[4..8].copy_from_slice(&len.to_le_bytes());
        assert!(webp_frame_count(&animation, 4, 4).is_err());
        assert!(normalize(&animation, 64_000_000).is_err());
        // Compressed alpha is another lossless entropy entry point, with no VP8L tag.
        assert!(webp_alpha_preflight(b"ALPH", &[1, 0]).is_err());
        assert!(webp_alpha_preflight(b"ALPH", &[0, 0]).is_ok());
        // Predictor map with a non-singleton code is outside the bounded subset.
        assert!(webp_lossless_preflight(&[0x2f, 0, 0, 0, 0, 1, 0]).is_err());
    }
    #[test]
    fn large_png_becomes_compact_jpeg_without_mutating_source() {
        let bytes = fixtures::still(ImageFormat::Png, 4000, 3000, false);
        fixtures::evidence("large.png", &bytes);
        fixtures::evidence(
            "small.png",
            &fixtures::still(ImageFormat::Png, 24, 18, false),
        );
        let original = bytes.clone();
        let result = normalize(&bytes, 64_000_000).unwrap();
        assert_eq!(result.images.len(), 1);
        let out = &result.images[0];
        fixtures::evidence("large.normalized.jpg", &out.bytes);
        let decoded = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1024, 768));
        assert_eq!(out.mime, "image/jpeg");
        assert!(out.bytes.len() <= 96 * 1024);
        assert_eq!(bytes, original);
        println!("large PNG: {} -> {} bytes", bytes.len(), out.bytes.len());
    }

    #[test]
    fn gif_samples_all_composed_frames_evenly_in_order_including_endpoints() {
        let bytes = fixtures::gif(37, 40, 30);
        fixtures::evidence("37.gif", &bytes);
        let result = normalize(&bytes, 64_000_000).unwrap();
        assert_eq!(result.original_mime, "image/gif");
        assert_eq!(
            result.note.as_deref(),
            Some("动图，共 37 帧，已均匀抽取 8 帧")
        );
        assert_eq!(result.images.len(), 8);
        for (out, expected) in result.images.iter().zip([0, 5, 10, 15, 20, 25, 30, 36]) {
            fixtures::evidence(&format!("37.sample-{expected}.jpg"), &out.bytes);
            let img = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
            assert_eq!(img.dimensions(), (40, 30));
            let pixel = img.get_pixel(20, 15).0;
            for (actual, expected) in
                pixel
                    .into_iter()
                    .zip([expected, (expected * 7) % 256, (expected * 13) % 256])
            {
                assert!(
                    (i32::from(actual) - expected).abs() <= 4,
                    "{pixel:?} != frame {expected}"
                );
            }
            assert!(out.bytes.len() < 10_000);
        }
    }

    #[test]
    fn webp_animation_is_sampled_and_single_frame_gif_is_a_still() {
        let bytes = fixtures::webp_animation(9);
        fixtures::evidence("9.webp", &bytes);
        let webp = normalize(&bytes, 64_000_000).unwrap();
        assert_eq!(webp.images.len(), 8);
        assert_eq!(webp.note.as_deref(), Some("动图，共 9 帧，已均匀抽取 8 帧"));
        for (out, index) in webp.images.iter().zip([0, 1, 2, 3, 4, 5, 6, 8]) {
            fixtures::evidence(&format!("9.sample-{index}.jpg"), &out.bytes);
            let frame = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
            assert_eq!(frame.dimensions(), (4, 4));
            assert!(frame.get_pixel(0, 0)[0].abs_diff(index) <= 3);
        }
        let gif = normalize(&fixtures::gif(1, 12, 9), 64_000_000).unwrap();
        assert_eq!(gif.images.len(), 1);
        assert!(gif.note.is_none());
        assert_ne!(gif.images[0].mime, "image/gif");
    }

    #[test]
    fn gif_transparency_previous_and_background_disposal_are_composited() {
        use image::{Rgba, RgbaImage};
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new_with_speed(&mut bytes, 30);
            let red = RgbaImage::from_pixel(32, 24, Rgba([240, 0, 0, 255]));
            let blue = RgbaImage::from_fn(32, 24, |x, _| {
                if x < 16 {
                    Rgba([0, 0, 240, 255])
                } else {
                    Rgba([0, 0, 0, 0])
                }
            });
            let green = RgbaImage::from_fn(32, 24, |x, _| {
                if x >= 16 {
                    Rgba([0, 240, 0, 255])
                } else {
                    Rgba([0, 0, 0, 0])
                }
            });
            let clear = RgbaImage::from_pixel(32, 24, Rgba([0, 0, 0, 0]));
            for frame in [red, blue, green, clear] {
                encoder.encode_frame(image::Frame::new(frame)).unwrap();
            }
        }
        let offsets: Vec<_> = bytes
            .windows(3)
            .enumerate()
            .filter_map(|(i, b)| (b == [0x21, 0xf9, 4]).then_some(i + 3))
            .collect();
        assert_eq!(offsets.len(), 4);
        for (offset, disposal) in offsets.into_iter().zip([1, 3, 2, 1]) {
            bytes[offset] = (bytes[offset] & !0x1c) | (disposal << 2);
        }
        let result = normalize(&bytes, 64_000_000).unwrap();
        assert_eq!(result.images.len(), 4);
        let decoded: Vec<_> = result
            .images
            .iter()
            .map(|i| image::load_from_memory(&i.bytes).unwrap().to_rgba8())
            .collect();
        assert!(decoded[0].get_pixel(24, 12)[0] > 230);
        assert!(decoded[1].get_pixel(8, 12)[2] > 230);
        assert!(
            decoded[1].get_pixel(24, 12)[0] > 230,
            "transparent overlay must retain red"
        );
        assert!(
            decoded[2].get_pixel(8, 12)[0] > 230,
            "previous disposal must restore red"
        );
        assert!(decoded[2].get_pixel(24, 12)[1] > 230);
        assert_eq!(
            decoded[3].get_pixel(8, 12)[3],
            0,
            "background disposal must clear the canvas"
        );
    }

    #[test]
    fn alpha_is_preserved_small_stills_pass_and_large_jpeg_shrinks() {
        for format in [ImageFormat::Png, ImageFormat::WebP] {
            let input = fixtures::still(format, 1400, 1100, true);
            fixtures::evidence(
                if format == ImageFormat::Png {
                    "alpha.png"
                } else {
                    "alpha.webp"
                },
                &input,
            );
            let result = normalize(&input, 64_000_000).unwrap();
            let out = &result.images[0];
            fixtures::evidence(
                if format == ImageFormat::Png {
                    "alpha-png.normalized.webp"
                } else {
                    "alpha-webp.normalized.webp"
                },
                &out.bytes,
            );
            assert!(matches!(out.mime, "image/png" | "image/webp"));
            let img = image::load_from_memory(&out.bytes).unwrap().to_rgba8();
            assert!(img.width() <= 1024 && img.height() <= 1024);
            assert_eq!(img.get_pixel(0, 0)[3], 0);
            assert_eq!(img.get_pixel(img.width() - 1, 0)[3], 180);
            assert!(out.bytes.len() <= 96 * 1024);
            println!(
                "alpha {format:?}: {} -> {} bytes {}",
                input.len(),
                out.bytes.len(),
                out.mime
            );
        }
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let input = fixtures::still(format, 24, 18, false);
            let result = normalize(&input, 64_000_000).unwrap();
            assert_eq!(result.images[0].bytes, input);
            let img = image::load_from_memory(&result.images[0].bytes).unwrap();
            assert_eq!((img.width(), img.height()), (24, 18));
        }
        let input = fixtures::still(ImageFormat::Jpeg, 2200, 1200, false);
        fixtures::evidence("photo.jpg", &input);
        let out = normalize(&input, 64_000_000).unwrap();
        let img = image::load_from_memory(&out.images[0].bytes).unwrap();
        assert!(img.width() <= 1024 && img.height() <= 1024);
    }

    #[test]
    fn corrupt_bombs_and_excessive_frames_fail_without_panicking() {
        fixtures::evidence("many.gif", &fixtures::gif(257, 1, 1));
        fixtures::evidence("corrupt.gif", b"GIF89a corrupt");
        for input in [
            b"GIF89a corrupt".to_vec(),
            b"<html>no image</html>".to_vec(),
            fixtures::gif(257, 1, 1),
        ] {
            assert!(normalize(&input, 64_000_000).is_err());
        }
        let mut huge = fixtures::still(ImageFormat::Png, 2, 2, false);
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(normalize(&huge, 64_000_000).is_err());
        let mut huge_gif = fixtures::gif(2, 2, 2);
        huge_gif[6..10].copy_from_slice(&[255, 255, 255, 255]);
        assert!(normalize(&huge_gif, 64_000_000).is_err());
        assert!(normalize(&fixtures::gif(200, 1024, 1), 64_000_000).is_ok());
        assert!(normalize(
            &fixtures::still(ImageFormat::Png, 1200, 1000, false),
            1_000_000
        )
        .is_err());
        // A late damaged frame invalidates the entire animation, never a partial success.
        let mut truncated = fixtures::gif(9, 8, 8);
        truncated.truncate(truncated.len() - 8);
        assert!(normalize(&truncated, 64_000_000).is_err());
        let mut webp = fixtures::webp_animation(2);
        let vp8l = webp.windows(4).position(|b| b == b"VP8L").unwrap();
        let header = vp8l + 9;
        // Small safe mismatch fixture; the same preflight check rejects bomb-sized dimensions.
        webp[header..header + 4].copy_from_slice(&4u32.to_le_bytes());
        assert!(normalize(&webp, 64_000_000).is_err());
    }
}
