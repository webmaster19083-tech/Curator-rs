#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use axum::body::{to_bytes, Body};
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use tower::ServiceExt;

#[derive(Clone)]
struct Backend {
    app: axum::Router,
    state: curator::AppState,
}

#[derive(serde::Serialize)]
struct ApiResponse {
    status: u16,
    body: Vec<u8>,
    headers: std::collections::HashMap<String, String>,
}

// Transitional adapter runs existing handlers in process, with no socket or HTTP client.
#[tauri::command]
async fn api_request(
    backend: tauri::State<'_, Backend>,
    path: String,
    method: String,
    body: Option<String>,
) -> Result<ApiResponse, String> {
    if !path.starts_with("/api/") {
        return Err("Only library API requests are accepted".into());
    }
    if body.as_ref().map_or(0, String::len) > 32 * 1024 * 1024 {
        return Err("API request body exceeds 32 MiB limit".into());
    }
    let request = axum::http::Request::builder()
        .uri(path)
        .method(method.as_str())
        .header("content-type", "application/json")
        .body(Body::from(body.unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    let response = backend
        .app
        .clone()
        .oneshot(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ApiResponse {
        status,
        body: bytes.to_vec(),
        headers,
    })
}

#[tauri::command]
async fn library_summary(backend: tauri::State<'_, Backend>) -> Result<serde_json::Value, String> {
    curator::library_summary(&backend.state)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn media_action(
    backend: tauri::State<'_, Backend>,
    id: i64,
    action: String,
) -> Result<String, String> {
    let path = curator::media_path(&backend.state, id).map_err(|e| e.to_string())?;
    match action.as_str() {
        "path" => {}
        "reveal" => tauri_plugin_opener::reveal_item_in_dir(&path).map_err(|e| e.to_string())?,
        "open" => tauri_plugin_opener::open_path(&path, None::<&str>).map_err(|e| e.to_string())?,
        _ => return Err("Unsupported file action".into()),
    }
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
async fn choose_path(app: tauri::AppHandle, directory: bool) -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(move || {
        let picker = app.dialog().file();
        if directory {
            picker.blocking_pick_folder()
        } else {
            picker.blocking_pick_file()
        }
        .map(|p| p.to_string())
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn import_local_folder(
    app: tauri::AppHandle,
    backend: tauri::State<'_, Backend>,
    group_id: Option<i64>,
) -> Result<Option<i64>, String> {
    let state = backend.state.clone();
    tokio::task::spawn_blocking(move || {
        let Some(folder) = app.dialog().file().blocking_pick_folder() else {
            return Ok(None);
        };
        curator::local_import::import_folder(
            &state,
            std::path::Path::new(&folder.to_string()),
            group_id,
        )
        .map(Some)
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Runtime initialization failed: {error}");
            return;
        }
    };
    let mut state = match runtime.block_on(curator::initialize()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("Library initialization failed: {error:#}");
            return;
        }
    };
    // Tauri resolves packaged resources independently of the process working directory.
    let shutdown_state = state.clone();
    let handle = runtime.handle().clone();
    let protocol_handle = handle.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .invoke_handler(tauri::generate_handler![
            api_request,
            library_summary,
            media_action,
            choose_path,
            import_local_folder
        ])
        .setup(move |app| {
            state.static_dir = app.path().resource_dir()?.join("static");
            app.manage(Backend {
                app: curator::router(state.clone()),
                state: state.clone(),
            });
            let completed = handle.block_on(async { state.settings.read().await.oobe_completed });
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App(if completed { "index.html" } else { "oobe.html" }.into()),
            )
            .title("Curator")
            .inner_size(1440.0, 900.0)
            .min_inner_size(960.0, 600.0)
            .disable_drag_drop_handler()
            .build()?;
            let menu = tauri::menu::Menu::with_items(
                app,
                &[
                    &tauri::menu::Submenu::with_items(
                        app,
                        "File",
                        true,
                        &[
                            &tauri::menu::MenuItem::with_id(
                                app,
                                "add-source",
                                "Add Source",
                                true,
                                Some("CmdOrCtrl+N"),
                            )?,
                            &tauri::menu::PredefinedMenuItem::quit(app, None)?,
                        ],
                    )?,
                    &tauri::menu::Submenu::with_items(
                        app,
                        "Tools",
                        true,
                        &[&tauri::menu::MenuItem::with_id(
                            app,
                            "settings",
                            "Settings",
                            true,
                            Some("CmdOrCtrl+,"),
                        )?],
                    )?,
                ],
            )?;
            app.set_menu(menu)?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            let _ = app.emit("desktop-menu", event.id().as_ref());
        })
        .register_asynchronous_uri_scheme_protocol("curator", move |context, request, responder| {
            let backend = context.app_handle().state::<Backend>().inner().clone();
            protocol_handle.spawn(async move {
                let (mut parts, body) = request.into_parts();
                let path = parts
                    .uri
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/");
                if !(path.starts_with("/library/") || path.starts_with("/api/thumb/")) {
                    responder.respond(
                        tauri::http::Response::builder()
                            .status(403)
                            .body(Vec::new())
                            .unwrap_or_else(|_| tauri::http::Response::new(Vec::new())),
                    );
                    return;
                }
                parts.uri = match path.parse() {
                    Ok(uri) => uri,
                    Err(_) => {
                        responder.respond(
                            tauri::http::Response::builder()
                                .status(400)
                                .body(Vec::new())
                                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new())),
                        );
                        return;
                    }
                };
                let response = match backend
                    .app
                    .oneshot(axum::http::Request::from_parts(parts, Body::from(body)))
                    .await
                {
                    Ok(response) => response,
                    Err(_) => {
                        responder.respond(
                            tauri::http::Response::builder()
                                .status(500)
                                .body(Vec::new())
                                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new())),
                        );
                        return;
                    }
                };
                let (parts, body) = response.into_parts();
                match to_bytes(body, 256 * 1024 * 1024).await {
                    Ok(bytes) => {
                        responder.respond(tauri::http::Response::from_parts(parts, bytes.to_vec()))
                    }
                    Err(_) => responder.respond(
                        tauri::http::Response::builder()
                            .status(413)
                            .body(Vec::new())
                            .unwrap_or_else(|_| tauri::http::Response::new(Vec::new())),
                    ),
                }
            });
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("Desktop initialization failed: {error}");
            runtime.block_on(curator::shutdown(&shutdown_state));
            return;
        }
    };
    app.run(move |_, event| {
        if let tauri::RunEvent::Exit = event {
            runtime.block_on(curator::shutdown(&shutdown_state));
        }
    });
}
