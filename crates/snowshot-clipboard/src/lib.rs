use std::fmt;

use rayon::prelude::*;

const BITMAP_INFO_HEADER_SIZE: usize = 40;
const SHARED_BUFFER_METADATA_SIZE: usize = 8;

#[derive(Debug)]
pub enum ClipboardError {
    Backend(String),
    Image(image::ImageError),
    InvalidDimensions,
    InvalidPayload(String),
    UnsupportedPlatform,
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "clipboard backend failed: {error}"),
            Self::Image(error) => write!(formatter, "failed to decode clipboard image: {error}"),
            Self::InvalidDimensions => write!(formatter, "clipboard image dimensions are invalid"),
            Self::InvalidPayload(error) => write!(formatter, "invalid clipboard payload: {error}"),
            Self::UnsupportedPlatform => {
                write!(
                    formatter,
                    "image clipboard is not supported on this platform"
                )
            }
        }
    }
}

impl std::error::Error for ClipboardError {}

impl From<image::ImageError> for ClipboardError {
    fn from(error: image::ImageError) -> Self {
        Self::Image(error)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RgbaPayload<'a> {
    pixels: &'a [u8],
    width: u32,
    height: u32,
}

impl<'a> RgbaPayload<'a> {
    pub fn pixels(self) -> &'a [u8] {
        self.pixels
    }

    pub fn width(self) -> u32 {
        self.width
    }

    pub fn height(self) -> u32 {
        self.height
    }
}

pub fn parse_shared_buffer_payload(data: &[u8]) -> Result<RgbaPayload<'_>, ClipboardError> {
    let pixels_len = data
        .len()
        .checked_sub(SHARED_BUFFER_METADATA_SIZE)
        .ok_or_else(|| {
            ClipboardError::InvalidPayload(
                "payload is shorter than width/height metadata".to_string(),
            )
        })?;
    let metadata: [u8; SHARED_BUFFER_METADATA_SIZE] =
        data[pixels_len..].try_into().map_err(|_| {
            ClipboardError::InvalidPayload("failed to read width/height metadata".to_string())
        })?;
    let width = u32::from_le_bytes(metadata[0..4].try_into().map_err(|_| {
        ClipboardError::InvalidPayload("failed to read width metadata".to_string())
    })?);
    let height = u32::from_le_bytes(metadata[4..8].try_into().map_err(|_| {
        ClipboardError::InvalidPayload("failed to read height metadata".to_string())
    })?);
    validate_rgba_len(&data[..pixels_len], width, height)?;

    Ok(RgbaPayload {
        pixels: &data[..pixels_len],
        width,
        height,
    })
}

pub fn encode_cf_dib_from_png(image_data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let image =
        image::load_from_memory_with_format(image_data, image::ImageFormat::Png)?.to_rgba8();
    encode_cf_dib(image.as_raw(), image.width(), image.height())
}

pub fn encode_cf_dib(
    rgba_image: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, ClipboardError> {
    validate_rgba_len(rgba_image, width, height)?;

    let width_usize = width as usize;
    let height_usize = height as usize;
    let unpadded_row_size = width_usize
        .checked_mul(3)
        .ok_or(ClipboardError::InvalidDimensions)?;
    let row_size = unpadded_row_size
        .checked_add(3)
        .map(|size| size & !3)
        .ok_or(ClipboardError::InvalidDimensions)?;
    let pixel_data_size = row_size
        .checked_mul(height_usize)
        .ok_or(ClipboardError::InvalidDimensions)?;
    let total_size = BITMAP_INFO_HEADER_SIZE
        .checked_add(pixel_data_size)
        .ok_or(ClipboardError::InvalidDimensions)?;
    let width_i32 = i32::try_from(width).map_err(|_| ClipboardError::InvalidDimensions)?;
    let height_i32 = i32::try_from(height).map_err(|_| ClipboardError::InvalidDimensions)?;
    let pixel_data_size_u32 =
        u32::try_from(pixel_data_size).map_err(|_| ClipboardError::InvalidDimensions)?;

    let mut dib_data = vec![0; total_size];
    write_bitmap_info_header(
        &mut dib_data[..BITMAP_INFO_HEADER_SIZE],
        width_i32,
        height_i32,
        pixel_data_size_u32,
    );

    dib_data[BITMAP_INFO_HEADER_SIZE..]
        .par_chunks_exact_mut(row_size)
        .enumerate()
        .for_each(|(target_y, target_row)| {
            let source_y = height_usize - target_y - 1;
            let source_start = source_y * width_usize * 4;
            let source_end = source_start + width_usize * 4;
            let source_row = &rgba_image[source_start..source_end];

            source_row
                .chunks_exact(4)
                .zip(target_row[..unpadded_row_size].chunks_exact_mut(3))
                .for_each(|(rgba, bgr)| {
                    bgr.copy_from_slice(&[rgba[2], rgba[1], rgba[0]]);
                });
        });

    Ok(dib_data)
}

pub fn write_png_image(image_data: &[u8]) -> Result<(), ClipboardError> {
    let dib_data = encode_cf_dib_from_png(image_data)?;
    write_cf_dib(&dib_data)
}

pub fn write_rgba_image(rgba_image: &[u8], width: u32, height: u32) -> Result<(), ClipboardError> {
    let dib_data = encode_cf_dib(rgba_image, width, height)?;
    write_cf_dib(&dib_data)
}

pub fn write_shared_buffer_payload(data: &[u8]) -> Result<(), ClipboardError> {
    let payload = parse_shared_buffer_payload(data)?;
    write_rgba_image(payload.pixels(), payload.width(), payload.height())
}

fn validate_rgba_len(rgba_image: &[u8], width: u32, height: u32) -> Result<(), ClipboardError> {
    if width == 0 || height == 0 {
        return Err(ClipboardError::InvalidDimensions);
    }

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixel_count| pixel_count.checked_mul(4))
        .ok_or(ClipboardError::InvalidDimensions)?;
    if rgba_image.len() != expected_len {
        return Err(ClipboardError::InvalidPayload(format!(
            "RGBA buffer length {} does not match {width}x{height} image",
            rgba_image.len()
        )));
    }

    Ok(())
}

fn write_bitmap_info_header(header: &mut [u8], width: i32, height: i32, pixel_data_size: u32) {
    header[0..4].copy_from_slice(&(BITMAP_INFO_HEADER_SIZE as u32).to_le_bytes());
    header[4..8].copy_from_slice(&width.to_le_bytes());
    header[8..12].copy_from_slice(&height.to_le_bytes());
    header[12..14].copy_from_slice(&1_u16.to_le_bytes());
    header[14..16].copy_from_slice(&24_u16.to_le_bytes());
    header[16..20].copy_from_slice(&0_u32.to_le_bytes());
    header[20..24].copy_from_slice(&pixel_data_size.to_le_bytes());
    header[24..28].copy_from_slice(&0_i32.to_le_bytes());
    header[28..32].copy_from_slice(&0_i32.to_le_bytes());
    header[32..36].copy_from_slice(&0_u32.to_le_bytes());
    header[36..40].copy_from_slice(&0_u32.to_le_bytes());
}

#[cfg(target_os = "windows")]
fn write_cf_dib(dib_data: &[u8]) -> Result<(), ClipboardError> {
    use clipboard_win::{Setter, formats};

    let _clipboard = clipboard_win::Clipboard::new()
        .map_err(|error| ClipboardError::Backend(format!("failed to open clipboard: {error}")))?;
    formats::RawData(formats::CF_DIB)
        .write_clipboard(&dib_data)
        .map_err(|error| ClipboardError::Backend(format!("failed to write CF_DIB: {error}")))
}

#[cfg(not(target_os = "windows"))]
fn write_cf_dib(_dib_data: &[u8]) -> Result<(), ClipboardError> {
    Err(ClipboardError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn manual_header_matches_windows_bitmap_info_header_size() {
        assert_eq!(
            BITMAP_INFO_HEADER_SIZE,
            std::mem::size_of::<clipboard_win::types::BITMAPINFOHEADER>()
        );
    }

    #[test]
    fn encodes_bottom_up_bgr_rows_with_padding() {
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, // top: red, green
            0, 0, 255, 255, 255, 255, 255, 255, // bottom: blue, white
        ];
        let dib = encode_cf_dib(&rgba, 2, 2).unwrap();

        assert_eq!(dib.len(), 56);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 24);
        assert_eq!(u32::from_le_bytes(dib[20..24].try_into().unwrap()), 16);
        assert_eq!(
            &dib[40..],
            &[
                255, 0, 0, 255, 255, 255, 0, 0, // bottom row
                0, 0, 255, 0, 255, 0, 0, 0, // top row
            ]
        );
    }

    #[test]
    fn decodes_png_before_dib_encoding() {
        let image = image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 255]).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        let dib = encode_cf_dib_from_png(png.get_ref()).unwrap();

        assert_eq!(&dib[40..], &[3, 2, 1, 0]);
    }

    #[test]
    fn parses_valid_shared_buffer_payload() {
        let mut data = vec![1, 2, 3, 4];
        data.extend_from_slice(&1_u32.to_le_bytes());
        data.extend_from_slice(&1_u32.to_le_bytes());

        let payload = parse_shared_buffer_payload(&data).unwrap();

        assert_eq!(payload.pixels(), &[1, 2, 3, 4]);
        assert_eq!(payload.width(), 1);
        assert_eq!(payload.height(), 1);
    }

    #[test]
    fn rejects_short_shared_buffer_payload() {
        let error = parse_shared_buffer_payload(&[0; 7]).unwrap_err();

        assert!(error.to_string().contains("shorter"));
    }

    #[test]
    fn rejects_mismatched_rgba_length() {
        let error = encode_cf_dib(&[0; 3], 1, 1).unwrap_err();

        assert!(error.to_string().contains("does not match"));
    }
}
