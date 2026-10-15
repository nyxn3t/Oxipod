//! Preparing a picture from the computer to be used as a cover on the iPod.

use std::path::Path;

use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
use gtk::glib;

/// The longest side of a cover, in pixels. The iPod shows covers tiny, so this
/// is plenty, and a small picture keeps the iPod's artwork files small.
const MAX_SIDE: i32 = 320;

/// Reads the picture at `path` and returns it as a JPEG of at most 320 × 320
/// pixels, the form libgpod wants for a cover.
///
/// A `Pixbuf` is gdk-pixbuf's name for a picture in memory. The `?` after a
/// call returns its error from this function at once.
pub fn jpeg_from_file(path: &Path) -> Result<Vec<u8>, glib::Error> {
    // Ask for the size before loading, so a small picture isn't blown up
    // (which would only make it blurry). Bigger ones are shrunk while loading,
    // keeping their proportions.
    let (_, width, height) = Pixbuf::file_info(path)
        .ok_or_else(|| glib::Error::new(glib::FileError::Inval, "this isn't a picture the app can read"))?;
    let picture = if width.max(height) > MAX_SIDE {
        Pixbuf::from_file_at_scale(path, MAX_SIDE, MAX_SIDE, true)?
    } else {
        Pixbuf::from_file(path)?
    };
    // Pictures from a phone can carry a note saying "turn me": apply it.
    let picture = picture.apply_embedded_orientation().unwrap_or(picture);
    // A JPEG can't be see-through: a picture with transparent parts goes on white.
    let picture = if picture.has_alpha() { on_white(&picture) } else { picture };
    picture.save_to_bufferv("jpeg", &[("quality", "90")])
}

/// The picture on a white background, without a transparent channel.
fn on_white(picture: &Pixbuf) -> Pixbuf {
    let (width, height) = (picture.width(), picture.height());
    let canvas = Pixbuf::new(Colorspace::Rgb, false, 8, width, height).expect("a cover fits in memory");
    canvas.fill(0xffffffff);
    picture.composite(&canvas, 0, 0, width, height, 0.0, 0.0, 1.0, 1.0, InterpType::Nearest, 255);
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saves a plain picture of the given size as a PNG and returns its path.
    fn picture(folder: &Path, name: &str, width: i32, height: i32, alpha: bool) -> std::path::PathBuf {
        let pixbuf = Pixbuf::new(Colorspace::Rgb, alpha, 8, width, height).unwrap();
        // Orange, or fully see-through when there is an alpha channel.
        pixbuf.fill(if alpha { 0xed815000 } else { 0xed8150ff });
        let path = folder.join(name);
        pixbuf.savev(&path, "png", &[]).unwrap();
        path
    }

    /// Writes the JPEG next to the others and reads its size back.
    fn size_of(folder: &Path, jpeg: &[u8]) -> (i32, i32) {
        let path = folder.join("result.jpg");
        std::fs::write(&path, jpeg).unwrap();
        let (format, width, height) = Pixbuf::file_info(&path).expect("the result is a picture");
        assert_eq!(format.name().as_deref(), Some("jpeg"));
        (width, height)
    }

    #[test]
    fn shrinks_a_big_picture_and_keeps_its_proportions() {
        let folder = tempfile::tempdir().unwrap();
        let path = picture(folder.path(), "big.png", 1000, 600, false);
        let jpeg = jpeg_from_file(&path).unwrap();
        assert_eq!(size_of(folder.path(), &jpeg), (320, 192));
    }

    #[test]
    fn leaves_a_small_picture_alone() {
        let folder = tempfile::tempdir().unwrap();
        let path = picture(folder.path(), "small.png", 100, 80, false);
        let jpeg = jpeg_from_file(&path).unwrap();
        assert_eq!(size_of(folder.path(), &jpeg), (100, 80));
    }

    #[test]
    fn puts_a_see_through_picture_on_white() {
        let folder = tempfile::tempdir().unwrap();
        let path = picture(folder.path(), "ghost.png", 64, 64, true);
        let jpeg = jpeg_from_file(&path).unwrap();
        let result = folder.path().join("result.jpg");
        std::fs::write(&result, &jpeg).unwrap();
        let decoded = Pixbuf::from_file(&result).unwrap();
        assert!(!decoded.has_alpha());
        // The first pixel (red, green, blue) is white, give or take JPEG noise.
        let pixel = &decoded.read_pixel_bytes()[..3];
        assert!(pixel.iter().all(|&channel| channel > 240), "{pixel:?}");
    }

    #[test]
    fn says_so_when_the_file_is_not_a_picture() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("notes.txt");
        std::fs::write(&path, "just some text").unwrap();
        assert!(jpeg_from_file(&path).is_err());
        assert!(jpeg_from_file(&folder.path().join("missing.png")).is_err());
    }
}
