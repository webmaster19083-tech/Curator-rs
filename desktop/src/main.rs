#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use tower::ServiceExt;

#[cfg(target_os = "windows")]
mod single_instance {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };

    const MUTEX_NAME: &str = "Local\\Curator.Desktop.SingleInstance";
    const ACTIVATE_EVENT_NAME: &str = "Local\\Curator.Desktop.Activate";

    pub enum Claim {
        Primary(Guard),
        Secondary,
    }

    pub struct Guard {
        _mutex: HANDLE,
        activation_event: HANDLE,
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    pub fn claim(activate_existing: bool) -> Result<Claim, String> {
        let mutex_name = wide(MUTEX_NAME);
        unsafe { SetLastError(0) };
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex.is_null() {
            return Err(format!(
                "could not create Curator instance lock: {}",
                unsafe { GetLastError() }
            ));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(mutex) };
            if activate_existing {
                let event_name = wide(ACTIVATE_EVENT_NAME);
                let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
                if !event.is_null() {
                    unsafe {
                        SetEvent(event);
                        CloseHandle(event);
                    }
                }
            }
            return Ok(Claim::Secondary);
        }

        let event_name = wide(ACTIVATE_EVENT_NAME);
        let activation_event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
        if activation_event.is_null() {
            let error = unsafe { GetLastError() };
            unsafe { CloseHandle(mutex) };
            return Err(format!(
                "could not create Curator activation event: {error}"
            ));
        }
        Ok(Claim::Primary(Guard {
            _mutex: mutex,
            activation_event,
        }))
    }

    impl Guard {
        pub fn listen_for_activation(&self, app: tauri::AppHandle) {
            // Windows HANDLE is a raw pointer type, so carry its opaque value
            // across the listener thread as an integer and restore it only at
            // the FFI call boundary.
            let event = self.activation_event as usize;
            let _ = std::thread::Builder::new()
                .name("curator-instance-activation".into())
                .spawn(move || loop {
                    if unsafe { WaitForSingleObject(event as HANDLE, INFINITE) } != WAIT_OBJECT_0 {
                        break;
                    }
                    let app = app.clone();
                    let app_for_ui = app.clone();
                    let _ = app.run_on_main_thread(move || super::open_curator(&app_for_ui));
                });
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.activation_event);
                CloseHandle(self._mutex);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod single_instance {
    pub struct Guard;
    pub enum Claim {
        Primary(Guard),
    }
    pub fn claim(_: bool) -> Result<Claim, String> {
        Ok(Claim::Primary(Guard))
    }
    impl Guard {
        pub fn listen_for_activation(&self, _: tauri::AppHandle) {}
    }
}

#[derive(Clone)]
struct Backend {
    app: axum::Router,
    state: curator::AppState,
}

#[derive(Clone)]
struct DesktopRuntime {
    handle: tokio::runtime::Handle,
}

#[derive(Default)]
struct DesktopLifecycle {
    explicit_quit: AtomicBool,
}

#[derive(Clone)]
struct TrayControls {
    status: tauri::menu::MenuItem<tauri::Wry>,
    pause: tauri::menu::MenuItem<tauri::Wry>,
    startup: tauri::menu::CheckMenuItem<tauri::Wry>,
}

fn background_launch_requested() -> bool {
    std::env::args().any(|argument| argument == "--background")
}

fn create_main_window(app: &tauri::AppHandle, completed: bool) -> tauri::Result<()> {
    // Do not put the WebView2 profile beside the library. Curator's library
    // can live on an external drive which should stay independent of browser
    // cache/lock state. The versioned local profile also leaves the abandoned
    // profile from an older desktop build untouched.
    let webview_data_dir = app.path().app_local_data_dir()?.join("webview-v2");
    tracing::info!(
        webview_data_dir = %webview_data_dir.display(),
        "Creating Curator desktop WebView"
    );
    let result = tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App(if completed { "index.html" } else { "oobe.html" }.into()),
    )
    .title("Curator")
    .inner_size(1440.0, 900.0)
    .min_inner_size(960.0, 600.0)
    .visible(true)
    .data_directory(webview_data_dir)
    .disable_drag_drop_handler()
    .build()
    .map(|_| ());
    if let Err(error) = &result {
        tracing::error!(error = %error, "Could not create Curator desktop WebView");
    } else {
        tracing::info!("Curator desktop WebView is ready");
    }
    result
}

fn open_curator(app: &tauri::AppHandle) {
    tracing::info!("Curator desktop: opening main window");
    if let Some(window) = app.get_webview_window("main") {
        tracing::info!("Curator desktop: restoring existing main window");
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    tracing::info!("Curator desktop: creating first main window");
    let completed = app
        .state::<Backend>()
        .state
        .settings
        .try_read()
        .map(|settings| settings.oobe_completed)
        .unwrap_or(true);
    if let Err(error) = create_main_window(app, completed) {
        tracing::error!(error = %error, "Could not open Curator desktop window");
    }
}

fn request_explicit_quit(app: &tauri::AppHandle) {
    app.state::<DesktopLifecycle>()
        .explicit_quit
        .store(true, Ordering::Release);
    app.exit(0);
}

async fn refresh_tray_controls(state: &curator::AppState, controls: &TrayControls) {
    let pool = state.pool.clone();
    let (downloading, pending, retrying) = tokio::task::spawn_blocking(move || {
        let conn = pool.get().ok()?;
        conn.query_row(
            "SELECT \
                COALESCE(SUM(status='downloading'),0), \
                COALESCE(SUM(status='pending'),0), \
                COALESCE(SUM(status='retrying'),0) \
             FROM sources",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .ok()
    })
    .await
    .ok()
    .flatten()
    .unwrap_or((0, 0, 0));
    let paused = state.downloads_paused.load(Ordering::Acquire);
    let active = downloading + pending + retrying;
    let status = if paused {
        "Downloads: paused".to_string()
    } else if active == 0 {
        "Downloads: idle".to_string()
    } else if retrying == 0 {
        format!("Downloads: {active} active")
    } else {
        format!("Downloads: {active} active ({retrying} retrying)")
    };
    let _ = controls.status.set_text(status);
    let _ = controls.pause.set_text(if paused {
        "Resume Downloads"
    } else {
        "Pause Downloads"
    });
    if let Ok(settings) = state.settings.try_read() {
        let _ = controls.startup.set_checked(settings.start_with_windows);
    }
}

fn spawn_tray_status_poller(
    state: curator::AppState,
    controls: TrayControls,
    runtime: tokio::runtime::Handle,
) {
    let cancellation = state.shutdown.clone();
    let server_tasks = state.server_tasks.clone();
    server_tasks.spawn_on(
        async move {
            loop {
                refresh_tray_controls(&state, &controls).await;
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        },
        &runtime,
    );
}

fn toggle_downloads(app: &tauri::AppHandle) {
    let backend = app.state::<Backend>().inner().clone();
    let controls = app.state::<TrayControls>().inner().clone();
    let handle = app.state::<DesktopRuntime>().handle.clone();
    handle.spawn(async move {
        let state = std::sync::Arc::new(backend.state.clone());
        if state.downloads_paused.load(Ordering::Acquire) {
            let _ = curator::routes::downloads::resume(axum::extract::State(state.clone())).await;
        } else {
            let _ = curator::routes::downloads::pause(axum::extract::State(state.clone())).await;
        }
        refresh_tray_controls(&backend.state, &controls).await;
    });
}

fn toggle_start_with_windows(app: &tauri::AppHandle) {
    let backend = app.state::<Backend>().inner().clone();
    let controls = app.state::<TrayControls>().inner().clone();
    let handle = app.state::<DesktopRuntime>().handle.clone();
    let current = backend
        .state
        .settings
        .try_read()
        .map(|settings| settings.start_with_windows)
        .unwrap_or(false);
    let enabled = !current;
    let _ = controls.startup.set_checked(enabled);
    handle.spawn(async move {
        if curator::set_start_with_windows_preference(&backend.state, enabled)
            .await
            .is_err()
        {
            let _ = controls.startup.set_checked(current);
        }
    });
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
    // Curator's backend deliberately outlives its visible window. Without an
    // instance gate, launching the shortcut while the tray process is alive
    // opens a second WebView2 profile and produces a blank, non-responsive
    // window (0x800700AA). A later launch now signals the primary process to
    // restore its existing window and exits before it can bind another server.
    let background_launch = background_launch_requested();
    let instance = match single_instance::claim(!background_launch) {
        Ok(single_instance::Claim::Primary(guard)) => guard,
        #[cfg(target_os = "windows")]
        Ok(single_instance::Claim::Secondary) => return,
        Err(error) => {
            eprintln!("Curator instance initialization failed: {error}");
            return;
        }
    };
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
            // This is intentionally started before the webview is created. The
            // Tauri shell and headless binary share this exact listener, so a
            // hidden/tray-only desktop process remains reachable to LAN and
            // Tailscale clients without a second server.
            handle
                .block_on(curator::remote::start_http_server(&state))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            app.manage(Backend {
                app: curator::router(state.clone()),
                state: state.clone(),
            });
            app.manage(DesktopRuntime {
                handle: handle.clone(),
            });
            app.manage(DesktopLifecycle::default());
            // The WebView is deliberately not created from `setup`: on some
            // Windows/WebView2 combinations that happens before the native
            // message loop is running and leaves a blank, non-responsive
            // shell. `RunEvent::Ready` below creates foreground UI after the
            // loop is live. Background launches stay WebView-free until the
            // user opens Curator from its tray icon.

            let tray_open = tauri::menu::MenuItem::with_id(
                app,
                "open-curator",
                "Open Curator",
                true,
                None::<&str>,
            )?;
            let tray_status = tauri::menu::MenuItem::with_id(
                app,
                "download-status",
                "Downloads: checking…",
                false,
                None::<&str>,
            )?;
            let tray_pause = tauri::menu::MenuItem::with_id(
                app,
                "pause-downloads",
                "Pause Downloads",
                true,
                None::<&str>,
            )?;
            // Do not block the native setup thread on an async lock. The
            // tray poller refreshes this shortly after startup if a settings
            // write is in progress.
            let start_with_windows = state
                .settings
                .try_read()
                .map(|settings| settings.start_with_windows)
                .unwrap_or(false);
            let tray_startup = tauri::menu::CheckMenuItem::with_id(
                app,
                "start-with-windows",
                "Start with Windows",
                true,
                start_with_windows,
                None::<&str>,
            )?;
            let tray_quit = tauri::menu::MenuItem::with_id(
                app,
                "quit-curator",
                "Quit Curator",
                true,
                None::<&str>,
            )?;
            let tray_menu = tauri::menu::Menu::with_items(
                app,
                &[
                    &tray_open,
                    &tray_status,
                    &tray_pause,
                    &tray_startup,
                    &tauri::menu::PredefinedMenuItem::separator(app)?,
                    &tray_quit,
                ],
            )?;
            let tray_controls = TrayControls {
                status: tray_status,
                pause: tray_pause,
                startup: tray_startup,
            };
            app.manage(tray_controls.clone());
            tracing::info!("Curator desktop setup: creating tray icon");
            let mut tray_builder = tauri::tray::TrayIconBuilder::with_id("curator-tray")
                .menu(&tray_menu)
                .tooltip("Curator")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| match event {
                    tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    }
                    | tauri::tray::TrayIconEvent::DoubleClick {
                        button: tauri::tray::MouseButton::Left,
                        ..
                    } => open_curator(tray.app_handle()),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(icon);
            }
            // Tauri retains a registered clone for the process lifetime; the
            // local value can drop after build without making the icon vanish.
            let _tray = tray_builder.build(app)?;
            tracing::info!("Curator desktop setup: tray icon ready");

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
                            &tauri::menu::MenuItem::with_id(
                                app,
                                "quit-curator",
                                "Quit Curator",
                                true,
                                None::<&str>,
                            )?,
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
            tracing::info!("Curator desktop setup: application menu ready");
            // Native menu mutations must wait until the Windows event loop is
            // accepting messages. Starting the poller here can race the first
            // `Ready` event on WebView2/Windows combinations and strand the
            // shell before the UI callback is reached.
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open-curator" => open_curator(app),
            "pause-downloads" => toggle_downloads(app),
            "start-with-windows" => toggle_start_with_windows(app),
            "quit-curator" => request_explicit_quit(app),
            _ => {
                let _ = app.emit("desktop-menu", event.id().as_ref());
            }
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
    tracing::info!(background_launch, "Curator desktop: Tauri app built");
    instance.listen_for_activation(app.handle().clone());
    tracing::info!("Curator desktop: entering native event loop");
    app.run(move |app_handle, event| match event {
        tauri::RunEvent::Ready => {
            tracing::info!(background_launch, "Curator desktop: received Ready event");
            let backend = app_handle.state::<Backend>().inner().clone();
            let controls = app_handle.state::<TrayControls>().inner().clone();
            let runtime = app_handle.state::<DesktopRuntime>().handle.clone();
            spawn_tray_status_poller(backend.state, controls, runtime);
            if !background_launch {
                open_curator(app_handle);
            }
        }
        tauri::RunEvent::ExitRequested { api, .. } => {
            if !app_handle
                .state::<DesktopLifecycle>()
                .explicit_quit
                .load(Ordering::Acquire)
            {
                api.prevent_exit();
            }
        }
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } if label == "main" => {
            if !app_handle
                .state::<DesktopLifecycle>()
                .explicit_quit
                .load(Ordering::Acquire)
            {
                let keep_running_in_tray = app_handle
                    .state::<Backend>()
                    .state
                    .settings
                    .try_read()
                    .map(|settings| settings.keep_running_in_tray)
                    .unwrap_or(true);
                if keep_running_in_tray {
                    api.prevent_close();
                    if let Some(window) = app_handle.get_webview_window("main") {
                        let _ = window.hide();
                    }
                }
            }
        }
        tauri::RunEvent::Exit => {
            tracing::info!("Curator desktop: native event loop is exiting");
            runtime.block_on(curator::shutdown(&shutdown_state))
        }
        _ => {}
    });
}
