# Curator reliability and scalability fixes

Based on GitHub `master` at `31c558fa092cf84f63a3af40320427336a45f308`, fetched and compared with the clean workspace. The supplied setup-wizard patch/tree is included. Evidence was read from `E:\CuratorData\curator.log`; the live database and library were not modified.

## Fixed

- **Stale downloaded rows:** added persistent `missing`, file-version, classification, and duration-attempt fields. Startup and hourly reconciliation use batches of 256 rows. Missing files become `downloaded=0, missing=1`, are excluded from browsing/workers, and retain IDs, ratings, tags, and origin URLs. Restoring a file restores availability. SQLite triggers maintain downloaded source counts. No automatic redownload is attempted: that would override the existing gallery-dl archive and intentional file-deletion behavior.
- **Classification storms:** checks `Path::is_file()` before enqueueing and immediately before worker execution. The channel holds at most 32 jobs. SQL selects only eligible rows; persistent leases, one-hour retry delays, and a three-attempt limit per file version replace the growing in-memory skip set. File changes reset failed work. An unavailable worker pauses dispatch, and the existing restart supervisor now has startup/request timeouts. Repeated per-file warnings are limited to one per minute.
- **Missing media responses:** retained Axum `ServeDir` and its range support; missing files return HTTP 404. A 404 reconciles its DB path. Thumbnail requests check availability before decoding. The ASGI tracebacks in the log came from the older Python runtime, not current Rust HTTP serving.
- **Thumbnails:** eight concurrent blocking jobs maximum, striped per-media locks, existing failure cache retained and bounded, persistent failure fingerprints, and positive/negative cache invalidation by path, size, and modification time. Missing files do not generate decode warnings. Browser caching now permits revalidation after content changes.
- **Indexing:** native `notify` events feed a bounded 1,024-path queue and a single serialized consumer per source. Downloads use gallery-dl's normal partial-file handling; `.part` files are not indexed. Removed four-second recursive scans. Recovery scans run initially, on completion, and at least five minutes apart while a download is active. Startup recovers files whose completion event was lost. Overflow/events missed by a filesystem are recovered without overlapping scans. Late sidecar metadata merges an early indexed row into its placeholder without losing annotations; sidecars remain available for recovery.
- **Media API:** default 100 rows, maximum 500, keyset cursors, stable tie-breakers for all existing sorts, and seeded shuffle pages. Type, rating, source/group, and tag filters execute before SQL LIMIT. Own tags and inherited ancestor/group tags preserve their meaning. Browser grid, lightbox, slideshow, portrait wall, and feed load additional pages on demand.
- **Tag queries and playlists:** SQL predicates implement the existing single-tag filter and playlist AND/exclusion semantics. `/api/media` also accepts AND/OR/exclusion lists. Playlist selection filters and limits in SQLite; shuffle no longer loads the whole library into Rust. Its total count remains an SQL count.
- **Cache/DB lifetimes:** requests clone an `Arc` to the effective group-tag map. Cache rebuilds cannot overwrite an intervening invalidation. Group creation retrieves its result before releasing the DB connection and awaiting invalidation. Other affected group/tag/media/export/source/playlist handlers release connections before async work.
- **Downloader lifecycle:** explicit application/source cancellation, tracked tasks, child reaping, and Windows `taskkill /F /T /PID`. Missing/zero PIDs never reach taskkill. Pause retains resume information, resume waits for stopped runs, queued work observes cancellation, and source deletion waits for its download to stop. Nonzero gallery-dl exits remain failures even when some files downloaded. `KeyboardInterrupt` output no longer determines shutdown state. Both output streams continue draining concurrently; retained stderr is bounded.
- **Placeholder scans:** independent of download permits, capped at 30 seconds, 1,000 listed items, and 16 MiB of listing output. Persistent six-hour URL-aware cooldowns cover successful and failed scans and deduplicate concurrent calls. Timeouts/cancellation deliberately terminate and reap the subprocess.
- **Duration probes:** kept on blocking threads, moved out of the indexing path into the existing bounded backfill, added a five-second process timeout, and persisted failures so malformed videos are not probed every pass. Known durations are retained until the file changes.
- **Migrations:** schema/data changes and legacy tracker repair now run in a transaction. Existing legacy backups are preserved, missing `name` or `applied_at` columns are repaired, and slug migration failures propagate as Results instead of panicking or being marked applied after failure.
- **Dead modules:** removed `src/state.rs`, `src/thumb.rs`, and `src/settings.rs` after checking module declarations/references. The active implementations remain in `main.rs`, `thumb_worker.rs`, and `db.rs` respectively.

## API pagination

Start with `/api/media?limit=100`. When `has_more` is true, pass the returned `next_cursor` as `cursor` with the same filters and sort. The response retains `media` and adds `has_more`, `next_cursor`, `next_after_id`, and `limit`.

`after_id` is also supported: `/api/media?limit=100&after_id=12345`. Prefer `cursor`, which includes the sort value and still works if the anchor row is deleted. For shuffle use `sort=shuffle&shuffle_seed=12345` and retain the seed for subsequent pages. Shuffle bounds result memory, though SQLite must still examine matching IDs to order a randomized selection.

Filters: `tag` is one exact normalized tag; `tags` is a comma-separated AND list; `any_tags` is an OR list; `exclude_tags` excludes any listed tag. `media_type=image|clip|video` mirrors the browser's duration rules, including excluding unknown durations from the clip/video split. Existing scope precedence and rating semantics are preserved.

## Files changed

- `.gitignore`: excludes build/package output.
- `Cargo.toml`: filesystem notifications, Tokio task tracking, HTTP test utilities; release optimization settings preserved.
- `Cargo.lock`: corresponding dependency resolution.
- `src/db.rs`: persistent availability/worker fields, counters/indexes, transactional migration repairs, regression tests, supplied wizard settings compatibility.
- `src/media_files.rs`: batched missing/restored/changed-file reconciliation and annotation-preservation tests.
- `src/nsfw.rs`: bounded queue, persisted claims/retry suppression, file checks, worker readiness/timeouts, tests.
- `src/downloader.rs`: incremental indexing, placeholder merging/cache/bounds, process-tree termination and explicit cancellation, subprocess/event regression tests.
- `src/duration.rs`: persistent one-attempt-per-version duration backfill.
- `src/process.rs`: bounded synchronous subprocess execution and hung-process test.
- `src/thumb_worker.rs`: serialized, versioned positive/negative thumbnail caches and concurrency/invalidation tests.
- `src/main.rs`: shared Arc cache, cancellation/task tracking, startup/hourly reconciliation, startup index recovery, graceful HTTP shutdown, supplied wizard/config integration.
- `src/routes/media.rs`: SQL filtering, bounded keyset pages, shared tag-cache helper, pagination/filter tests.
- `src/routes/ch.rs`: SQL playlist filtering/limit/randomization and bounded response construction.
- `src/routes/library.rs`: reconcile missing paths after clean ServeDir 404 responses; HTTP regression test.
- `src/routes/thumb.rs`: file checks, stale-row marking, clean errors, revalidatable caching.
- `src/routes/groups.rs`: connection scopes before cache awaits.
- `src/routes/tags.rs`: shared Arc cache and connection lifetime fixes.
- `src/routes/downloads.rs`: reliable pause/resume handling.
- `src/routes/sources.rs`: explicit cancellation and wait before source deletion.
- `src/routes/export.rs`: release DB connection before async invalidation; Clippy cleanup.
- `src/routes/mod.rs`: library/wizard module routing.
- `src/routes/settings.rs`: supplied setup-wizard settings and Clippy cleanup.
- `src/chpack.rs`: derive the unchanged default implementation to keep Clippy clean.
- `src/config.rs`: supplied shared setup configuration; Clippy cleanup.
- `src/oobe.rs`: supplied setup-wizard checks and compatibility tests.
- `src/routes/oobe.rs`: supplied setup-wizard routes.
- `src/test_support.rs`: isolated DB/application fixtures and a compiled fake downloader.
- `static/app.js`: on-demand media pages across viewing modes, stable server shuffle, stale-response/concurrent-scroll guards, supplied wizard entry point.
- `static/index.html`: supplied setup-wizard entry point.
- `static/oobe.html`: supplied wizard UI.
- `static/oobe.css`: supplied wizard styling.
- `static/oobe.js`: supplied wizard behavior.
- `tests/media_pagination.test.js`: browser pagination/filter/navigation regression tests.
- `tests/fixtures/fake_gallery_dl.rs`: local subprocess fixture for failure, shutdown, and Windows process-tree tests.
- `README.md`: supplied setup-wizard documentation.
- `DOCS.txt`: supplied setup-wizard documentation.
- `FIXES.md`: this implementation and validation record.
- Removed `src/state.rs`, `src/thumb.rs`, `src/settings.rs`: unreferenced duplicate modules.

## Validation

- `cargo check`: passed.
- `cargo test`: 45 passed. Windows process-tree tests require normal process-management permissions; the sandbox prevents taskkill from terminating descendants. The final full run passed outside the sandbox.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo build --release`: passed with existing opt-level/LTO/codegen/strip settings.
- `node --check static/app.js`: passed.
- `node --test tests/media_pagination.test.js`: 3 passed.
- `git diff --check`: passed.

The optional real-video duration test skips if ffmpeg is unavailable. Core duration timeout/failure behavior is covered independently. Live remote gallery-dl extractors, authenticated downloads, and the optional Python model were not exercised; process behavior was verified with local fixtures. No production data was changed and no GitHub push was made.
