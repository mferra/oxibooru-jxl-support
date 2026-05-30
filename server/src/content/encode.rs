use image::error::{EncodingError, ImageFormatHint};
use image::{DynamicImage, ImageError, ImageResult};
use jpegxl_rs::{encode::EncoderFrame, encoder_builder};

/// Encodes `image` as a JPEG XL byte stream at the given `quality` (0–100).
/// Alpha is preserved: images with an alpha channel are encoded as 4-channel RGBA;
/// opaque images are encoded as 3-channel RGB.
pub fn to_jxl(image: &DynamicImage, quality: f32) -> ImageResult<Vec<u8>> {
    let make_err =
        |e| ImageError::Encoding(EncodingError::new(ImageFormatHint::Unknown, Box::new(e)));

    if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        let mut encoder = encoder_builder().quality(quality).build().map_err(make_err)?;
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
        let mut encoder = encoder_builder().quality(quality).build().map_err(make_err)?;
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
