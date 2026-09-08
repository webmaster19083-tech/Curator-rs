use crate::test_support;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(state: &crate::AppState, method: &str, path: &str, body: Value) -> (u16, Value) {
    let response = crate::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn organization_moves_reject_cycles_and_preserve_paths_and_inherited_tags() {
    let root = tempfile::tempdir().unwrap();
    let state = test_support::state(root.path());
    test_support::source(&state);
    let (_, parent) = request(&state, "POST", "/api/groups", json!({"name":"Models"})).await;
    let parent = parent["id"].as_i64().unwrap();
    let (_, child) = request(
        &state,
        "POST",
        "/api/groups",
        json!({"name":"Creator","parent_id":parent}),
    )
    .await;
    let child = child["id"].as_i64().unwrap();
    assert_eq!(
        request(
            &state,
            "PATCH",
            &format!("/api/groups/{parent}"),
            json!({"parent_id":child})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request(
            &state,
            "PATCH",
            &format!("/api/groups/{child}"),
            json!({"parent_id":child})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        request(
            &state,
            "PATCH",
            "/api/sources/1/group",
            json!({"group_id":child})
        )
        .await
        .0,
        200
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &state,
                "POST",
                &format!("/api/groups/{parent}/tags"),
                json!({"name":"portrait"})
            )
            .await
            .0,
            200
        );
    }
    let conn = state.pool.get().unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM group_tags", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    conn.execute("INSERT INTO media(source_id,filepath,filename,type,added_at,file_size_bytes) VALUES(1,'test/a.jpg','a.jpg','image','now',125)",[]).unwrap();
    let (_, page) = request(
        &state,
        "GET",
        &format!("/api/media?group_id={parent}&tag=portrait"),
        Value::Null,
    )
    .await;
    assert_eq!(page["media"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(
            &state,
            "PATCH",
            &format!("/api/groups/{child}"),
            json!({"clear_parent":true})
        )
        .await
        .0,
        200
    );
    let (_, page) = request(
        &state,
        "GET",
        &format!("/api/media?group_id={parent}"),
        Value::Null,
    )
    .await;
    assert_eq!(page["media"].as_array().unwrap().len(), 0);
    assert_eq!(
        conn.query_row("SELECT filepath FROM media", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "test/a.jpg"
    );
    assert_eq!(
        request(
            &state,
            "PATCH",
            "/api/sources/1/group",
            json!({"group_id":null})
        )
        .await
        .0,
        200
    );
    let (_, page) = request(&state, "GET", "/api/media?group_id=0", Value::Null).await;
    assert_eq!(page["media"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn sizes_filter_sort_paginate_with_unknown_last() {
    let root = tempfile::tempdir().unwrap();
    let state = test_support::state(root.path());
    test_support::source(&state);
    let conn = state.pool.get().unwrap();
    conn.execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,file_size_bytes) VALUES
      (1,1,'a','a','image','now',NULL),(2,1,'b','b','image','now',0),(3,1,'c','c','image','now',500),(4,1,'d','d','image','now',500);").unwrap();
    for (sort, ids) in [
        ("size_desc", vec![3, 4, 2, 1]),
        ("size_asc", vec![2, 3, 4, 1]),
    ] {
        let mut actual = Vec::new();
        let mut path = format!("/api/media?sort={sort}&limit=1");
        loop {
            let (_, page) = request(&state, "GET", &path, Value::Null).await;
            actual.extend(
                page["media"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["id"].as_i64().unwrap()),
            );
            match page["next_cursor"].as_str() {
                Some(c) => path = format!("/api/media?sort={sort}&limit=1&cursor={c}"),
                None => break,
            }
        }
        assert_eq!(actual, ids);
    }
    let (_, page) = request(
        &state,
        "GET",
        "/api/media?min_size=1&max_size=500",
        Value::Null,
    )
    .await;
    assert_eq!(page["media"].as_array().unwrap().len(), 2);
    let (_, page) = request(&state, "GET", "/api/media?unknown_size=true", Value::Null).await;
    assert_eq!(page["media"][0]["id"], 1);
    assert_eq!(
        request(&state, "GET", "/api/media?min_size=-1", Value::Null)
            .await
            .0,
        400
    );
}

#[test]
fn legacy_size_migration_and_interruptible_backfill() {
    let root = tempfile::tempdir().unwrap();
    let state = test_support::state(root.path());
    test_support::source(&state);
    let conn = state.pool.get().unwrap();
    conn.execute_batch("DROP INDEX idx_media_size; ALTER TABLE media DROP COLUMN file_size_bytes;
      INSERT INTO media(source_id,filepath,filename,type,added_at,rating) VALUES(1,'test/a.jpg','a.jpg','image','now',4);").unwrap();
    crate::db::run_migrations(&conn).unwrap();
    crate::db::run_migrations(&conn).unwrap();
    std::fs::write(state.library_dir.join("test/a.jpg"), [1, 2, 3, 4]).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    crate::media_files::reconcile_cancellable(&state.pool, &state.library_dir, Some(&cancel))
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT file_size_bytes FROM media", [], |r| r
            .get::<_, Option<i64>>(0))
            .unwrap(),
        None
    );
    crate::media_files::reconcile(&state.pool, &state.library_dir).unwrap();
    assert_eq!(
        conn.query_row("SELECT file_size_bytes,rating FROM media", [], |r| Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?
        )))
        .unwrap(),
        (4, 4)
    );
    assert_eq!(crate::media_files::file_size(&state.library_dir), None);
    assert_eq!(
        crate::media_files::file_size(&state.library_dir.join("absent")),
        None
    );
}
