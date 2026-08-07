use serde::{Deserialize, Serialize};
use sysinfo::System;
use tauri::{ipc::Channel, Manager};
use tokio::sync::{watch, Mutex};

mod hardware;
mod model_manager;
mod tuning;
mod mcp_bridge;
mod migrate;
mod paths;
mod file_browser;
mod browser;
mod provider_manager;
mod llama_engine;
mod proc;
mod secure_db;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallChunk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: ToolCallFunction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatStreamChunk {
    pub delta: String,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

#[derive(Debug, Serialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: String,
    pub vram_mb: Option<u64>,
    pub driver: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HardwareInfo {
    pub os: String,
    pub os_version: String,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub total_ram_mb: u64,
    pub gpus: Vec<GpuInfo>,
}

#[tauri::command]
async fn search_huggingface(query: String) -> Result<Vec<model_manager::HfSearchResult>, String> {
    model_manager::search_huggingface(&query).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_huggingface_files(repo_id: String) -> Result<Vec<model_manager::HfGgufFile>, String> {
    model_manager::list_huggingface_files(&repo_id).await.map_err(|e| e.to_string())
}

/// Compare the bundled app version against the latest GitHub release.
#[tauri::command]
async fn check_for_update() -> Result<model_manager::UpdateInfo, String> {
    model_manager::check_update(env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn install_update(
    tag: String,
    on_event: tauri::ipc::Channel<model_manager::UpdateProgress>,
) -> Result<(), String> {
    model_manager::download_and_install_update(tag, on_event)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn open_external_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ─── Local Ollama ─────────────────────────────────────────────────────────

/// Returns local Ollama daemon status (running + version, or error).
#[tauri::command]
async fn check_local_ollama() -> model_manager::LocalStatus {
    model_manager::local_status().await
}

/// List models available locally (Ollama models + raw .gguf files).
#[tauri::command]
async fn list_models(app: tauri::AppHandle) -> Result<Vec<model_manager::ModelInfo>, String> {
    let app_dir = paths::app_dir(&app);
    model_manager::list_local(app_dir).await.map_err(|e| e.to_string())
}

struct PullState(Mutex<Option<watch::Sender<bool>>>);
struct ChatState(Mutex<Option<watch::Sender<bool>>>);

/// Stream `ollama pull <name>` progress to the frontend.
#[tauri::command]
async fn pull_model(
    repo_id: String,
    filename: String,
    on_event: Channel<model_manager::PullProgress>,
    state: tauri::State<'_, PullState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let (tx, rx) = watch::channel(false);
    *state.0.lock().await = Some(tx);
    let app_dir = paths::app_dir(&app);
    let result = model_manager::pull_model(repo_id, filename, app_dir, on_event, rx).await;
    *state.0.lock().await = None;
    result.map_err(|e| e.to_string())
}

/// Cancel an in-progress model download.
#[tauri::command]
async fn cancel_pull(state: tauri::State<'_, PullState>) -> Result<(), String> {
    if let Some(tx) = state.0.lock().await.take() {
        let _ = tx.send(true);
    }
    Ok(())
}

/// Check if the bundled llama.cpp engine is available and running.
#[tauri::command]
async fn engine_status(app: tauri::AppHandle) -> llama_engine::EngineStatus {
    let app_dir = paths::app_dir(&app);
    llama_engine::engine_status(&app_dir).await
}

/// Download / setup the bundled llama-server engine binary.
#[tauri::command]
async fn setup_engine(app: tauri::AppHandle) -> Result<String, String> {
    let app_dir = paths::app_dir(&app);
    let path = llama_engine::find_or_download_llama_server(&app_dir)
        .await
        .map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

/// Stop the llama-server engine process.
#[tauri::command]
fn stop_engine() {
    llama_engine::stop_server();
}

/// Stream a chat completion from the user's local Ollama.
#[tauri::command]
async fn chat_stream(
    model: String,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<model_manager::OllamaToolDef>>,
    on_event: Channel<ChatStreamChunk>,
    state: tauri::State<'_, ChatState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let (tx, rx) = watch::channel(false);
    *state.0.lock().await = Some(tx);
    let app_dir = paths::app_dir(&app);
    let result = provider_manager::ProviderManager::route_chat(
        "ollama",
        model,
        messages,
        tools,
        on_event,
        rx,
        app_dir,
    )
    .await;
    *state.0.lock().await = None;
    result.map_err(|e| e.to_string())
}

/// Stop an ongoing chat completion by aborting the HTTP stream.
#[tauri::command]
async fn cancel_chat(state: tauri::State<'_, ChatState>) -> Result<(), String> {
    if let Some(tx) = state.0.lock().await.take() {
        let _ = tx.send(true);
    }
    Ok(())
}

/// List all downloaded raw `.gguf` files kept in the local models folder.
#[tauri::command]
async fn list_local_ggufs(app: tauri::AppHandle) -> Result<Vec<model_manager::GgufFile>, String> {
    let app_dir = paths::app_dir(&app);
    model_manager::list_local_ggufs(app_dir).await.map_err(|e| e.to_string())
}

/// Delete a specific downloaded `.gguf` file to free up disk space.
#[tauri::command]
async fn delete_local_gguf(filename: String, app: tauri::AppHandle) -> Result<(), String> {
    let app_dir = paths::app_dir(&app);
    model_manager::delete_local_gguf(filename, app_dir).await.map_err(|e| e.to_string())
}

/// Safely move a `.gguf` file to an arbitrary location.
#[tauri::command]
async fn move_local_gguf(filename: String, destination: String, app: tauri::AppHandle) -> Result<(), String> {
    let app_dir = paths::app_dir(&app);
    model_manager::move_local_gguf(filename, destination, app_dir).await.map_err(|e| e.to_string())
}

/// Import a `.gguf` file from anywhere on disk into the local Ollama instance.
#[tauri::command]
async fn import_local_gguf(source_path: String, model_name: String, app: tauri::AppHandle) -> Result<String, String> {
    let app_dir = paths::app_dir(&app);
    model_manager::import_local_gguf(source_path, model_name, app_dir).await.map_err(|e| e.to_string())
}

/// Activate a `.gguf` file that is already inside the local managed models folder.
#[tauri::command]
async fn activate_managed_gguf(filename: String, model_name: String, app: tauri::AppHandle) -> Result<String, String> {
    let app_dir = paths::app_dir(&app);
    model_manager::activate_managed_gguf(filename, model_name, app_dir).await.map_err(|e| e.to_string())
}

/// Delete a model from the local Ollama instance.
#[tauri::command]
async fn delete_ollama_model(name: String) -> Result<(), String> {
    model_manager::delete_ollama_model(&name).await.map_err(|e| e.to_string())
}

// ─── Hardware ─────────────────────────────────────────────────────────────

#[tauri::command]
fn detect_hardware() -> HardwareInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_else(|| "Unknown CPU".into());
    let cpu_cores = System::physical_core_count().unwrap_or(sys.cpus().len());

    HardwareInfo {
        os: System::name().unwrap_or_else(|| "unknown".into()),
        os_version: System::os_version().unwrap_or_else(|| "unknown".into()),
        cpu_brand,
        cpu_cores,
        total_ram_mb: sys.total_memory() / 1024 / 1024,
        gpus: hardware::detect_gpus(),
    }
}

#[tauri::command]
async fn sample_usage() -> Result<hardware::UsageSample, String> {
    // Refreshing sysinfo touches /proc-equivalents and the PDH query blocks on
    // the driver, so keep it off the async runtime's worker thread.
    tokio::task::spawn_blocking(hardware::sample_usage)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_text_file(path: String, content: String) -> Result<(), String> {
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| format!("Failed to save file to {path}: {e}"))
}

#[tauri::command]
fn db_set_kv(key: String, value: String, state: tauri::State<'_, secure_db::SecureDbState>) -> Result<(), String> {
    state.get()?.set_kv(&key, &value).map_err(|e| e.to_string())
}

#[tauri::command]
fn db_get_kv(key: String, state: tauri::State<'_, secure_db::SecureDbState>) -> Result<Option<String>, String> {
    state.get()?.get_kv(&key).map_err(|e| e.to_string())
}

#[tauri::command]
fn db_delete_kv(key: String, state: tauri::State<'_, secure_db::SecureDbState>) -> Result<(), String> {
    state.get()?.delete_kv(&key).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Remove any stale temp files left by interrupted downloads.
    let tmp = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            let n = entry.file_name();
            let s = n.to_string_lossy();
            if s.starts_with("cerberus-") && s.ends_with(".gguf") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    tauri::Builder::default()
        // Must be registered before anything that logs. `targets` replaces the
        // plugin's defaults rather than adding to them — the default set is
        // [Stdout, LogDir], so appending here would write every line twice.
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stderr),
                ])
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(mcp_bridge::McpState::new())
        .manage(PullState(Mutex::new(None)))
        .manage(ChatState(Mutex::new(None)))
        .setup(|app| {
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window_vibrancy::apply_mica(&window, Some(true));
            }
            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main") {
                let _ = window_vibrancy::apply_vibrancy(&window, window_vibrancy::NSVisualEffectMaterial::HudWindow, None, None);
            }

            let app_dir = paths::app_dir(app);

            // Carry a pre-rename install across before anything opens storage.
            // A failure here has to stop `SecureDb::new` rather than warn past
            // it: a missing master key looks exactly like a first run, and the
            // first run mints a fresh key, which would leave every existing row
            // encrypted under a key that no longer exists anywhere.
            let db = app
                .path()
                .home_dir()
                .map_err(anyhow::Error::from)
                .and_then(|home| migrate::run(&home, &app_dir))
                .and_then(|()| secure_db::SecureDb::new(app_dir));
            if let Err(e) = &db {
                log::error!("Secure storage unavailable, settings will not persist: {e:#}");
            }
            // Registered even on failure so `db_*` commands can report the cause.
            app.manage(secure_db::SecureDbState::new(db));

            // First-run Ollama tuning (Windows only). No-op after the first
            // successful application; safe to call every launch.
            tauri::async_runtime::spawn(async {
                let changed = tokio::task::spawn_blocking(tuning::apply_first_run_tuning)
                    .await
                    .unwrap_or(false);
                if changed {
                    log::info!(
                        "applied first-run Ollama tuning; user should restart Ollama \
                         to pick up the new keep-alive / flash-attention settings"
                    );
                }
            });

            // Log Ollama daemon version once at startup so it shows up in
            // crash reports and support tickets without depending on the user
            // opening the right UI panel.
            tauri::async_runtime::spawn(async {
                let s = model_manager::local_status().await;
                if s.running {
                    log::info!(
                        "ollama daemon detected: version={} (helix desktop v{})",
                        s.version.as_deref().unwrap_or("unknown"),
                        env!("CARGO_PKG_VERSION")
                    );
                } else {
                    log::warn!(
                        "ollama daemon NOT running on startup ({}); helix desktop v{}",
                        s.error.as_deref().unwrap_or("no detail"),
                        env!("CARGO_PKG_VERSION")
                    );
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            save_text_file,
            open_external_url,
            search_huggingface,
            list_huggingface_files,
            check_for_update,
            install_update,
            check_local_ollama,
            list_models,
            pull_model,
            cancel_pull,
            engine_status,
            setup_engine,
            stop_engine,
            chat_stream,
            cancel_chat,
            detect_hardware,
            sample_usage,
            list_local_ggufs,
            delete_local_gguf,
            move_local_gguf,
            import_local_gguf,
            activate_managed_gguf,
            delete_ollama_model,
            db_set_kv,
            db_get_kv,
            db_delete_kv,
            mcp_bridge::load_mcp_config,
            mcp_bridge::get_bundled_skills_server,
            mcp_bridge::search_awesome_skills,
            mcp_bridge::install_awesome_skill,
            mcp_bridge::spawn_mcp_server,
            mcp_bridge::send_mcp_message,
            mcp_bridge::kill_mcp_server,
            file_browser::fb_quick_dirs,
            file_browser::fb_list_dir,
            file_browser::fb_read_base64,
            file_browser::fb_read_text,
            file_browser::library_list,
            file_browser::library_save,
            file_browser::library_import_path,
            file_browser::library_read_base64,
            file_browser::library_read_text,
            file_browser::library_delete,
            browser::browser_open,
            browser::browser_back,
            browser::browser_forward,
            browser::browser_reload,
            browser::browser_get_url,
            browser::browser_close,
            browser::browser_extract_text,
            browser::browser_screenshot,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if matches!(event, tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }) {
                llama_engine::stop_server();
            }
        });
}
