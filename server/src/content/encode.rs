use image::error::{EncodingError, ImageFormatHint};
use image::{DynamicImage, ImageError, ImageResult};
use jpegxl_rs::{encode::EncoderFrame, encoder_builder};

/// Converts a JPEG-style quality (0–100, higher = better) to a libjxl butteraugli distance
/// (0.0–25.0, lower = better). This mirrors the formula used by `JxlEncoderDistanceFromQuality`
/// in libjxl. The `.quality()` builder method on jpegxl_rs expects the distance, not the
/// 0–100 scale, so passing the raw quality value causes JXL_ENC_ERR_API_USAGE (valid range
/// for distance is [0, 25]).
fn jpeg_quality_to_distance(quality: f32) -> f32 {
    if quality >= 30.0 {
        0.1 + (100.0 - quality) * 0.09
    } else {
        53.0 / 3000.0 * quality * quality - 23.0 / 20.0 * quality + 25.0
    }
}

/// Encodes `image` as a JPEG XL byte stream at the given `quality` (0–100, higher = better).
/// Alpha is preserved: images with an alpha channel are encoded as 4-channel RGBA;
/// opaque images are encoded as 3-channel RGB.
pub fn to_jxl(image: &DynamicImage, quality: f32) -> ImageResult<Vec<u8>> {
    let make_err =
        |e| ImageError::Encoding(EncodingError::new(ImageFormatHint::Unknown, Box::new(e)));

    let distance = jpeg_quality_to_distance(quality);

    if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        let mut encoder = encoder_builder().has_alpha(true).quality(distance).build().map_err(make_err)?;
        let frame = EncoderFrame::new(rgba.as_raw()).num_channels(4);
        encoder
            .encode_frame::<u8, u8>(&frame, width, height)
            .map(|output| {
                let b: &[u8] = output.as_ref();
                b.to_vec()
            })
            .map_err(make_err)
    } else {
        let rgb = image.to_rgb8();
        let (width, height) = rgb.dimensions();
        let mut encoder = encoder_builder().quality(distance).build().map_err(make_err)?;
        let frame = EncoderFrame::new(rgb.as_raw()).num_channels(3);
        encoder
            .encode_frame::<u8, u8>(&frame, width, height)
            .map(|output| {
                let b: &[u8] = output.as_ref();
                b.to_vec()
            })
            .map_err(make_err)
    }
}
