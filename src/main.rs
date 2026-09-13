#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--docs") {
        print!("{}", curator::DOCS_TEXT);
        return Ok(());
    }
    let state = curator::initialize().await?;
    curator::remote::start_http_server(&state).await?;
    // The same listener is used by the desktop shell and the browser
    // fallback. Ctrl+C is the explicit headless shutdown path; closing a
    // Tauri window does not arrive here and therefore leaves tray clients up.
    let _ = tokio::signal::ctrl_c().await;
    curator::shutdown(&state).await;
    Ok(())
}
