use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};
use image::{ImageFormat, ImageReader};

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Reads a cover image from disk and optionally converts non-PNG images to PNG bytes.
/// Returns `(file_bytes, target_extension_without_dot)`.
pub fn prepare_cover_bytes(source_path: &Path, convert_to_png: bool) -> Result<(Vec<u8>, String)> {
    let raw_bytes = fs::read(source_path)
        .with_context(|| format!("No se pudo leer la imagen: {}", source_path.display()))?;

    let src_ext = source_path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string());

    if !convert_to_png {
        return Ok((raw_bytes, src_ext));
    }

    // Fast path: already a valid PNG file on disk, skip decode/encode overhead.
    if src_ext == "png" && raw_bytes.starts_with(PNG_MAGIC) {
        return Ok((raw_bytes, "png".to_string()));
    }

    let img = ImageReader::new(Cursor::new(&raw_bytes))
        .with_guessed_format()
        .with_context(|| format!("No se pudo detectar formato de: {}", source_path.display()))?
        .decode()
        .with_context(|| format!("No se pudo decodificar imagen: {}", source_path.display()))?;

    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .with_context(|| format!("No se pudo convertir a PNG: {}", source_path.display()))?;

    Ok((out.into_inner(), "png".to_string()))
}

/// Decodes and resizes an image for fast preview in the GUI, returning `(width, height, rgba8)`.
pub fn load_thumbnail_rgba(source_path: &Path, max_dim: u32) -> Result<(usize, usize, Vec<u8>)> {
    let img = ImageReader::open(source_path)
        .with_context(|| format!("No se pudo abrir miniatura: {}", source_path.display()))?
        .with_guessed_format()
        .with_context(|| format!("Formato desconocido en miniatura: {}", source_path.display()))?
        .decode()
        .with_context(|| format!("Error al decodificar miniatura: {}", source_path.display()))?;

    let thumb = img.thumbnail(max_dim, max_dim);
    let rgba = thumb.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((w as usize, h as usize, rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, RgbImage};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn png_passthrough_keeps_exact_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cover.png");
        let mut fake_png = PNG_MAGIC.to_vec();
        fake_png.extend_from_slice(b"custom-chunk-data");
        fs::write(&path, &fake_png).unwrap();

        let (bytes, ext) = prepare_cover_bytes(&path, true).unwrap();
        assert_eq!(ext, "png");
        assert_eq!(bytes, fake_png);
    }

    #[test]
    fn jpeg_converts_to_valid_png_bytes() {
        let dir = tempdir().unwrap();
        let jpg_path = dir.path().join("cover.jpg");
        let rgb = RgbImage::from_fn(16, 16, |x, y| image::Rgb([(x * 10) as u8, (y * 10) as u8, 128]));
        DynamicImage::ImageRgb8(rgb)
            .save_with_format(&jpg_path, ImageFormat::Jpeg)
            .unwrap();

        let (bytes, ext) = prepare_cover_bytes(&jpg_path, true).unwrap();
        assert_eq!(ext, "png");
        assert!(bytes.starts_with(PNG_MAGIC));

        let decoded = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!(decoded.width(), 16);
        assert_eq!(decoded.height(), 16);
    }
}
