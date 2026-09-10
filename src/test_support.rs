use crate::*;

pub fn state(root: &std::path::Path) -> Arc<AppState> {
    for dir in ["library", "archives", "thumbnails"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    Arc::new(AppState {
        pool: db::init_pool(root).unwrap(),
        group_tag_cache: Arc::new(RwLock::new(None)),
        shutdown: tokio_util::sync::CancellationToken::new(),
        download_tasks: tokio_util::task::TaskTracker::new(),
        source_cancellations: Arc::new(Mutex::new(HashMap::new())),
        running_sources: Arc::new(Mutex::new(HashSet::new())),
        downloads_paused: Arc::new(AtomicBool::new(false)),
        active_processes: Arc::new(Mutex::new(HashMap::new())),
        paused_source_ids: Arc::new(Mutex::new(HashSet::new())),
        download_semaphore: Arc::new(Mutex::new(Arc::new(Semaphore::new(2)))),
        placeholder_semaphore: Arc::new(Semaphore::new(1)),
        settings: Arc::new(RwLock::new(db::Settings::default())),
        search_registry: Arc::new(routes::search::default_provider_registry()),
        data_dir: root.into(),
        library_dir: root.join("library"),
        archives_dir: root.join("archives"),
        thumbs_dir: root.join("thumbnails"),
        log_path: root.join("curator.log"),
        static_dir: root.into(),
        gallery_dl_bin: "missing-gallery-dl-test".into(),
        ffprobe_bin: "missing-ffprobe-test".into(),
        ffmpeg_bin: "missing-ffmpeg-test".into(),
        python_bin: "missing-python-test".into(),
        nsfw: None,
        action_classifier: None,
        action_model_path: None,
    })
}

pub fn source(state: &AppState) {
    state.pool.get().unwrap().execute("INSERT INTO sources(id,name,url,slug,added_at) VALUES(1,'test','https://example.test','test','2026')", []).unwrap();
    std::fs::create_dir_all(state.library_dir.join("test")).unwrap();
}

#[cfg(windows)]
pub fn fake_downloader() -> String {
    static FIXTURE: once_cell::sync::Lazy<(tempfile::TempDir, String)> =
        once_cell::sync::Lazy::new(|| {
            let dir = tempfile::tempdir().unwrap();
            let src = dir.path().join("fake.rs");
            let exe = dir
                .path()
                .join(if cfg!(windows) { "fake.exe" } else { "fake" });
            std::fs::write(&src, include_str!("../tests/fixtures/fake_gallery_dl.rs")).unwrap();
            let output = std::process::Command::new("rustc")
                .arg(&src)
                .arg("-o")
                .arg(&exe)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (dir, exe.to_string_lossy().into_owned())
        });
    FIXTURE.1.clone()
}
