//! Lossy WebP through libwebp (vendored C, statically linked by `libwebp-sys`).
//!
//! There's no pure-Rust lossy WebP encoder under a licence roll can ship (`image-webp` only
//! encodes lossless, which is several times larger for photos; `zenwebp` is AGPL), so this is
//! the one C dependency of the engine. Its surface is small: the advanced encoding API on a
//! buffer roll owns, single-threaded (`thread_level = 0`), so the output is deterministic.

use crate::ImageError;
use libwebp_sys as w;
use std::mem::MaybeUninit;

/// libwebp's `method` (0 fastest … 6 smallest). 4 is libwebp's default; measured on this
/// engine's typical sizes it costs ~1.5× method 2 for ~5% smaller files, worth it for images
/// that are cached and served many times.
const METHOD: i32 = 4;

/// Encode 8-bit RGB (`channels` = 3) or RGBA (4) pixels.
pub(crate) fn encode(pixels: &[u8], width: u32, height: u32, channels: usize, quality: u8) -> Result<Vec<u8>, ImageError> {
    let fail = |what: &str| ImageError::Internal(format!("WebP encoding failed: {what}"));
    if width == 0 || height == 0 || width > 16383 || height > 16383 || pixels.len() < width as usize * height as usize * channels {
        return Err(fail("bad dimensions"));
    }
    // SAFETY: every libwebp struct is initialised by its own init function before use; the
    // picture imports (copies) from `pixels`, which outlives the calls and holds at least
    // width × height × channels bytes (checked above); the writer's buffer is copied out and
    // then freed with `WebPMemoryWriterClear`, and the picture with `WebPPictureFree`, on every
    // path.
    unsafe {
        let mut config = MaybeUninit::<w::WebPConfig>::zeroed();
        if w::WebPConfigInitInternal(config.as_mut_ptr(), w::WebPPreset::WEBP_PRESET_DEFAULT, quality as f32, w::WEBP_ENCODER_ABI_VERSION as i32) == 0 {
            return Err(fail("libwebp version mismatch"));
        }
        let mut config = config.assume_init();
        config.method = METHOD;
        config.thread_level = 0;
        if w::WebPValidateConfig(&config) == 0 {
            return Err(fail("invalid configuration"));
        }
        let mut pic = MaybeUninit::<w::WebPPicture>::zeroed();
        if w::WebPPictureInitInternal(pic.as_mut_ptr(), w::WEBP_ENCODER_ABI_VERSION as i32) == 0 {
            return Err(fail("libwebp version mismatch"));
        }
        let mut pic = pic.assume_init();
        pic.width = width as i32;
        pic.height = height as i32;
        let stride = (width as usize * channels) as i32;
        let imported = if channels == 4 { w::WebPPictureImportRGBA(&mut pic, pixels.as_ptr(), stride) } else { w::WebPPictureImportRGB(&mut pic, pixels.as_ptr(), stride) };
        if imported == 0 {
            w::WebPPictureFree(&mut pic);
            return Err(fail("out of memory"));
        }
        let mut writer = MaybeUninit::<w::WebPMemoryWriter>::zeroed();
        w::WebPMemoryWriterInit(writer.as_mut_ptr());
        let mut writer = writer.assume_init();
        pic.writer = Some(w::WebPMemoryWrite);
        pic.custom_ptr = (&mut writer as *mut w::WebPMemoryWriter).cast();
        let ok = w::WebPEncode(&config, &mut pic);
        let out = if ok != 0 && !writer.mem.is_null() { Ok(std::slice::from_raw_parts(writer.mem, writer.size).to_vec()) } else { Err(fail(&format!("error code {:?}", pic.error_code))) };
        w::WebPMemoryWriterClear(&mut writer);
        w::WebPPictureFree(&mut pic);
        out
    }
}
