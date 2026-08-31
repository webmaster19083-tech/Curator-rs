use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::warn;

const THUMB_MAX_DIM: u32 = 360;

pub async fn get_or_create_thumb(
    media_id:  i64,
    src:       PathBuf,
    thumbs_dir: PathBuf,
) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        get_or_create_thumb_sync(media_id, &src, &thumbs_dir)
    })
    .await?
}

pub fn get_or_create_thumb_sync(
    media_id:  i64,
    src:       &Path,
    thumbs_dir: &Path,
) -> Result<Vec<u8>> {
    // Use dunce::simplified to handle Windows long paths (\\?\ prefix)
    let src       = dunce::simplified(src).to_path_buf();
    let thumb_path = dunce::simplified(&thumbs_dir.join(format!("{}.jpg", media_id))).to_path_buf();

    if thumb_path.exists() {
        return Ok(std::fs::read(&thumb_path)?);
    }

    let img = image::open(&src)?;
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    let rgb   = thumb.to_rgb8();

    let mut buf = std::io::Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82);
    encoder.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ColorType::Rgb8.into(),
    )?;

    let bytes = buf.into_inner();
    if let Err(e) = std::fs::write(&thumb_path, &bytes) {
        warn!("Could not write thumbnail for media {}: {}", media_id, e);
        // Return the bytes anyway — don't fail the request
    }

    Ok(bytes)
}
