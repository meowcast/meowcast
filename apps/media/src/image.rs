//! Pictures for an app's UI, whatever format a site sends: the format is read from the first
//! bytes of the file, never from its name or the server's word.
//!
//! - PNG is decoded here ([`crate::png`]).
//! - WebP (lossy, lossless, with transparency; the first frame of an animation) and JPEG go
//!   to the system image service ([`meow_app::image`]): the decoders are a store package
//!   that the system loads when an app first needs one, so no app and no firmware carries
//!   them, and a file from the internet is parsed in a sandbox that has nothing in it.
//!
//! ```no_run
//! # let bytes: &[u8] = &[];
//! let mut decoder = meow_media::image::Decoder::new();     // one per thread that decodes
//! let image = decoder.decode(bytes, 96)?;                  // at most 96 pixels a side
//! let tile = image.resized(96, 96).premultiplied();        // 0xAARRGGBB, as the canvas blends
//! # Ok::<(), String>(())
//! ```
//!
//! Without the service (an older firmware, a store that cannot be reached) WebP and JPEG
//! files are errors and PNG files still decode: keep a PNG address as the fallback where a
//! site has one. Call `decode` from a thread other than the one that runs `App::wait`.

pub use crate::png::Image;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Png,
    Webp,
    Jpeg,
}

/// What kind of file this is, by its first bytes.
pub fn format(data: &[u8]) -> Option<Format> {
    match data {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(Format::Png),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Format::Webp),
        [0xff, 0xd8, 0xff, ..] => Some(Format::Jpeg),
        _ => None,
    }
}

/// Decodes pictures; holds the connection to the image service once one was needed.
#[derive(Default)]
pub struct Decoder {
    service: meow_app::image::Decoder,
}

impl Decoder {
    /// Nothing is loaded or connected until a file needs it.
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// Decode a PNG, WebP or JPEG file to straight RGBA. A picture with a side longer than
    /// `fit` pixels is made smaller to fit (its shape kept; 0 = as it is): ask for the size
    /// it is shown at, and a large file costs no more memory than a small one.
    pub fn decode(&mut self, data: &[u8], fit: usize) -> Result<Image, String> {
        match format(data) {
            Some(Format::Png) => Ok(crate::png::decode(data)?.fitted(fit)),
            Some(Format::Webp | Format::Jpeg) => {
                let side = fit.min(u16::MAX as usize) as u16;
                let pixels = self.service.decode(data, (side, side))?;
                Ok(Image { width: pixels.width, height: pixels.height, rgba: pixels.rgba })
            }
            None => Err("not a PNG, WebP or JPEG file".to_string()),
        }
    }

    /// False while the image service is known to be out of reach (it is tried again after
    /// half a minute): only PNG files decode then.
    pub fn service_available(&self) -> bool {
        self.service.available()
    }

    /// Close the connection to the image service (the next file that needs it connects
    /// again). The service leaves the device's memory when no app is connected, so an app
    /// that is done with pictures for now calls this.
    pub fn release(&mut self) {
        self.service = meow_app::image::Decoder::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One made-up badge in three formats (imagesvc/testdata/make.sh): a 96-pixel PNG (8-bit palette) ...
    const PNG: &[u8] = include_bytes!("../../imagesvc/testdata/logo-96.png");
    // ... a 96-pixel JPEG and a WebP original, which need the image service.
    const JPEG: &[u8] = include_bytes!("../../imagesvc/testdata/logo-96.jpg");
    const WEBP: &[u8] = include_bytes!("../../imagesvc/testdata/logo.webp");

    #[test]
    fn formats_are_told_by_their_first_bytes() {
        assert_eq!((format(PNG), format(JPEG), format(WEBP)), (Some(Format::Png), Some(Format::Jpeg), Some(Format::Webp)));
        for other in [&b""[..], b"RIFF", b"RIFF\0\0\0\0WAVE", b"<html>", b"GIF89a", &[0xff, 0xd8]] {
            assert_eq!(format(other), None);
        }
        assert!(Decoder::new().decode(b"<html>", 96).unwrap_err().contains("not a PNG"));
    }

    #[test]
    fn a_png_needs_no_service() {
        let mut decoder = Decoder::new();
        let image = decoder.decode(PNG, 0).unwrap();
        assert_eq!((image.width, image.height, image.rgba.len()), (96, 96, 96 * 96 * 4));
        // Made to fit, its shape kept, never larger.
        let small = decoder.decode(PNG, 48).unwrap();
        assert_eq!((small.width, small.height), (48, 48));
        assert_eq!(decoder.decode(PNG, 300).unwrap().width, 96);
        let wide = Image { width: 200, height: 50, rgba: vec![255; 200 * 50 * 4] }.fitted(96);
        assert_eq!((wide.width, wide.height), (96, 24));
        assert!(decoder.service_available());
    }

    /// With the service's program at hand (MEOW_IMAGE_HELPER, as on the build host), WebP
    /// and JPEG decode through it on one connection; without it they are errors and the
    /// decoder says the service is away. Both are what an app must cope with.
    #[test]
    fn webp_and_jpeg_go_to_the_service() {
        let mut decoder = Decoder::new();
        let helper = std::env::var("MEOW_IMAGE_HELPER").is_ok();
        match decoder.decode(WEBP, 96) {
            Ok(image) => {
                assert!(helper);
                assert_eq!((image.width, image.height, image.rgba.len()), (96, 96, 96 * 96 * 4));
                let whole = decoder.decode(WEBP, 0).unwrap();
                assert_eq!((whole.width, whole.height), (300, 300));
                assert_eq!(decoder.decode(JPEG, 96).unwrap().width, 96);
                // A file the service refuses does not end the connection.
                assert!(decoder.decode(&WEBP[..100], 96).is_err());
                assert!(decoder.service_available());
                assert_eq!(decoder.decode(JPEG, 32).unwrap().width, 32);
                decoder.release();
                assert_eq!(decoder.decode(JPEG, 32).unwrap().height, 32);
            }
            Err(why) => {
                assert!(!helper, "{why}");
                assert!(why.starts_with("no image service"), "{why}");
                assert!(!decoder.service_available());
                assert!(decoder.decode(JPEG, 96).is_err());
                // PNG still works.
                assert_eq!(decoder.decode(PNG, 96).unwrap().width, 96);
            }
        }
    }
}
