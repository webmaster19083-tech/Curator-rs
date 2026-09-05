use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Result;
use once_cell::sync::Lazy;
use tracing::warn;

const THUMB_MAX_DIM: u32 = 360;
static THUMB_LOCKS: Lazy<Vec<Mutex<()>>> = Lazy::new(|| (0..64).map(|_| Mutex::new(())).collect());
static THUMB_WORK: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

// ─── Negative cache for undecodable/missing source files ─────────────────────
//
// (size, mtime) of the source file at the moment we failed on it, keyed by
// media_id. `None` means the file didn't exist / its metadata couldn't be
// read at all.
//
// Without this, a broken thumbnail (truncated download, unsupported/corrupt
// file, or a placeholder row pointing at a file that was never written) pays
// the full cost of a decode attempt — or at minimum a failed filesystem
// call — on *every single request* for it: every grid scroll, every page
// load, every poll. For a file that's permanently broken that's an unbounded
// amount of repeated wasted work for a result that will never change.
//
// Keying on the fingerprint rather than just media_id means a genuine fix —
// re-syncing the source, replacing the file — is picked up automatically
// the moment the file's size or mtime changes, with no explicit
// invalidation needed. The cache itself is in-memory and per-process, so it
// also resets naturally on restart.
type Fingerprint = Option<(u64, Option<SystemTime>)>;

static FAILED_THUMBS: Lazy<Mutex<HashMap<i64, (Fingerprint, String)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn fingerprint(path: &Path) -> Fingerprint {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

pub async fn get_or_create_thumb(
    media_id: i64,
    src: PathBuf,
    thumbs_dir: PathBuf,
) -> Result<Vec<u8>> {
    let _permit = THUMB_WORK.acquire().await?;
    tokio::task::spawn_blocking(move || get_or_create_thumb_sync(media_id, &src, &thumbs_dir))
        .await?
}

pub fn get_or_create_thumb_sync(media_id: i64, src: &Path, thumbs_dir: &Path) -> Result<Vec<u8>> {
    let _guard = THUMB_LOCKS[media_id.unsigned_abs() as usize % 64]
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Use dunce::simplified to handle Windows long paths (\\?\ prefix)
    let src = dunce::simplified(src).to_path_buf();
    let thumb_path = dunce::simplified(&thumbs_dir.join(format!("{}.jpg", media_id))).to_path_buf();

    let current_fp = fingerprint(&src);
    if !src.is_file() {
        return Err(anyhow::anyhow!("Media file is unavailable"));
    }
    let stamp = format!("{}:{:?}", src.display(), current_fp);
    let stamp_path = thumbs_dir.join(format!("{media_id}.stamp"));
    let failed_path = thumbs_dir.join(format!("{media_id}.failed"));
    if thumb_path.is_file() && std::fs::read_to_string(&stamp_path).ok().as_ref() == Some(&stamp) {
        return Ok(std::fs::read(&thumb_path)?);
    }
    if std::fs::read_to_string(&failed_path).ok().as_ref() == Some(&stamp) {
        return Err(anyhow::anyhow!(
            "Thumbnail unavailable for this file version"
        ));
    }

    // Already know this exact file (same size/mtime, or same "still missing")
    // failed last time — skip straight to the cached error, no I/O.
    if let Some((cached_fp, msg)) = FAILED_THUMBS.lock().unwrap().get(&media_id) {
        if *cached_fp == current_fp {
            return Err(anyhow::anyhow!("{}", msg));
        }
    }

    let img = match image::open(&src) {
        Ok(img) => img,
        Err(e) => {
            let msg = e.to_string();
            let mut failures = FAILED_THUMBS.lock().unwrap();
            if failures.len() >= 4096 {
                failures.clear();
            }
            failures.insert(media_id, (current_fp, msg.clone()));
            let _ = std::fs::write(&failed_path, &stamp);
            warn!(
                "Could not decode image for thumbnail, media {}: {}",
                media_id, msg
            );
            return Err(e.into());
        }
    };
    let thumb = img.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    let rgb = thumb.to_rgb8();

    let mut buf = std::io::Cursor::new(Vec::new());
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82);
    encoder.encode(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ColorType::Rgb8.into(),
    )?;

    let bytes = buf.into_inner();
    let written = std::fs::write(&thumb_path, &bytes);
    let cached = written.is_ok();
    if let Err(e) = written {
        warn!("Could not write thumbnail for media {}: {}", media_id, e);
        // Return the bytes anyway — don't fail the request
    }

    if cached {
        let _ = std::fs::write(stamp_path, stamp);
    }
    let _ = std::fs::remove_file(failed_path);
    // Successful decode — clear any stale failure recorded for this media_id.
    FAILED_THUMBS.lock().unwrap().remove(&media_id);

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_requests_and_file_version_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("image.png");
        image::RgbImage::from_pixel(8, 8, image::Rgb([255, 0, 0]))
            .save(&src)
            .unwrap();
        let (a, b) = tokio::join!(
            get_or_create_thumb(922, src.clone(), dir.path().into()),
            get_or_create_thumb(922, src.clone(), dir.path().into())
        );
        let first = a.unwrap();
        assert_eq!(first, b.unwrap());
        image::RgbImage::from_pixel(12, 12, image::Rgb([0, 0, 255]))
            .save(&src)
            .unwrap();
        let changed = get_or_create_thumb(922, src.clone(), dir.path().into())
            .await
            .unwrap();
        assert_ne!(first, changed);
        std::fs::remove_file(&src).unwrap();
        assert!(get_or_create_thumb(922, src.clone(), dir.path().into())
            .await
            .is_err());
        std::fs::write(&src, b"broken").unwrap();
        assert!(get_or_create_thumb(922, src.clone(), dir.path().into())
            .await
            .is_err());
        assert!(dir.path().join("922.failed").is_file());
        FAILED_THUMBS.lock().unwrap().clear(); // Simulate a restart; persistent negative cache still applies.
        assert!(get_or_create_thumb(922, src.clone(), dir.path().into())
            .await
            .is_err());
        image::RgbImage::new(16, 16).save(&src).unwrap();
        assert!(get_or_create_thumb(922, src, dir.path().into())
            .await
            .is_ok());
    }
}
