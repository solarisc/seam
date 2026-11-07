//! Conversion between the native clipboard image and its wire representation.
use ::image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage, codecs::png::PngEncoder};
use anyhow::Result;
use clipboard_rs::{RustImageData, common::RustImage};

pub(super) struct PreparedImage {
    pixels: RgbaImage,
    pub identity: [u8; 32],
}

impl PreparedImage {
    pub fn from_native(native: RustImageData) -> Result<Self> {
        let pixels = native
            .to_rgba8()
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        Ok(Self::from_pixels(pixels))
    }

    pub fn from_wire(encoded: Vec<u8>) -> Result<Self> {
        let mut reader =
            ::image::ImageReader::new(std::io::Cursor::new(encoded)).with_guessed_format()?;
        let mut limits = ::image::Limits::default();
        limits.max_alloc = Some(256 * 1024 * 1024);
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        reader.limits(limits);
        Ok(Self::from_pixels(reader.decode()?.into_rgba8()))
    }

    fn from_pixels(pixels: RgbaImage) -> Self {
        // PNG bytes can differ after a native clipboard readback. Compare the
        // image itself before paying to encode an update we would discard.
        let mut hash = blake3::Hasher::new();
        hash.update(b"seam.clipboard.rgba8\0");
        hash.update(&pixels.width().to_le_bytes());
        hash.update(&pixels.height().to_le_bytes());
        hash.update(pixels.as_raw());
        Self {
            identity: *hash.finalize().as_bytes(),
            pixels,
        }
    }

    pub fn into_wire(self) -> Result<Vec<u8>> {
        let mut encoded = Vec::new();
        PngEncoder::new(&mut encoded).write_image(
            self.pixels.as_raw(),
            self.pixels.width(),
            self.pixels.height(),
            ExtendedColorType::Rgba8,
        )?;
        Ok(encoded)
    }

    pub fn into_native(self) -> RustImageData {
        RustImageData::from_dynamic_image(DynamicImage::ImageRgba8(self.pixels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{
        ExtendedColorType, ImageEncoder,
        codecs::png::{CompressionType, FilterType, PngEncoder},
    };

    #[test]
    fn received_image_identity_survives_native_reencoding() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0];
        let mut wire = Vec::new();
        PngEncoder::new_with_quality(&mut wire, CompressionType::Best, FilterType::Sub)
            .write_image(&pixels, 3, 1, ExtendedColorType::Rgba8)
            .unwrap();
        let received = PreparedImage::from_wire(wire).unwrap();
        let identity = received.identity;
        let captured = PreparedImage::from_native(received.into_native()).unwrap();
        assert_eq!(
            captured.identity, identity,
            "a native readback of the peer's image must not become a new clipboard update"
        );
        let round_trip = PreparedImage::from_wire(captured.into_wire().unwrap()).unwrap();
        let native = round_trip.into_native();
        assert_eq!(native.get_size(), (3, 1));
        assert_eq!(native.to_rgba8().unwrap().as_raw(), &pixels);

        // A differently shaped image or an alpha-only change is a new copy,
        // even when all its other bytes are identical.
        let reshaped = RustImageData::from_dynamic_image(DynamicImage::ImageRgba8(
            RgbaImage::from_raw(1, 3, pixels.to_vec()).unwrap(),
        ));
        assert_ne!(
            PreparedImage::from_native(reshaped).unwrap().identity,
            identity
        );
        let mut changed = pixels;
        changed[7] = 127;
        let changed = RustImageData::from_dynamic_image(DynamicImage::ImageRgba8(
            RgbaImage::from_raw(3, 1, changed.to_vec()).unwrap(),
        ));
        assert_ne!(
            PreparedImage::from_native(changed).unwrap().identity,
            identity
        );
    }
}
