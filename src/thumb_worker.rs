use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Result;
use once_cell::sync::Lazy;
use tracing::warn;

const THUMB_MAX_DIM: u32 = 360;

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

    let current_fp = fingerprint(&src);

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
            FAILED_THUMBS.lock().unwrap().insert(media_id, (current_fp, msg.clone()));
            warn!("Could not decode image for thumbnail, media {}: {}", media_id, msg);
            return Err(e.into());
        }
    };
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

    // Successful decode — clear any stale failure recorded for this media_id.
    FAILED_THUMBS.lock().unwrap().remove(&media_id);

    Ok(bytes)
}
