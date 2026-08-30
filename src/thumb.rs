use std::{
    io::Cursor,
    path::{Path, PathBuf},
};
use anyhow::Result;
use image::{ImageFormat, imageops::FilterType};

const THUMB_MAX_DIM: u32 = 360;

/// Get cached thumbnail bytes, generating if missing.
/// Runs in spawn_blocking — do not call from async context directly.
pub fn get_or_create_thumb(media_id: i64, src: PathBuf, thumbs_dir: PathBuf) -> Result<Vec<u8>> {
    let src = dunce::simplified(&src).to_path_buf();
    let thumbs_dir = dunce::simplified(&thumbs_dir).to_path_buf();
    let thumb_path = thumbs_dir.join(format!("{media_id}.jpg"));

    if thumb_path.exists() {
        return Ok(std::fs::read(&thumb_path)?);
    }

    std::fs::create_dir_all(&thumbs_dir)?;

    let img = image::open(&src)?;
    // thumbnail() maintains aspect ratio, fits within NxN box
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    // Force RGB (no alpha) for JPEG
    let rgb = thumb.to_rgb8();

    let mut buf = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)?;

    // Write cache — ignore errors (next request will just regenerate)
    let _ = std::fs::write(&thumb_path, &buf);

    Ok(buf)
}
