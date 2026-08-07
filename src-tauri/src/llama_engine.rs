//! Standalone llama.cpp engine manager.
//! Runs local GGUF models independently via `llama-server` on localhost:11435
//! without needing Ollama or any external services.
//!
//! On first use the engine automatically downloads a pre-built llama-server
//! binary from the llama.cpp GitHub releases, extracts it into
//! `~/.HELIX/bin/`, and manages the process lifecycle.

use crate::model_manager::OllamaToolDef;
use crate::proc::NoWindow;
use crate::{ChatMessage, ChatStreamChunk, ToolCallChunk, ToolCallFunction};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tauri::ipc::Channel;

static ACTIVE_SERVER: StdMutex<Option<ActiveLlamaServer>> = StdMutex::new(None);

struct ActiveLlamaServer {
    model_path: PathBuf,
    child: Child,
}

const SERVER_PORT: u16 = 11435;
const SERVER_URL: &str = "http://127.0.0.1:11435";

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

fn pick_asset_suffix() -> &'static str {
    if cfg!(target_os = "windows") {
        if cfg!(target_arch = "x86_64") {
            // Vulkan build works on NVIDIA + AMD + Intel on Windows
            "bin-win-vulkan-x64.zip"
        } else {
            "bin-win-arm64.zip"
        }
    } else if cfg!(target_os = "macos") {
        "bin-macos-arm64.zip"
    } else {
        // Linux x86_64
        "bin-ubuntu-x64.zip"
    }
}

/// First release that publishes an asset ending in `suffix`.
///
/// Split out from the network call so the "skip incomplete releases" rule can
/// be tested without GitHub.
fn pick_release(releases: Vec<GithubRelease>, suffix: &str) -> Option<(String, String, String)> {
    for release in releases {
        let tag = release.tag_name;
        if let Some(asset) = release.assets.into_iter().find(|a| a.name.ends_with(suffix)) {
            return Some((tag, asset.name, asset.browser_download_url));
        }
    }
    None
}

/// Resolve a llama.cpp release that actually ships a binary for this platform.
///
/// `/releases/latest` alone is not enough. llama.cpp publishes the release tag
/// first and uploads its ~25 platform archives over the following minutes, so
/// the newest tag routinely carries none of them yet. The previous code read
/// that as "nothing found" and fell back to a hardcoded `b4800`, an early-2025
/// build — which is how this install ended up on an engine old enough that the
/// `-fa` spelling used above did not exist yet, and local chat could not start
/// at all.
///
/// Scanning back a few releases lands on a complete one within a build or two.
async fn fetch_latest_release_info(client: &reqwest::Client) -> Result<(String, String, String), anyhow::Error> {
    let suffix = pick_asset_suffix();
    let api_url = "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10";
    let resp = client
        .get(api_url)
        .header("User-Agent", "HELIX-Desktop")
        .header("Accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "GitHub returned HTTP {} while listing llama.cpp releases",
            resp.status()
        );
    }

    pick_release(resp.json::<Vec<GithubRelease>>().await?, suffix)
        .ok_or_else(|| anyhow::anyhow!("No recent llama.cpp release publishes a `{suffix}` build"))
}

/// Whether a file from the engine archive belongs in `bin/`.
///
/// The archive carries ~25 tools (`llama-cli`, `llama-bench`, …) and only the
/// server is wanted, plus every shared library, since that is where llama.cpp
/// keeps the actual compute backends. Used both to filter the extraction and to
/// decide what a previous install left behind, so the two can never disagree
/// about which files the engine owns.
///
/// `name` must already be lowercased.
fn is_engine_artifact(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    name.starts_with("llama-server") || matches!(ext, "dll" | "so" | "dylib")
}

/// Delete the files a previous install extracted into `bin_dir`.
///
/// Scoped to what `is_engine_artifact` claims, so anything else in the
/// directory is untouched. `version.txt` is rewritten by the caller on success
/// and deliberately left in place here: if extraction fails, it still records
/// which build the leftovers came from. `keep` is the freshly downloaded
/// archive, which lives in this directory until it is unpacked.
async fn remove_installed_engine_files(bin_dir: &Path, keep: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(bin_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path == keep || !is_engine_artifact(&entry.file_name().to_string_lossy().to_lowercase()) {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            // Usually a file still mapped by a process that outlived
            // `stop_server`. Extraction overwrites what it ships, so the risk
            // is a stale library surviving — worth a line in the log, not worth
            // aborting an otherwise good install.
            log::warn!("Could not remove old engine file {}: {e}", path.display());
        }
    }
}

/// Download and extract llama-server into `app_dir/bin/`.
/// Returns the path to the extracted `llama-server` binary.
async fn download_llama_server(app_dir: &Path) -> Result<PathBuf, anyhow::Error> {
    let bin_dir = app_dir.join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;

    let bin_name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let target_bin = bin_dir.join(bin_name);
    let version_file = bin_dir.join("version.txt");

    let current_version = tokio::fs::read_to_string(&version_file)
        .await
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;

    let (tag_name, asset_name, download_url) = match fetch_latest_release_info(&client).await {
        Ok(info) => info,
        Err(e) => {
            // Offline, rate-limited, or mid-upload. An engine that is already
            // installed still runs, and refusing to chat because the update
            // check failed would be a worse outcome than skipping the check.
            if tokio::fs::metadata(&target_bin).await.is_ok() {
                log::warn!("Could not check for a newer llama-server ({e}); using the installed build.");
                return Ok(target_bin);
            }
            return Err(e.context("llama-server is not installed and no release could be resolved"));
        }
    };

    // Already downloaded with matching release version?
    if !current_version.trim().is_empty()
        && current_version.trim() == tag_name
        && tokio::fs::metadata(&target_bin).await.is_ok()
    {
        return Ok(target_bin);
    }

    // The existing install is left running and intact until the new archive is
    // actually in hand — a download that fails halfway should cost the user an
    // update, not their working engine.
    log::info!("Downloading llama-server release {} from {}...", tag_name, download_url);

    let resp = client
        .get(&download_url)
        .header("User-Agent", "HELIX-Desktop")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to download llama-server: HTTP {} from {}",
            resp.status(),
            download_url
        ));
    }

    let bytes = resp.bytes().await?;
    let zip_path = bin_dir.join(&asset_name);
    tokio::fs::write(&zip_path, &bytes).await?;

    // Now that the replacement is on disk, clear the old build out. Extracting
    // over it is not enough: llama.cpp discovers its compute backends by
    // scanning for `ggml-*` shared libraries next to the binary, so a library
    // from a previous release is not inert — it gets probed, and an ABI
    // mismatch there shows up as a crash or a silently wrong backend instead of
    // a version error. Release file sets differ enough for this to bite (b4800
    // shipped one `ggml-cpu.dll`; b10295 ships fifteen CPU variants and nothing
    // by that name), so overwriting in place always strands something.
    stop_server();
    remove_installed_engine_files(&bin_dir, &zip_path).await;

    // Extract the zip
    let zip_path_clone = zip_path.clone();
    let bin_dir_clone = bin_dir.clone();
    tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
        let file = std::fs::File::open(&zip_path_clone)?;
        let mut archive = zip::ZipArchive::new(file)?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let outpath = match entry.enclosed_name() {
                Some(p) => p.to_owned(),
                None => continue,
            };

            let fname = outpath.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();

            if entry.is_dir() || !is_engine_artifact(&fname.to_lowercase()) {
                continue;
            }

            let dest = bin_dir_clone.join(&fname);
            let mut outfile = std::fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut outfile)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        Ok(())
    })
    .await??;

    // Clean up zip
    let _ = tokio::fs::remove_file(&zip_path).await;

    if tokio::fs::metadata(&target_bin).await.is_ok() {
        let _ = tokio::fs::write(&version_file, &tag_name).await;
        log::info!("llama-server {} installed successfully to {}", tag_name, target_bin.display());
        Ok(target_bin)
    } else {
        Err(anyhow::anyhow!(
            "Failed to extract llama-server binary from {}. Expected binary at: {}",
            asset_name,
            target_bin.display()
        ))
    }
}

/// Find or auto-download `llama-server` binary.
pub async fn find_or_download_llama_server(app_dir: &Path) -> Result<PathBuf, anyhow::Error> {
    download_llama_server(app_dir).await
}

/// Whether this `llama-server` build's `-fa` flag takes a value.
///
/// `-fa` was a plain boolean switch for most of llama.cpp's history and only
/// later became `--flash-attn [on|off|auto]`. Handing `-fa auto` to an older
/// build makes it exit immediately with `error: invalid argument: auto`, which
/// takes down chat for *every* model rather than degrading quietly — so the
/// flag is matched to whatever binary is actually installed.
///
/// The result is cached because `--help` initialises the GPU backend, which is
/// far too slow to repeat on every spawn. Unreadable help is treated as "no
/// value": bare `-fa` is accepted by both spellings, so it is the safe guess.
fn flash_attn_takes_value(server_bin: &Path) -> bool {
    static CACHE: StdMutex<Option<(PathBuf, bool)>> = StdMutex::new(None);

    if let Some((path, takes_value)) = CACHE.lock().unwrap().as_ref() {
        if path == server_bin {
            return *takes_value;
        }
    }

    let takes_value = Command::new(server_bin)
        .arg("--help")
        .no_window()
        .output()
        .ok()
        .map(|out| {
            let help = String::from_utf8_lossy(&out.stdout);
            help.lines()
                .find(|line| line.contains("--flash-attn"))
                // The valued form documents its argument as `[on|off|auto]`;
                // the boolean form has nothing between flag and description.
                .is_some_and(|line| line.contains('['))
        })
        .unwrap_or(false);

    *CACHE.lock().unwrap() = Some((server_bin.to_path_buf(), takes_value));
    takes_value
}

fn spawn_llama_server(server_bin: &Path, model_path: &Path, ngl: u32) -> Result<Child, anyhow::Error> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1).max(1))
        .unwrap_or(4);

    let mut cmd = Command::new(server_bin);
    cmd.arg("-m")
       .arg(model_path)
       .arg("--port")
       .arg(SERVER_PORT.to_string())
       .arg("-c")
       .arg("8192")
       .arg("-b")
       .arg("2048")
       .arg("-ub")
       .arg("512")
       .arg("-t")
       .arg(threads.to_string())
       .arg("-ngl")
       .arg(ngl.to_string())
       .arg("-np")
       .arg("1")
       .arg("--mmap");

    if flash_attn_takes_value(server_bin) {
        cmd.arg("-fa").arg("auto");
    } else {
        cmd.arg("-fa");
    }

    cmd.stdout(Stdio::piped())
       .stderr(Stdio::piped())
       .no_window();

    cmd.spawn().map_err(|e| anyhow::anyhow!("Failed to start llama-server: {e}"))
}

/// Ensure a `llama-server` process is running for the specified `.gguf` file.
pub async fn ensure_server(model_path: &Path, app_dir: &Path) -> Result<(), anyhow::Error> {
    if !model_path.exists() {
        return Err(anyhow::anyhow!(
            "Model file not found at path: {}. Please pull or import the model first.",
            model_path.display()
        ));
    }

    // A projector is the image-encoder half of a vision model, not something
    // that can be loaded on its own. Caught here because `llama-server` reports
    // it as "unsupported model architecture: 'clip'" and exits, which reads like
    // an engine fault rather than the wrong file being selected.
    if let Some(name) = model_path.file_name().and_then(|n| n.to_str()) {
        if crate::model_manager::is_projector_gguf(name) {
            return Err(anyhow::anyhow!(
                "{name} is a multimodal projector, not a chat model. It only works \
                 alongside its matching vision model. Pick a different model."
            ));
        }
    }

    // Check if server is already running with the exact same model
    let need_spawn = {
        let mut lock = ACTIVE_SERVER.lock().unwrap();
        if let Some(active) = lock.as_mut() {
            if active.model_path == model_path {
                match active.child.try_wait() {
                    Ok(None) => false, // Still running fine
                    _ => {
                        lock.take(); // Dead process, clean up
                        true
                    }
                }
            } else {
                // Different model requested — kill previous server process
                let _ = active.child.kill();
                let _ = active.child.wait();
                lock.take();
                true
            }
        } else {
            true
        }
    };

    if !need_spawn {
        return Ok(());
    }

    // Kill any orphan llama-server instances from previous runs holding file locks
    kill_orphan_llama_servers();
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Find or auto-download binary
    let server_bin = find_or_download_llama_server(app_dir).await?;

    // On Windows, unblock file (remove Zone.Identifier stream) to bypass App Control / SmartScreen blocks
    #[cfg(windows)]
    {
        let zone_file = format!("{}:Zone.Identifier", server_bin.display());
        let _ = std::fs::remove_file(zone_file);
    }

    // Try starting with GPU offload (-ngl 99) first, then fallback to CPU (-ngl 0) if GPU fails
    let ngl_options = [99, 0];
    let mut last_error = String::new();

    for &ngl in &ngl_options {
        let mut child = match spawn_llama_server(&server_bin, model_path, ngl) {
            Ok(c) => c,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };

        // Wait for server health endpoint to respond
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(120); // 120s timeout for cold model loading
        let mut process_crashed = false;

        while start.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Check if child process crashed
            match child.try_wait() {
                Ok(Some(status)) => {
                    process_crashed = true;
                    let mut stderr_output = String::new();
                    if let Some(mut stderr_pipe) = child.stderr.take() {
                        use std::io::Read;
                        let _ = stderr_pipe.read_to_string(&mut stderr_output);
                    }
                    let err_msg = stderr_output.trim();
                    last_error = if err_msg.is_empty() {
                        format!("llama-server exited with status {status}")
                    } else {
                        format!("llama-server exited with status {status}: {err_msg}")
                    };
                    break;
                }
                Err(e) => {
                    process_crashed = true;
                    last_error = format!("Error checking llama-server: {e}");
                    break;
                }
                Ok(None) => {} // Still running/loading
            }

            if let Ok(resp) = client.get(format!("{SERVER_URL}/health")).send().await {
                if resp.status().is_success() {
                    let mut lock = ACTIVE_SERVER.lock().unwrap();
                    *lock = Some(ActiveLlamaServer {
                        model_path: model_path.to_path_buf(),
                        child,
                    });
                    return Ok(());
                }
            }
        }

        if !process_crashed {
            let _ = child.kill();
            let _ = child.wait();
            last_error = "`llama-server` timed out while loading model into memory".to_string();
        }
    }

    Err(anyhow::anyhow!("Failed to start llama-server engine: {last_error}"))
}

// ─── OpenAI-compatible request shapes ──────────────────────────────────────

#[derive(Serialize)]
struct OpenAiImageUrl {
    url: String,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum OpenAiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OpenAiImageUrl },
}

#[derive(Serialize)]
#[serde(untagged)]
enum OpenAiContent {
    Text(String),
    Parts(Vec<OpenAiContentPart>),
}

#[derive(Serialize)]
struct OpenAiToolCallFunction {
    name: String,
    /// OpenAI transports the arguments as a JSON *string*, not an object.
    arguments: String,
}

#[derive(Serialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolCallFunction,
}

#[derive(Serialize)]
struct OpenAiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OpenAiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct OpenAiChatReq<'a> {
    messages: Vec<OpenAiMessage>,
    stream: bool,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [OllamaToolDef]>,
}

/// Wrap a raw base64 image in the data URL that the OpenAI content part wants.
/// Sniffs the container from the base64 prefix so PNG screenshots aren't
/// mislabelled as JPEG.
fn to_data_url(image: &str) -> String {
    if image.starts_with("data:") {
        return image.to_string();
    }
    let mime = if image.starts_with("iVBOR") {
        "image/png"
    } else if image.starts_with("R0lGOD") {
        "image/gif"
    } else if image.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    format!("data:{mime};base64,{image}")
}

/// Translate HELIX chat history into the OpenAI wire format.
///
/// The frontend emits `role: "tool"` messages with no `tool_call_id` (Ollama
/// doesn't need one), so pair each tool result with the ids minted for the
/// preceding assistant turn — llama-server rejects orphaned tool messages.
fn to_openai_messages(messages: &[ChatMessage]) -> Vec<OpenAiMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut unclaimed_ids: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    for (i, m) in messages.iter().enumerate() {
        if m.role == "tool" {
            out.push(OpenAiMessage {
                role: "tool".to_string(),
                content: Some(OpenAiContent::Text(m.content.clone())),
                tool_calls: None,
                tool_call_id: unclaimed_ids.pop_front(),
            });
            continue;
        }

        let tool_calls = m.tool_calls.as_ref().filter(|tcs| !tcs.is_empty()).map(|tcs| {
            unclaimed_ids.clear();
            tcs.iter()
                .enumerate()
                .map(|(j, tc)| {
                    let id = tc.id.clone().unwrap_or_else(|| format!("call_{i}_{j}"));
                    unclaimed_ids.push_back(id.clone());
                    OpenAiToolCall {
                        id,
                        kind: "function",
                        function: OpenAiToolCallFunction {
                            name: tc.function.name.clone(),
                            arguments: match &tc.function.arguments {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            },
                        },
                    }
                })
                .collect()
        });

        let content = match m.images.as_ref().filter(|imgs| !imgs.is_empty()) {
            Some(images) => {
                let mut parts = Vec::with_capacity(images.len() + 1);
                if !m.content.is_empty() {
                    parts.push(OpenAiContentPart::Text {
                        text: m.content.clone(),
                    });
                }
                for img in images {
                    parts.push(OpenAiContentPart::ImageUrl {
                        image_url: OpenAiImageUrl {
                            url: to_data_url(img),
                        },
                    });
                }
                Some(OpenAiContent::Parts(parts))
            }
            // An assistant turn that only calls tools legitimately has no text.
            None if m.content.is_empty() && tool_calls.is_some() => None,
            None => Some(OpenAiContent::Text(m.content.clone())),
        };

        out.push(OpenAiMessage {
            role: m.role.clone(),
            content,
            tool_calls,
            tool_call_id: None,
        });
    }

    out
}

// ─── OpenAI-compatible stream shapes ───────────────────────────────────────

#[derive(Deserialize)]
struct StreamToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct StreamToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamToolCallFunction>,
}

#[derive(Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

/// llama.cpp appends its own generation stats to the final streamed chunk.
#[derive(Deserialize)]
struct StreamTimings {
    #[serde(default)]
    predicted_n: Option<f64>,
    #[serde(default)]
    predicted_ms: Option<f64>,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    timings: Option<StreamTimings>,
}

/// Tool calls arrive as fragments keyed by `index`; the name lands in the first
/// fragment and the arguments accumulate one string piece at a time.
#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn finish(self) -> ToolCallChunk {
        let arguments = serde_json::from_str(self.arguments.trim())
            .unwrap_or_else(|_| serde_json::json!({}));
        ToolCallChunk {
            id: self.id,
            function: ToolCallFunction {
                name: self.name,
                arguments,
            },
        }
    }
}

/// Stream chat completions from the local `llama-server` endpoint.
pub async fn stream_chat_llama(
    model_path: PathBuf,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<OllamaToolDef>>,
    on_event: Channel<ChatStreamChunk>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    app_dir: PathBuf,
) -> Result<(), anyhow::Error> {
    // Ensure llama-server is running for this GGUF file
    ensure_server(&model_path, &app_dir).await?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(600))
        .build()?;

    let body = OpenAiChatReq {
        messages: to_openai_messages(&messages),
        stream: true,
        temperature: 0.7,
        tools: tools.as_deref().filter(|t| !t.is_empty()),
    };

    let resp = client
        .post(format!("{SERVER_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("llama-server returned HTTP {status}: {err_text}"));
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    let start = std::time::Instant::now();
    let mut first_token_at: Option<std::time::Instant> = None;
    let mut ttft_ms: Option<u64> = None;
    let mut emitted_tokens: u64 = 0;
    let mut server_timings: Option<StreamTimings> = None;
    let mut partial_tool_calls: Vec<PartialToolCall> = Vec::new();

    'stream: loop {
        tokio::select! {
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    let _ = on_event.send(ChatStreamChunk {
                        delta: String::new(),
                        done: true,
                        error: None,
                        ttft_ms,
                        tps: None,
                        tool_calls: None,
                    });
                    return Ok(());
                }
            }
            item = stream.next() => {
                match item {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].trim().to_string();
                            buffer.drain(..=pos);

                            if line.is_empty() || line.starts_with(':') {
                                continue;
                            }
                            let Some(data) = line.strip_prefix("data: ") else {
                                continue;
                            };
                            let data = data.trim();
                            if data == "[DONE]" {
                                break 'stream;
                            }

                            let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
                                continue;
                            };
                            if chunk.timings.is_some() {
                                server_timings = chunk.timings;
                            }

                            let Some(choice) = chunk.choices.first() else {
                                continue;
                            };

                            for tc in choice.delta.tool_calls.iter().flatten() {
                                let idx = tc.index.unwrap_or(0);
                                if partial_tool_calls.len() <= idx {
                                    partial_tool_calls.resize_with(idx + 1, PartialToolCall::default);
                                }
                                let slot = &mut partial_tool_calls[idx];
                                if let Some(id) = &tc.id {
                                    slot.id = Some(id.clone());
                                }
                                if let Some(f) = &tc.function {
                                    if let Some(name) = &f.name {
                                        slot.name.push_str(name);
                                    }
                                    if let Some(args) = &f.arguments {
                                        slot.arguments.push_str(args);
                                    }
                                }
                            }

                            let delta = choice.delta.content.clone().unwrap_or_default();
                            if ttft_ms.is_none()
                                && (!delta.is_empty() || choice.delta.tool_calls.is_some())
                            {
                                first_token_at = Some(std::time::Instant::now());
                                ttft_ms = Some(start.elapsed().as_millis() as u64);
                            }

                            if !delta.is_empty() {
                                // llama-server emits one token per event, so
                                // this doubles as the fallback token count.
                                emitted_tokens += 1;
                                let _ = on_event.send(ChatStreamChunk {
                                    delta,
                                    done: false,
                                    error: None,
                                    ttft_ms,
                                    tps: None,
                                    tool_calls: None,
                                });
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => break 'stream,
                }
            }
        }
    }

    let tps = match &server_timings {
        Some(t) => match (t.predicted_n, t.predicted_ms) {
            (Some(n), Some(ms)) if ms > 0.0 => Some(n / (ms / 1000.0)),
            _ => None,
        },
        None => first_token_at.and_then(|t0| {
            let secs = t0.elapsed().as_secs_f64();
            (secs > 0.0 && emitted_tokens > 0).then(|| emitted_tokens as f64 / secs)
        }),
    };

    let tool_calls: Option<Vec<ToolCallChunk>> = {
        let finished: Vec<ToolCallChunk> = partial_tool_calls
            .into_iter()
            .filter(|tc| !tc.name.is_empty())
            .map(PartialToolCall::finish)
            .collect();
        (!finished.is_empty()).then_some(finished)
    };

    let _ = on_event.send(ChatStreamChunk {
        delta: String::new(),
        done: true,
        error: None,
        ttft_ms,
        tps,
        tool_calls,
    });
    Ok(())
}

// ─── Engine lifecycle ──────────────────────────────────────────────────────

#[derive(Debug, Serialize, Clone)]
pub struct EngineStatus {
    pub ready: bool,
    pub binary_found: bool,
    pub active_model: Option<String>,
}

/// Check the current state of the llama engine.
pub async fn engine_status(app_dir: &Path) -> EngineStatus {
    let bin_name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let app_bin = app_dir.join("bin").join(bin_name);
    let binary_found = tokio::fs::metadata(&app_bin).await.is_ok();

    let lock = ACTIVE_SERVER.lock().unwrap();
    let active_model = lock.as_ref().map(|a| a.model_path.display().to_string());
    let ready = lock.is_some();

    EngineStatus {
        ready,
        binary_found,
        active_model,
    }
}

/// Stop any running llama-server process.
pub fn stop_server() {
    let mut lock = ACTIVE_SERVER.lock().unwrap();
    if let Some(mut active) = lock.take() {
        let _ = active.child.kill();
        let _ = active.child.wait();
    }
    kill_orphan_llama_servers();
}

/// Kill any orphaned `llama-server.exe` processes on Windows that may be holding model file locks.
pub fn kill_orphan_llama_servers() {
    #[cfg(target_os = "windows")]
    {
        use crate::proc::NoWindow;
        let _ = Command::new("taskkill")
            .args(["/F", "/IM", "llama-server.exe"])
            .no_window()
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact situation that stranded this install on a 2025-era engine:
    /// llama.cpp had just tagged b10297 and uploaded only the CUDA runtime
    /// archive, with the platform builds still going up. Anything that reads
    /// "newest tag" and stops there finds no usable asset.
    fn releases_mid_upload() -> Vec<GithubRelease> {
        serde_json::from_str(
            r#"[
                {
                    "tag_name": "b10297",
                    "assets": [
                        {
                            "name": "cudart-llama-bin-win-cuda-12.4-x64.zip",
                            "browser_download_url": "https://example.invalid/cudart.zip"
                        }
                    ]
                },
                {
                    "tag_name": "b10296",
                    "assets": []
                },
                {
                    "tag_name": "b10295",
                    "assets": [
                        {
                            "name": "llama-b10295-bin-win-cuda-12.4-x64.zip",
                            "browser_download_url": "https://example.invalid/cuda.zip"
                        },
                        {
                            "name": "llama-b10295-bin-win-vulkan-x64.zip",
                            "browser_download_url": "https://example.invalid/vulkan.zip"
                        }
                    ]
                }
            ]"#,
        )
        .expect("fixture must parse as the GitHub releases shape")
    }

    #[test]
    fn skips_releases_whose_platform_asset_has_not_uploaded_yet() {
        let (tag, asset, url) = pick_release(releases_mid_upload(), "bin-win-vulkan-x64.zip")
            .expect("a complete release exists further down the list");

        assert_eq!(tag, "b10295", "must skip the incomplete newest release");
        assert_eq!(asset, "llama-b10295-bin-win-vulkan-x64.zip");
        assert_eq!(url, "https://example.invalid/vulkan.zip");
    }

    /// Returning `None` is what makes the caller keep an already-installed
    /// engine instead of downloading something wrong. The predecessor of this
    /// code fell back to a hardcoded `b4800` here, which is how an engine
    /// predating the current `-fa` spelling got installed over a working one.
    #[test]
    fn reports_nothing_rather_than_guessing_when_no_release_matches() {
        assert!(pick_release(releases_mid_upload(), "bin-macos-arm64.zip").is_none());
    }

    /// The filter decides both what gets extracted and what an upgrade sweeps
    /// away, so it is worth pinning against the names a real archive contains.
    #[test]
    fn engine_artifact_filter_matches_what_the_archive_ships() {
        for name in [
            "llama-server.exe",
            "llama-server-impl.dll",
            "ggml-cpu.dll",
            "ggml-cpu-zen4.dll",
            "ggml-vulkan.dll",
            "llama.dll",
            "libcurl-x64.dll",
            "libggml.so",
            "libllama.dylib",
        ] {
            assert!(is_engine_artifact(name), "{name} should be engine-owned");
        }

        // Other tools in the archive, and files that are not the engine's.
        for name in [
            "llama-cli.exe",
            "llama-bench.exe",
            "ggml-rpc-server.exe",
            "version.txt",
            "llama-b10295-bin-win-vulkan-x64.zip",
            "mistral-7b-q4_k_m.gguf",
        ] {
            assert!(!is_engine_artifact(name), "{name} should be left alone");
        }
    }

    /// Upgrades used to extract straight over the previous install. Releases
    /// ship different file sets — b4800's single `ggml-cpu.dll` has no
    /// counterpart in b10295 — so the old library survived, and llama.cpp
    /// probes every `ggml-*` next to the binary rather than ignoring strangers.
    #[tokio::test]
    async fn upgrade_clears_the_previous_build() {
        let dir = std::env::temp_dir().join("helix-engine-sweep-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let stale = ["llama-server.exe", "ggml-cpu.dll", "ggml-vulkan.dll"];
        let kept = ["version.txt", "notes.txt"];
        let archive = dir.join("llama-b10295-bin-win-vulkan-x64.zip");

        for name in stale.iter().chain(kept.iter()) {
            std::fs::write(dir.join(name), b"x").expect("plant file");
        }
        std::fs::write(&archive, b"zip").expect("plant archive");

        remove_installed_engine_files(&dir, &archive).await;

        for name in stale {
            assert!(!dir.join(name).exists(), "{name} should have been removed");
        }
        for name in kept {
            assert!(dir.join(name).exists(), "{name} should have been kept");
        }
        assert!(
            archive.exists(),
            "the archive being installed must survive the sweep that precedes unpacking it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full first-run path — resolve, download, extract, record the version
    /// — against the real GitHub releases, into a throwaway directory.
    ///
    /// Ignored by default: it needs the network and pulls a ~100 MB archive.
    /// Run it deliberately after touching anything in the install path:
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture
    /// ```
    ///
    /// It asserts what a fresh install actually depends on: the binary lands
    /// where `ensure_server` looks for it, it runs, and it is new enough that
    /// `-fa` takes a value. That last check is the one that matters — a stale
    /// engine still extracts fine and still fails every chat.
    #[tokio::test]
    #[ignore = "downloads ~100 MB from GitHub"]
    async fn downloads_and_installs_a_usable_engine() {
        let app_dir = std::env::temp_dir().join("helix-engine-install-test");
        let _ = std::fs::remove_dir_all(&app_dir);
        std::fs::create_dir_all(&app_dir).expect("temp dir");

        let bin = find_or_download_llama_server(&app_dir)
            .await
            .expect("first-run install must succeed");

        assert!(bin.exists(), "installed binary missing at {}", bin.display());
        assert_eq!(bin, app_dir.join("bin").join(if cfg!(windows) {
            "llama-server.exe"
        } else {
            "llama-server"
        }));

        let tag = std::fs::read_to_string(app_dir.join("bin").join("version.txt"))
            .expect("version.txt records the installed release");
        println!("installed llama-server {}", tag.trim());

        assert!(
            flash_attn_takes_value(&bin),
            "installed engine {} is too old to accept `-fa auto`; the release \
             picker resolved a stale build",
            tag.trim()
        );

        let _ = std::fs::remove_dir_all(&app_dir);
    }
}
