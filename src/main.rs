#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let state = curator::initialize().await?;
    let app = curator::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:42168").await?;
    tracing::info!("Curator listening on http://127.0.0.1:42168");
    let result = axum::serve(listener, app).await;
    curator::shutdown(&state).await;
    result?;
    Ok(())
}
