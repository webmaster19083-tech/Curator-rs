//! Optional headless server; the installed application lives in desktop/.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--docs") {
        print!("{}", curator::DOCS_TEXT);
        return Ok(());
    }
    let state = curator::initialize().await?;
    let app = curator::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8642").await?;
    tracing::info!("Headless Curator listening on http://127.0.0.1:8642");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            curator::shutdown(&state).await;
        })
        .await?;
    Ok(())
}
