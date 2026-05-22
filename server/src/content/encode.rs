use image::error::{EncodingError, ImageFormatHint};
use image::{DynamicImage, ImageError, ImageResult};
use jpegxl_rs::{encode::EncoderFrame, encoder_builder};

/// Encodes `image` as a JPEG XL byte stream.
///
/// The image is converted to RGB8 before encoding (transparency is composited
/// onto white by the thumbnail pipeline upstream, so this is lossless for
/// thumbnails). Quality defaults to the encoder's built-in setting.
pub fn to_jxl(image: &DynamicImage) -> ImageResult<Vec<u8>> {
    let rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();

    let mut encoder = encoder_builder()
        .build()
        .map_err(|e| ImageError::Encoding(EncodingError::new(ImageFormatHint::Unknown, Box::new(e))))?;

    let frame = EncoderFrame::new(rgb.as_raw()).num_channels(3);
    encoder
        .encode_frame::<u8, u8>(&frame, width, height)
        .map(|output| { let b: &[u8] = output.as_ref(); b.to_vec() })
        .map_err(|e| ImageError::Encoding(EncodingError::new(ImageFormatHint::Unknown, Box::new(e))))
}
