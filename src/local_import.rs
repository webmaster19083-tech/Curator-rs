//! Local folders use ordinary sources and the existing media indexer.
use std::path::Path;
use anyhow::Result;
use rusqlite::OptionalExtension;

pub fn import_folder(state: &crate::AppState, folder: &Path, group_id: Option<i64>) -> Result<i64> {
    let folder=dunce::canonicalize(folder)?;
    anyhow::ensure!(folder.is_dir(),"Select a folder");
    let library=dunce::canonicalize(&state.library_dir)?;
    anyhow::ensure!(!library.starts_with(&folder)&&!folder.starts_with(&library),"Select a folder outside Curator's managed library");
    let url=format!("local:{}",folder.to_string_lossy());
    let conn=state.pool.get()?;
    let existing:Option<i64>=conn.query_row("SELECT id FROM sources WHERE url=?1",[&url],|r|r.get(0)).optional()?;
    let id=if let Some(id)=existing { id } else {
        let name=folder.file_name().unwrap_or_default().to_string_lossy();
        let slug=format!("local-{:032x}",rand::random::<u128>());
        conn.execute("INSERT INTO sources(name,url,slug,status,group_id,added_at) VALUES(?1,?2,?3,'pending',?4,?5)",rusqlite::params![name,url,slug,group_id,crate::db::now_iso()])?;
        conn.last_insert_rowid()
    };
    drop(conn);
    sync_folder(state,id,&folder)?;
    Ok(id)
}

pub fn sync_folder(state:&crate::AppState,id:i64,folder:&Path)->Result<()> {
    let slug:String=state.pool.get()?.query_row("SELECT slug FROM sources WHERE id=?1",[id],|r|r.get(0))?;
    let dest=state.library_dir.join(slug);
    std::fs::create_dir_all(&dest)?;
    let result=(||->Result<()> {
        for entry in walkdir::WalkDir::new(folder).follow_links(false) {
            anyhow::ensure!(!state.shutdown.is_cancelled(),"Import interrupted; sync the source to resume");
            let entry=entry?;if !entry.file_type().is_file(){continue;}
            let ext=entry.path().extension().and_then(|e|e.to_str()).unwrap_or("").to_lowercase();
            if !crate::downloader::image_exts().contains(&ext.as_str())&&!crate::downloader::video_exts().contains(&ext.as_str()){continue;}
            let target=dest.join(entry.path().strip_prefix(folder)?);
            std::fs::create_dir_all(target.parent().unwrap())?;
            if !target.exists() {
                // Copy first into a temporary file. Interrupted copies cannot be indexed as complete media.
                let mut temporary=tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
                std::io::copy(&mut std::fs::File::open(entry.path())?,temporary.as_file_mut())?;
                temporary.persist_noclobber(&target)?;
            }
            crate::downloader::index_file(state,id,&target)?;
        }
        Ok(())
    })();
    let conn=state.pool.get()?;
    conn.execute("UPDATE sources SET status=?1,error_message=?2,synced_at=?3,item_count=(SELECT COUNT(*) FROM media WHERE source_id=?4 AND downloaded=1 AND missing=0) WHERE id=?4",rusqlite::params![if result.is_ok(){"done"}else{"error"},result.as_ref().err().map(ToString::to_string),crate::db::now_iso(),id])?;
    result
}
