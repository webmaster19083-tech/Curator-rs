//! Optional headless server; the installed application lives in desktop/.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--docs") {
        print!("{}", curator::DOCS_TEXT);
        return Ok(());
    }
    let state = curator::initialize().await?;
    curator::remote::start_http_server(&state).await?;
    let _ = tokio::signal::ctrl_c().await;
    curator::shutdown(&state).await;
    Ok(())
}
