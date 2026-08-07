use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use crate::proc::NoWindow;
use tauri::{AppHandle, Emitter, Manager, State};

// State to hold active MCP child processes
pub struct McpState {
    pub processes: Mutex<HashMap<String, Arc<Mutex<Child>>>>,
}

impl McpState {
    pub fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct McpMessagePayload {
    plugin_id: String,
    message: String,
}

#[derive(Clone, serde::Serialize)]
struct McpErrorPayload {
    plugin_id: String,
    error: String,
}

#[derive(serde::Deserialize)]
struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub cwd: Option<String>,
}

#[derive(serde::Serialize)]
pub struct DiscoveredPlugin {
    pub id: String,
    pub name: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub cwd: Option<String>,
    pub enabled: Option<bool>,
    pub verified: Option<bool>,
    pub verified_at: Option<String>,
    pub verify_error: Option<String>,
}

#[derive(serde::Serialize)]
pub struct AwesomeSkill {
    pub name: String,
    pub description: String,
    pub url: String,
}

struct GitHubSource {
    owner: String,
    repo: String,
    clone_url: String,
    branch: Option<String>,
    subdir: Option<PathBuf>,
}

/// Reads an .mcp.json file and returns the list of configured MCP servers.
#[tauri::command]
pub async fn load_mcp_config(path: String) -> Result<Vec<DiscoveredPlugin>, String> {
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read {}: {}", path, e))?;

    let config: McpConfigFile = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse JSON in {}: {}", path, e))?;

    let mut plugins = Vec::new();
    for (name, server_config) in config.mcp_servers {
        plugins.push(DiscoveredPlugin {
            id: format!("mcp_{}", name),
            name,
            command: server_config.command,
            args: server_config.args,
            env: server_config.env,
            url: server_config.url,
            cwd: server_config.cwd,
            enabled: None,
            verified: None,
            verified_at: None,
            verify_error: None,
        });
    }

    Ok(plugins)
}

fn app_plugins_dir(app: &AppHandle) -> PathBuf {
    crate::paths::app_dir(app).join("plugins")
}

/// Absolute path to the bundled skills server's entry point, if it is present.
///
/// The server ships as a bundled resource in installed builds, but during
/// `tauri dev` the resource directory is the target dir and holds nothing, so
/// the source tree is checked as well. Returns `None` rather than a guess when
/// neither exists — the frontend uses that to hide the built-in server instead
/// of offering the user an entry that could never start.
fn bundled_skills_server(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Executable directory (Windows installer puts _up_ at install root alongside helix-desktop.exe)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(
                exe_dir
                    .join("_up_")
                    .join("skills-server")
                    .join("dist")
                    .join("index.js"),
            );
            candidates.push(
                exe_dir
                    .join("skills-server")
                    .join("dist")
                    .join("index.js"),
            );
        }
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        // Tauri preserves the `../` in a resource glob as an `_up_` segment.
        candidates.push(
            resource_dir
                .join("_up_")
                .join("skills-server")
                .join("dist")
                .join("index.js"),
        );
        candidates.push(
            resource_dir
                .join("skills-server")
                .join("dist")
                .join("index.js"),
        );
        if let Some(parent) = resource_dir.parent() {
            candidates.push(
                parent
                    .join("_up_")
                    .join("skills-server")
                    .join("dist")
                    .join("index.js"),
            );
            candidates.push(
                parent
                    .join("skills-server")
                    .join("dist")
                    .join("index.js"),
            );
        }
    }

    // Development: src-tauri/../skills-server.
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("skills-server")
            .join("dist")
            .join("index.js"),
    );

    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.canonicalize().unwrap_or(p))
}

/// Describes the built-in skills server so it can be offered in Plugin Settings.
///
/// Reported as `enabled: false`: the server exposes `run_command` and
/// `write_file`, so it is opted into deliberately rather than switched on for
/// every model at startup.
#[tauri::command]
pub async fn get_bundled_skills_server(app: AppHandle) -> Result<Option<DiscoveredPlugin>, String> {
    let Some(entry) = bundled_skills_server(&app) else {
        return Ok(None);
    };

    let cwd = entry
        .parent()
        .and_then(|dist| dist.parent())
        .map(|dir| dir.display().to_string());

    // Point the server at the same plugins directory `install_awesome_skill`
    // writes to, so skills installed through the UI are visible to it.
    let mut env = HashMap::new();
    env.insert(
        "HELIX_PLUGINS_DIR".to_string(),
        app_plugins_dir(&app).display().to_string(),
    );

    Ok(Some(DiscoveredPlugin {
        id: "helix-skills".to_string(),
        name: "HELIX Skills".to_string(),
        command: Some("node".to_string()),
        args: Some(vec![entry.display().to_string()]),
        env: Some(env),
        url: None,
        cwd,
        enabled: Some(false),
        verified: None,
        verified_at: None,
        verify_error: None,
    }))
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() { "plugin".into() } else { trimmed }
}

fn html_decode(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn normalize_github_source(url: &str) -> Result<GitHubSource, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("Invalid GitHub URL: {}", e))?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return Err("Only HTTPS github.com plugin URLs are supported.".into());
    }

    let segments: Vec<String> = parsed
        .path_segments()
        .map(|parts| parts.map(|part| part.trim_end_matches(".git").to_string()).collect())
        .unwrap_or_default();
    if segments.len() < 2 {
        return Err("GitHub URL must include an owner and repository.".into());
    }

    let owner = segments[0].clone();
    let repo = segments[1].clone();
    let clone_url = format!("https://github.com/{owner}/{repo}.git");

    if segments.len() >= 5 && (segments[2] == "tree" || segments[2] == "blob") {
        let subdir = segments[4..].iter().collect::<PathBuf>();
        return Ok(GitHubSource {
            owner,
            repo,
            clone_url,
            branch: Some(segments[3].clone()),
            subdir: Some(subdir),
        });
    }

    Ok(GitHubSource {
        owner,
        repo,
        clone_url,
        branch: None,
        subdir: None,
    })
}

fn json_text(value: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

fn github_url_from_value(value: &serde_json::Value) -> Option<String> {
    for key in ["url", "githubUrl", "github_url", "repoUrl", "repository", "repositoryUrl", "source"] {
        if let Some(url) = value.get(key).and_then(|v| v.as_str()) {
            if url.contains("github.com/") {
                let normalized = if url.starts_with("https://") {
                    url.to_string()
                } else if let Some(pos) = url.find("github.com/") {
                    format!("https://{}", &url[pos..])
                } else {
                    url.to_string()
                };
                return Some(normalized.trim_end_matches('/').trim_end_matches(".git").to_string());
            }
        }
    }
    None
}

fn parse_api_skills_value(value: &serde_json::Value, out: &mut Vec<AwesomeSkill>) {
    if let Some(array) = value.as_array() {
        for item in array {
            parse_api_skills_value(item, out);
        }
        return;
    }

    let Some(object) = value.as_object() else {
        return;
    };

    if let Some(url) = github_url_from_value(value) {
        let fallback_name = url.split('/').rfind(|part| !part.is_empty()).unwrap_or(&url);
        out.push(AwesomeSkill {
            name: json_text(value, &["name", "title", "displayName"]).trim().to_string().if_empty(fallback_name),
            description: json_text(value, &["description", "summary", "excerpt", "readme"]).trim().to_string(),
            url,
        });
    }

    for key in ["skills", "items", "data", "results", "plugins"] {
        if let Some(child) = object.get(key) {
            parse_api_skills_value(child, out);
        }
    }
}

trait EmptyFallback {
    fn if_empty(self, fallback: &str) -> String;
}

impl EmptyFallback for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() { fallback.to_string() } else { self }
    }
}

fn parse_api_skills(text: &str) -> Option<Vec<AwesomeSkill>> {
    let data = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let mut skills = Vec::new();
    parse_api_skills_value(&data, &mut skills);
    if skills.is_empty() { None } else { Some(skills) }
}

fn github_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for marker in ["href=\"", "href='"] {
        let quote = marker.chars().last().unwrap_or('"');
        let mut rest = html;
        while let Some(idx) = rest.find(marker) {
            rest = &rest[idx + marker.len()..];
            if let Some(end) = rest.find(quote) {
                let mut url = html_decode(&rest[..end]);
                if let Some(pos) = url.find("github.com/") {
                    url = format!("https://{}", url[pos..].trim_start_matches('/'));
                }
                if url.starts_with("https://github.com/") {
                    url = url
                        .trim_end_matches('/')
                        .trim_end_matches(".git")
                        .to_string();
                    if seen.insert(url.clone()) {
                        urls.push(url);
                    }
                }
                rest = &rest[end + 1..];
            } else {
                break;
            }
        }
    }
    urls
}

fn parse_json_ld_skills(html: &str) -> Vec<AwesomeSkill> {
    let mut skills = Vec::new();
    let mut rest = html;
    let open = "<script type=\"application/ld+json\">";
    while let Some(start) = rest.find(open) {
        rest = &rest[start + open.len()..];
        if let Some(end) = rest.find("</script>") {
            let json = rest[..end].trim();
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(json) {
                if data.get("@type").and_then(|v| v.as_str()) == Some("ItemList") {
                    if let Some(items) = data.get("itemListElement").and_then(|v| v.as_array()) {
                        for item in items {
                            let inner = item.get("item").unwrap_or(item);
                            if let Some(url) = inner.get("url").and_then(|v| v.as_str()) {
                                if url.starts_with("https://github.com/") {
                                    skills.push(AwesomeSkill {
                                        name: inner.get("name").and_then(|v| v.as_str()).unwrap_or(url).to_string(),
                                        description: inner.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                        url: url.trim_end_matches('/').trim_end_matches(".git").to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            rest = &rest[end + "</script>".len()..];
        } else {
            break;
        }
    }
    skills
}

fn parse_awesome_skills(html: &str) -> Vec<AwesomeSkill> {
    let mut skills = parse_json_ld_skills(html);
    let mut seen: HashSet<String> = skills.iter().map(|s| s.url.clone()).collect();
    for url in github_urls(html) {
        if seen.insert(url.clone()) {
            let repo = url.split('/').rfind(|part| !part.is_empty()).unwrap_or(&url);
            skills.push(AwesomeSkill {
                name: repo.replace(['-', '_'], " "),
                description: "MCP plugin or SKILL.md repo from awesome-skills.com.".into(),
                url,
            });
        }
    }
    skills
}

fn normalize_search(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() || ch.is_ascii_whitespace() {
                Some(ch.to_ascii_lowercase())
            } else if ch == '-' || ch == '_' || ch == '/' {
                Some(' ')
            } else {
                None
            }
        })
        .collect::<String>()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let mut costs: Vec<usize> = (0..=b.chars().count()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut last = i;
        costs[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if ca == cb {
                last
            } else {
                1 + last.min(costs[j]).min(costs[j + 1])
            };
            last = old;
        }
    }
    *costs.last().unwrap_or(&0)
}

fn fuzzy_score(skill: &AwesomeSkill, query: &str) -> Option<usize> {
    let q = normalize_search(query);
    let q = q.trim();
    if q.is_empty() {
        return Some(0);
    }

    let haystack = normalize_search(&format!("{} {} {}", skill.name, skill.description, skill.url));
    if haystack.contains(q) {
        return Some(0);
    }

    let tokens: Vec<&str> = haystack.split_whitespace().collect();
    let query_tokens: Vec<&str> = q.split_whitespace().collect();
    let mut total = 0;

    for query_token in query_tokens {
        let best = tokens
            .iter()
            .map(|token| {
                if token.starts_with(query_token) || query_token.starts_with(token) {
                    1
                } else {
                    levenshtein(query_token, token)
                }
            })
            .min()
            .unwrap_or(usize::MAX);

        let allowed = (query_token.len() / 3).max(1);
        if best > allowed {
            return None;
        }
        total += best;
    }

    Some(total + 2)
}

async fn fetch_awesome_skills(query: Option<&str>) -> Result<Vec<AwesomeSkill>, String> {
    let client = reqwest::Client::new();
    let mut endpoints = vec![
        "https://awesome-skills.com/api/skills".to_string(),
        "https://awesome-skills.com/skills.json".to_string(),
        "https://awesome-skills.com/api/search".to_string(),
    ];
    if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
        endpoints.insert(0, format!("https://awesome-skills.com/api/skills?q={}", q.replace(' ', "%20")));
        endpoints.insert(1, format!("https://awesome-skills.com/api/search?q={}", q.replace(' ', "%20")));
    }

    for endpoint in endpoints {
        if let Ok(res) = client.get(&endpoint).send().await {
            if res.status().is_success() {
                if let Ok(text) = res.text().await {
                    if let Some(skills) = parse_api_skills(&text) {
                        return Ok(skills);
                    }
                }
            }
        }
    }

    let html = client
        .get("https://awesome-skills.com/")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch awesome-skills.com: {}", e))?
        .error_for_status()
        .map_err(|e| format!("awesome-skills.com returned an error: {}", e))?
        .text()
        .await
        .map_err(|e| format!("Failed to read awesome-skills.com: {}", e))?;

    Ok(parse_awesome_skills(&html))
}

#[tauri::command]
pub async fn search_awesome_skills(query: Option<String>) -> Result<Vec<AwesomeSkill>, String> {
    let query = query.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let mut skills = fetch_awesome_skills(query.as_deref()).await?;
    let mut seen = HashSet::new();
    skills.retain(|skill| seen.insert(skill.url.clone()));

    if let Some(q) = query {
        let mut scored: Vec<(usize, AwesomeSkill)> = skills
            .into_iter()
            .filter_map(|skill| fuzzy_score(&skill, &q).map(|score| (score, skill)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        skills = scored.into_iter().map(|(_, skill)| skill).take(50).collect();
    }
    Ok(skills)
}

async fn find_skill_files(dir: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&current)
            .await
            .map_err(|e| format!("Failed to read {}: {}", current.display(), e))?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let file_type = entry.file_type().await.map_err(|e| e.to_string())?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() && name.eq_ignore_ascii_case("skill.md") {
                found.push(entry.path());
            }
        }
    }
    Ok(())
}

async fn write_skill_wrapper(plugin_dir: &Path) -> Result<PathBuf, String> {
    let root = plugin_dir.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let wrapper = plugin_dir.join("helix-skill-wrapper.mjs");
    let source = format!(r##"#!/usr/bin/env node
import fs from "node:fs/promises";
import path from "node:path";
import {{ Server }} from "@modelcontextprotocol/sdk/server/index.js";
import {{ StdioServerTransport }} from "@modelcontextprotocol/sdk/server/stdio.js";
import {{ CallToolRequestSchema, ListToolsRequestSchema }} from "@modelcontextprotocol/sdk/types.js";

const root = "{root}";

function slug(value) {{
  return String(value || "skill").toLowerCase().replace(/[^a-z0-9]+/g, "_").replace(/^_+|_+$/g, "").slice(0, 48) || "skill";
}}

function parseSkill(markdown, filePath) {{
  const frontmatter = markdown.match(/^---\s*\n([\s\S]*?)\n---\s*\n?/);
  const meta = {{}};
  if (frontmatter) {{
    for (const line of frontmatter[1].split("\n")) {{
      const match = line.match(/^([A-Za-z0-9_-]+):\s*(.*)$/);
      if (match) meta[match[1].toLowerCase()] = match[2].replace(/^["']|["']$/g, "").trim();
    }}
  }}
  const body = markdown.replace(/^---\s*\n[\s\S]*?\n---\s*\n?/, "").trim();
  const heading = body.match(/^#\s+(.+)$/m);
  const name = meta.name || (heading ? heading[1].trim() : path.basename(path.dirname(filePath)));
  const description = meta.description || body.split("\n").find((line) => line.trim() && !line.startsWith("#")) || "HELIX-compatible converted agent skill.";
  return {{ name, description: description.slice(0, 480), markdown, filePath }};
}}

async function walk(current, found = []) {{
  const entries = await fs.readdir(current, {{ withFileTypes: true }});
  for (const entry of entries) {{
    if (entry.name === "node_modules" || entry.name === ".git") continue;
    const fullPath = path.join(current, entry.name);
    if (entry.isDirectory()) await walk(fullPath, found);
    else if (entry.isFile() && entry.name.toLowerCase() === "skill.md") found.push(fullPath);
  }}
  return found;
}}

const skills = [];
for (const filePath of await walk(root)) {{
  const markdown = await fs.readFile(filePath, "utf8");
  const parsed = parseSkill(markdown, filePath);
  skills.push({{ ...parsed, toolName: "skill_" + slug(parsed.name) }});
}}

const server = new Server({{ name: "helix-converted-skills", version: "1.0.0" }}, {{ capabilities: {{ tools: {{}} }} }});

server.setRequestHandler(ListToolsRequestSchema, async () => ({{
  tools: skills.map((skill) => ({{
    name: skill.toolName,
    description: skill.description,
    inputSchema: {{
      type: "object",
      properties: {{
        prompt: {{ type: "string", description: "The user's current task or question." }},
        context: {{ type: "string", description: "Optional local context to apply the skill to." }}
      }}
    }}
  }}))
}}));

server.setRequestHandler(CallToolRequestSchema, async (request) => {{
  const skill = skills.find((item) => item.toolName === request.params.name);
  if (!skill) throw new Error("Unknown converted skill: " + request.params.name);
  const args = request.params.arguments || {{}};
  const text = [
    "# Converted HELIX Skill",
    "Name: " + skill.name,
    "Source: " + path.relative(root, skill.filePath),
    "",
    "Use the following skill instructions to complete the user's task inside HELIX.",
    "",
    skill.markdown,
    args.prompt ? "\n## User task\n" + args.prompt : "",
    args.context ? "\n## Context\n" + args.context : ""
  ].filter(Boolean).join("\n");
  return {{ content: [{{ type: "text", text }}] }};
}});

await server.connect(new StdioServerTransport());
"##);
    tokio::fs::write(&wrapper, source)
        .await
        .map_err(|e| format!("Failed to write wrapper: {}", e))?;
    Ok(wrapper)
}

async fn ensure_skill_wrapper_dependencies(plugin_dir: &Path) -> Result<(), String> {
    let package_json = plugin_dir.join("package.json");
    if tokio::fs::metadata(&package_json).await.is_err() {
        let name = plugin_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("helix-converted-skill");
        let manifest = serde_json::json!({
            "name": name,
            "private": true,
            "type": "module",
            "dependencies": {
                "@modelcontextprotocol/sdk": "^1.29.0"
            }
        });
        tokio::fs::write(
            &package_json,
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| format!("Failed to write converted skill package.json: {}", e))?;
    }

    if tokio::fs::metadata(plugin_dir.join("node_modules").join("@modelcontextprotocol").join("sdk")).await.is_ok() {
        return Ok(());
    }

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let install = Command::new(npm)
        .args(["install", "@modelcontextprotocol/sdk"])
        .current_dir(plugin_dir)
        .no_window()
        .output()
        .await
        .map_err(|e| format!("Failed to install converted skill dependencies: {}", e))?;
    if !install.status.success() {
        return Err(format!(
            "npm install @modelcontextprotocol/sdk failed: {}{}",
            String::from_utf8_lossy(&install.stderr),
            String::from_utf8_lossy(&install.stdout)
        ));
    }

    Ok(())
}

async fn verify_mcp_plugin(command: &str, args: &[String], cwd: &Path) -> Result<Vec<String>, String> {
    let mut child = Command::new(command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .no_window()
        .spawn()
        .map_err(|e| format!("Failed to start plugin verifier: {}", e))?;

    let mut stdin = child.stdin.take().ok_or_else(|| "Plugin verifier had no stdin.".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "Plugin verifier had no stdout.".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "Plugin verifier had no stderr.".to_string())?;

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut text = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&line);
        }
        text
    });

    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "helix-plugin-verifier", "version": "1.0.0" }
        }
    });
    let list = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    stdin.write_all(format!("{init}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    stdin.write_all(format!("{list}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stdout).lines();
    let mut tools = Vec::new();
    let read_result = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while let Ok(Some(line)) = reader.next_line().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(|v| v.as_i64()) == Some(2) {
                if let Some(items) = value.pointer("/result/tools").and_then(|v| v.as_array()) {
                    for item in items {
                        if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                            tools.push(name.to_string());
                        }
                    }
                }
                break;
            }
            if let Some(error) = value.get("error") {
                return Err(format!("Plugin verification JSON-RPC error: {error}"));
            }
        }
        Ok::<(), String>(())
    }).await;

    let _ = child.kill().await;
    let stderr_text = stderr_task.await.unwrap_or_default();

    match read_result {
        Ok(Ok(())) if !tools.is_empty() => Ok(tools),
        Ok(Ok(())) => Err(if stderr_text.is_empty() {
            "Plugin started but returned no tools.".into()
        } else {
            format!("Plugin returned no tools. stderr: {stderr_text}")
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(if stderr_text.is_empty() {
            "Plugin verification timed out.".into()
        } else {
            format!("Plugin verification timed out. stderr: {stderr_text}")
        }),
    }
}

async fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .no_window()
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn download_github_zipball(source: &GitHubSource, target_dir: &Path) -> Result<(), String> {
    let reference = source.branch.as_deref().unwrap_or("HEAD");
    let url = format!(
        "https://api.github.com/repos/{}/{}/zipball/{}",
        source.owner, source.repo, reference
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("HELIX-Desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("Failed to prepare GitHub downloader: {e}"))?;
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Failed to download GitHub zipball: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub zipball download failed for {}/{}: HTTP {}",
            source.owner,
            source.repo,
            response.status()
        ));
    }
    let zip_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read GitHub zipball: {e}"))?
        .to_vec();

    let target = target_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let reader = std::io::Cursor::new(zip_bytes);
        let mut archive = zip::ZipArchive::new(reader)
            .map_err(|e| format!("Downloaded GitHub zipball was invalid: {e}"))?;

        std::fs::create_dir_all(&target)
            .map_err(|e| format!("Failed to create plugin folder: {e}"))?;

        for index in 0..archive.len() {
            let mut file = archive
                .by_index(index)
                .map_err(|e| format!("Failed to read zip entry: {e}"))?;
            let Some(enclosed) = file.enclosed_name() else {
                continue;
            };
            let stripped: PathBuf = enclosed.components().skip(1).collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            let out_path = target.join(stripped);
            if file.is_dir() {
                std::fs::create_dir_all(&out_path)
                    .map_err(|e| format!("Failed to create plugin directory: {e}"))?;
            } else {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create plugin directory: {e}"))?;
                }
                let mut out = std::fs::File::create(&out_path)
                    .map_err(|e| format!("Failed to write plugin file: {e}"))?;
                std::io::copy(&mut file, &mut out)
                    .map_err(|e| format!("Failed to extract plugin file: {e}"))?;
            }
        }

        Ok(())
    })
    .await
    .map_err(|e| format!("Plugin extraction task failed: {e}"))?
}

#[tauri::command]
pub async fn install_awesome_skill(app: AppHandle, url: String, name: String) -> Result<DiscoveredPlugin, String> {
    let source = normalize_github_source(&url)?;

    let plugins_dir = app_plugins_dir(&app);
    tokio::fs::create_dir_all(&plugins_dir)
        .await
        .map_err(|e| format!("Failed to create plugin directory: {}", e))?;
    let safe_name = slug(&name);
    let target_dir = plugins_dir.join(&safe_name);

    if tokio::fs::metadata(&target_dir).await.is_err() {
        let mut clone = Command::new("git");
        clone.args(["clone", "--depth", "1"]).no_window();
        if let Some(branch) = &source.branch {
            clone.args(["--branch", branch]);
        }
        let clone_result = clone
            .arg(&source.clone_url)
            .arg(&target_dir)
            .output()
            .await;
        match clone_result {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let _ = tokio::fs::remove_dir_all(&target_dir).await;
                download_github_zipball(&source, &target_dir).await.map_err(|zip_error| {
                    format!(
                        "git clone failed for {}: {}{}\nGitHub zip fallback also failed: {}",
                        source.clone_url,
                        String::from_utf8_lossy(&output.stderr),
                        String::from_utf8_lossy(&output.stdout),
                        zip_error
                    )
                })?;
            }
            Err(_) => {
                let _ = tokio::fs::remove_dir_all(&target_dir).await;
                download_github_zipball(&source, &target_dir).await?;
            }
        }
    }

    let plugin_dir = if let Some(subdir) = &source.subdir {
        let path = target_dir.join(subdir);
        if tokio::fs::metadata(&path).await.is_err() {
            let _ = tokio::fs::remove_dir_all(&target_dir).await;
            download_github_zipball(&source, &target_dir).await?;
        }
        if tokio::fs::metadata(&path).await.is_err() {
            return Err(format!(
                "Downloaded repository, but the linked plugin folder was not found: {}",
                subdir.display()
            ));
        }
        path
    } else {
        target_dir.clone()
    };

    let package_json = plugin_dir.join("package.json");
    if tokio::fs::metadata(&package_json).await.is_ok() {
        let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
        if !command_available(npm).await {
            return Err("Node.js/npm is required to install this MCP plugin. Install the latest Node.js LTS, then try again.".into());
        }
        let install = Command::new(npm)
            .arg("install")
            .current_dir(&plugin_dir)
            .no_window()
            .output()
            .await
            .map_err(|e| format!("Failed to run npm install: {}", e))?;
        if !install.status.success() {
            return Err(format!("npm install failed: {}", String::from_utf8_lossy(&install.stderr)));
        }
        if let Ok(pkg_text) = tokio::fs::read_to_string(&package_json).await {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&pkg_text) {
                if pkg.pointer("/scripts/build").is_some() {
                    let build = Command::new(npm)
                        .args(["run", "build"])
                        .current_dir(&plugin_dir)
                        .no_window()
                        .output()
                        .await
                        .map_err(|e| format!("Failed to run npm run build: {}", e))?;
                    if !build.status.success() {
                        return Err(format!("npm run build failed: {}", String::from_utf8_lossy(&build.stderr)));
                    }
                }
            }
        }
    }

    let mut skill_files = Vec::new();
    find_skill_files(&plugin_dir, &mut skill_files).await?;

    let (command, args) = if !skill_files.is_empty() {
        if !command_available(if cfg!(windows) { "node.exe" } else { "node" }).await {
            return Err("Node.js is required to run converted SKILL.md plugins. Install the latest Node.js LTS, then try again.".into());
        }
        if !command_available(if cfg!(windows) { "npm.cmd" } else { "npm" }).await {
            return Err("npm is required to install converted SKILL.md plugin dependencies. Install the latest Node.js LTS, then try again.".into());
        }
        ensure_skill_wrapper_dependencies(&plugin_dir).await?;
        let wrapper = write_skill_wrapper(&plugin_dir).await?;
        ("node".to_string(), vec![wrapper.to_string_lossy().to_string()])
    } else if tokio::fs::metadata(&package_json).await.is_ok() {
        let pkg_text = tokio::fs::read_to_string(&package_json).await.map_err(|e| e.to_string())?;
        let pkg: serde_json::Value = serde_json::from_str(&pkg_text).map_err(|e| e.to_string())?;
        if let Some(bin) = pkg.get("bin") {
            let bin_path = if let Some(s) = bin.as_str() {
                s.to_string()
            } else {
                bin.as_object()
                    .and_then(|o| o.values().next())
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            if bin_path.is_empty() {
                return Err("package.json has no runnable bin entry.".into());
            }
            ("node".to_string(), vec![plugin_dir.join(bin_path).to_string_lossy().to_string()])
        } else if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
            ("node".to_string(), vec![plugin_dir.join(main).to_string_lossy().to_string()])
        } else {
            (if cfg!(windows) { "npx.cmd" } else { "npx" }.to_string(), vec!["-y".into(), ".".into()])
        }
    } else {
        return Err("This repository does not expose package.json or any SKILL.md files to convert.".into());
    };

    let verified = match verify_mcp_plugin(&command, &args, &plugin_dir).await {
        Ok(_) => (Some(true), Some(chrono::Utc::now().to_rfc3339()), None),
        Err(e) => (Some(false), Some(chrono::Utc::now().to_rfc3339()), Some(e)),
    };

    Ok(DiscoveredPlugin {
        id: format!("awesome_{}", safe_name),
        name,
        command: Some(command),
        args: Some(args),
        env: None,
        url: None,
        cwd: Some(plugin_dir.to_string_lossy().to_string()),
        enabled: Some(true),
        verified: verified.0,
        verified_at: verified.1,
        verify_error: verified.2,
    })
}

/// Spawns an MCP server process as a sidecar/plugin and hooks up stdio to Tauri events.
#[tauri::command]
pub async fn spawn_mcp_server(
    app: AppHandle,
    state: State<'_, McpState>,
    plugin_id: String,
    command: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    cwd: Option<String>,
) -> Result<(), String> {
    let mut cmd = Command::new(&command);
    cmd.args(args)
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::piped())
       .no_window();

    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }

    if let Some(envs) = env {
        cmd.envs(envs);
    }

    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn {}: {}", command, e))?;

    let stdout = child.stdout.take().ok_or_else(|| format!("Failed to capture stdout pipe for {}", command))?;
    let stderr = child.stderr.take().ok_or_else(|| format!("Failed to capture stderr pipe for {}", command))?;

    let mut processes = state.processes.lock().await;
    processes.insert(plugin_id.clone(), Arc::new(Mutex::new(child)));

    let plugin_id_out = plugin_id.clone();
    let plugin_id_err = plugin_id.clone();

    // Spawn a task to read stdout and emit to frontend
    let app_handle_out = app.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = app_handle_out.emit(
                "mcp-stdout",
                McpMessagePayload {
                    plugin_id: plugin_id_out.clone(),
                    message: line,
                },
            );
        }
    });

    // Spawn a task to read stderr and emit to frontend
    let app_handle_err = app.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let _ = app_handle_err.emit(
                "mcp-stderr",
                McpErrorPayload {
                    plugin_id: plugin_id_err.clone(),
                    error: line,
                },
            );
        }
    });

    Ok(())
}

/// Writes a JSON-RPC message to the stdin of the specified MCP server process.
#[tauri::command]
pub async fn send_mcp_message(
    state: State<'_, McpState>,
    plugin_id: String,
    message: String,
) -> Result<(), String> {
    let processes = state.processes.lock().await;

    if let Some(child_arc) = processes.get(&plugin_id) {
        let mut child = child_arc.lock().await;
        if let Some(stdin) = child.stdin.as_mut() {
            let payload = format!("{}\n", message);
            stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| format!("Failed to write to stdin: {}", e))?;
            stdin.flush().await.map_err(|e| format!("Failed to flush stdin: {}", e))?;
            Ok(())
        } else {
            Err(format!("Process {} has no stdin", plugin_id))
        }
    } else {
        Err(format!("Process {} not found", plugin_id))
    }
}

/// Kills an MCP server process.
#[tauri::command]
pub async fn kill_mcp_server(
    state: State<'_, McpState>,
    plugin_id: String,
) -> Result<(), String> {
    let mut processes = state.processes.lock().await;

    if let Some(child_arc) = processes.remove(&plugin_id) {
        let mut child = child_arc.lock().await;
        child.kill().await.map_err(|e| format!("Failed to kill process: {}", e))?;
        Ok(())
    } else {
        Err(format!("Process {} not found", plugin_id))
    }
}
