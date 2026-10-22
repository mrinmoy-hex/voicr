//! Local config web UI, started alongside `voicr daemon`.
//!
//! Binds to 127.0.0.1 only — this is a developer convenience for editing config,
//! not something meant to be exposed on the network. Still, other processes on
//! the machine (and any page you open in a browser) share that interface, so
//! every request to `/` and `/api/*` must carry the access token: either the
//! `?token=` query parameter or an `Authorization: Bearer <token>` header.
//! The full URL is printed at daemon startup and the token persists in
//! `ui_token` inside the data directory so the URL survives daemon restarts.

use crate::config::Config;
use crate::managers::model::{DownloadProgress, ModelManager};
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};
use tokio::runtime::Handle;

const UI_HTML: &str = include_str!("web_ui.html");
const ICON_PNG: &[u8] = include_bytes!("../icons/voicr.iconset/icon_128x128@2x.png");
const GROQ_SVG: &str = include_str!("../icons/groq.svg");
const OPENAI_SVG: &str = include_str!("../icons/openai.svg");
const SARVAM_SVG: &str = include_str!("../icons/sarvam.svg");

/// Placeholder in `web_ui.html` replaced with the real token when served.
const TOKEN_PLACEHOLDER: &str = "__VOICR_TOKEN__";

/// Placeholder in the UI HTML replaced with the crate version at serve time.
const VERSION_PLACEHOLDER: &str = "__VOICR_VERSION__";

/// Latest progress for each model currently downloading, keyed by model id.
/// Entries are removed once the download finishes (success or failure).
pub type DownloadProgressMap = Arc<Mutex<HashMap<String, DownloadProgress>>>;

pub fn spawn(
    config: Arc<Mutex<Config>>,
    model_manager: Arc<ModelManager>,
    download_progress: DownloadProgressMap,
    runtime: Handle,
    port: u16,
) {
    std::thread::spawn(move || {
        let Some(token) = load_or_create_token() else {
            warn!("Web UI disabled: no access token (could not read/write the data directory)");
            return;
        };
        let addr = format!("127.0.0.1:{}", port);
        let server = match Server::http(&addr) {
            Ok(s) => s,
            Err(e) => {
                warn!("Web UI failed to bind {}: {}", addr, e);
                return;
            }
        };
        info!("Web UI: http://{}/?token={}", addr, token);

        for request in server.incoming_requests() {
            if let Err(e) = handle(
                request,
                &config,
                &model_manager,
                &download_progress,
                &runtime,
                &token,
            ) {
                error!("Web UI request error: {}", e);
            }
        }
    });
}

fn handle(
    mut request: Request,
    config: &Arc<Mutex<Config>>,
    model_manager: &Arc<ModelManager>,
    download_progress: &DownloadProgressMap,
    runtime: &Handle,
    token: &str,
) -> std::io::Result<()> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query_token) = split_query(&url);

    // Static assets carry no secrets. Everything else (the UI page and all API
    // routes) needs the token — the query parameter or a Bearer header both count.
    let is_api = path.starts_with("/api/");
    if is_api || path == "/" {
        if !authorized(&request, token, query_token) {
            return if is_api {
                respond_json(
                    request,
                    401,
                    &serde_json::json!({"error": "Missing or invalid web UI token. Open the ?token= URL printed at daemon startup, or send Authorization: Bearer <token>."}),
                )
            } else {
                respond_token_required(request)
            };
        }
    }

    match (&method, path) {
        (Method::Get, "/") => {
            // The page gets the token baked in so its own fetches are authorized.
            let html = UI_HTML
                .replace(TOKEN_PLACEHOLDER, token)
                .replace(VERSION_PLACEHOLDER, env!("CARGO_PKG_VERSION"));
            respond_html(request, &html)
        }
        (Method::Get, "/icon.png") => respond_png(request, ICON_PNG),
        (Method::Get, "/icons/groq.svg") => respond_svg(request, GROQ_SVG),
        (Method::Get, "/icons/openai.svg") => respond_svg(request, OPENAI_SVG),
        (Method::Get, "/icons/sarvam.svg") => respond_svg(request, SARVAM_SVG),
        (Method::Get, "/api/config") => {
            let cfg = config.lock().unwrap().clone();
            respond_json(request, 200, &cfg)
        }
        (Method::Put, "/api/config") => {
            let body = read_body(&mut request)?;
            match serde_json::from_str::<Config>(&body) {
                Ok(new_cfg) => {
                    if let Err(e) = crate::config::save_config(&new_cfg) {
                        return respond_json(
                            request,
                            500,
                            &serde_json::json!({"error": e.to_string()}),
                        );
                    }
                    *config.lock().unwrap() = new_cfg.clone();
                    respond_json(request, 200, &new_cfg)
                }
                Err(e) => respond_json(
                    request,
                    400,
                    &serde_json::json!({"error": format!("Invalid config: {}", e)}),
                ),
            }
        }
        (Method::Get, "/api/models") => {
            let selected = config.lock().unwrap().model.selected.clone();
            let models = model_manager.get_available_models();
            let progress = download_progress.lock().unwrap();
            let out: Vec<_> = models
                .into_iter()
                .map(|m| {
                    let pct = progress.get(&m.id).map(|p| p.percentage);
                    serde_json::json!({
                        "id": m.id,
                        "name": m.name,
                        "size_mb": m.size_mb,
                        "is_downloaded": m.is_downloaded,
                        "is_downloading": m.is_downloading,
                        "download_percentage": pct,
                        "is_recommended": m.is_recommended,
                        "selected": m.id == selected,
                    })
                })
                .collect();
            respond_json(request, 200, &out)
        }
        (Method::Post, "/api/models/download") => {
            let body = read_body(&mut request)?;
            let id = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string));
            let Some(id) = id else {
                return respond_json(
                    request,
                    400,
                    &serde_json::json!({"error": "Missing \"id\" field"}),
                );
            };
            if model_manager.get_model_info(&id).is_none() {
                return respond_json(
                    request,
                    404,
                    &serde_json::json!({"error": format!("Unknown model: {}", id)}),
                );
            }

            // Kick off the download on the tokio runtime and return immediately;
            // the web UI polls GET /api/models for progress.
            let mm = model_manager.clone();
            let cfg = config.clone();
            let progress_map = download_progress.clone();
            let download_id = id.clone();
            runtime.spawn(async move {
                match mm.download_model(&download_id).await {
                    Ok(()) => {
                        if let Err(e) = mm.set_active_model(&download_id) {
                            error!("Failed to auto-select {}: {}", download_id, e);
                        } else {
                            cfg.lock().unwrap().model.selected = download_id.clone();
                        }
                    }
                    Err(e) => error!("Download of {} failed: {}", download_id, e),
                }
                progress_map.lock().unwrap().remove(&download_id);
            });

            respond_json(request, 202, &serde_json::json!({"ok": true}))
        }
        (Method::Post, "/api/models/select") => {
            let body = read_body(&mut request)?;
            let id = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string));
            match id {
                Some(id) => match model_manager.set_active_model(&id) {
                    Ok(()) => {
                        config.lock().unwrap().model.selected = id;
                        respond_json(request, 200, &serde_json::json!({"ok": true}))
                    }
                    Err(e) => {
                        respond_json(request, 400, &serde_json::json!({"error": e.to_string()}))
                    }
                },
                None => respond_json(
                    request,
                    400,
                    &serde_json::json!({"error": "Missing \"id\" field"}),
                ),
            }
        }
        (Method::Get, "/api/cloud_models") => {
            let cfg = config.lock().unwrap().clone();

            if cfg.cloud.provider == crate::config::CloudProvider::SarvamAI {
                let models = vec!["saaras:v3".to_string(), "saaras:v2".to_string()];
                return respond_json(request, 200, &models);
            }

            let (api_key, base_url) = match cfg.cloud.provider {
                crate::config::CloudProvider::Groq => (
                    cfg.cloud.groq_api_key.clone(),
                    "https://api.groq.com/openai/v1",
                ),
                crate::config::CloudProvider::OpenAI => (
                    cfg.cloud.openai_api_key.clone(),
                    "https://api.openai.com/v1",
                ),
                crate::config::CloudProvider::Custom => {
                    let key = cfg.cloud.custom_api_key.clone();
                    let url = cfg.cloud.custom_base_url.clone().unwrap_or_default();
                    (key, Box::leak(url.into_boxed_str()) as &str)
                }
                crate::config::CloudProvider::SarvamAI => unreachable!(),
            };

            if let Some(key) = api_key {
                let client = reqwest::blocking::Client::new();
                let url = format!("{}/models", base_url.trim_end_matches('/'));
                match client.get(&url).bearer_auth(key).send() {
                    Ok(resp) => {
                        if let Ok(json) = resp.json::<serde_json::Value>() {
                            let mut models = vec![];
                            if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
                                for m in data {
                                    if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                                        models.push(id.to_string());
                                    }
                                }
                            }
                            if cfg.cloud.provider == crate::config::CloudProvider::Groq {
                                models.retain(|m| m.contains("whisper"));
                            } else if cfg.cloud.provider == crate::config::CloudProvider::OpenAI {
                                models.retain(|m| m.contains("whisper"));
                            }
                            respond_json(request, 200, &models)
                        } else {
                            respond_json(
                                request,
                                500,
                                &serde_json::json!({"error": "Invalid response from provider"}),
                            )
                        }
                    }
                    Err(e) => {
                        respond_json(request, 500, &serde_json::json!({"error": e.to_string()}))
                    }
                }
            } else {
                respond_json(
                    request,
                    400,
                    &serde_json::json!({"error": "API key not set for this provider"}),
                )
            }
        }
        (Method::Post, "/api/restart") => {
            info!("Restart requested from web UI");
            respond_json(request, 200, &serde_json::json!({"ok": true}))?;
            restart_process();
        }
        (Method::Get, "/api/audio_devices") => {
            let devices = crate::audio_toolkit::list_input_devices().unwrap_or_default();
            let out: Vec<_> = devices
                .into_iter()
                .map(|d| {
                    serde_json::json!({
                        "name": d.name,
                        "is_default": d.is_default
                    })
                })
                .collect();
            respond_json(request, 200, &out)
        }
        (Method::Get, "/api/autostart") => {
            let enabled = crate::autostart::is_enabled();
            respond_json(request, 200, &serde_json::json!({ "enabled": enabled }))
        }
        (Method::Post, "/api/autostart") => {
            let body = read_body(&mut request)?;
            let enabled = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
                .unwrap_or(false);

            let res = if enabled {
                crate::autostart::enable()
            } else {
                crate::autostart::disable()
            };

            match res {
                Ok(()) => respond_json(
                    request,
                    200,
                    &serde_json::json!({ "enabled": crate::autostart::is_enabled() }),
                ),
                Err(e) => {
                    respond_json(request, 500, &serde_json::json!({ "error": e.to_string() }))
                }
            }
        }
        _ => request.respond(Response::from_string("Not found").with_status_code(404)),
    }
}

/// Re-run the same binary with the same args in place of this process.
/// Same PID on Unix (via execve), so the PID file stays valid and there's
/// no race with a second process trying to bind the same socket/port.
fn restart_process() -> ! {
    let exe = std::env::current_exe().expect("current_exe");
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        error!("Restart failed: {}", err);
        std::process::exit(1);
    }

    // ponytail: Windows has no exec(); this leaves a brief window where two
    // processes could race on the PID file. Fine for a manual restart button.
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new(&exe).args(&args).spawn();
        std::process::exit(0);
    }
}

// ── Access token ──────────────────────────────────────────────────────────────
//
// The token is generated once and stored in `ui_token` inside the data
// directory with 0600 permissions. Persisting it means the `?token=` URL the
// daemon printed keeps working across `/api/restart` (which re-execs the same
// binary) and across daemon restarts.

/// Loads the access token, creating one if the file is missing or unusable.
/// Returns `None` when the data directory can't be used — the caller fails
/// closed and disables the web UI rather than serving it unauthenticated.
fn load_or_create_token() -> Option<String> {
    let path = match crate::paths::data_dir() {
        Ok(dir) => dir.join("ui_token"),
        Err(e) => {
            warn!(
                "Web UI token unavailable: cannot locate data directory: {}",
                e
            );
            return None;
        }
    };

    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if existing.len() >= 16 {
            return Some(existing.to_string());
        }
        // Too short to be one of ours (empty/corrupt) — fall through and regenerate.
    }

    let token = generate_token();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(
                "Web UI token unavailable: cannot create {}: {}",
                parent.display(),
                e
            );
            return None;
        }
    }
    if write_private_file(&path, &token).is_none() {
        warn!("Web UI token unavailable: cannot write {}", path.display());
        return None;
    }
    Some(token)
}

/// Writes the token file with owner-only permissions on Unix.
fn write_private_file(path: &std::path::Path, contents: &str) -> Option<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    #[allow(unused_mut)]
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);

    let mut file = opts.open(path).ok()?;
    file.write_all(contents.as_bytes()).ok()?;
    Some(())
}

/// 32 random bytes mapped onto an unambiguous alphanumeric charset.
/// Prefers the OS CSPRNG; falls back to per-process randomly-seeded hashing
/// (adequate for a localhost-only token, and Windows has no /dev/urandom).
fn generate_token() -> String {
    const CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 32];

    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut urandom) = std::fs::File::open("/dev/urandom") {
            if urandom.read_exact(&mut bytes).is_ok() {
                return charset_encode(&bytes, CHARSET);
            }
        }
    }

    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(seed);
        hasher.write_u64(std::process::id() as u64);
        hasher.write_usize(i);
        seed = hasher.finish();
        for (j, byte) in chunk.iter_mut().enumerate() {
            *byte = (seed >> (8 * j)) as u8;
        }
    }
    charset_encode(&bytes, CHARSET)
}

fn charset_encode(bytes: &[u8], charset: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| charset[(*b as usize) % charset.len()] as char)
        .collect()
}

fn split_query<'a>(url: &'a str) -> (&'a str, Option<&'a str>) {
    match url.split_once('?') {
        Some((path, query)) => (path, query_param(query, "token")),
        None => (url, None),
    }
}

fn query_param<'a>(query: &'a str, name: &'a str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            Some(value)
        } else {
            None
        }
    })
}

/// Check both accepted token carriers: `?token=` on the URL (used by the
/// login-less page URL) and `Authorization: Bearer <token>` (used by the
/// page's fetches and by curl).
fn authorized(request: &Request, token: &str, query_token: Option<&str>) -> bool {
    if let Some(query_token) = query_token {
        if query_token == token {
            return true;
        }
    }
    match bearer_header(request) {
        Some(provided) => provided == token,
        None => false,
    }
}

fn bearer_header(request: &Request) -> Option<&str> {
    for header in request.headers() {
        let name = header.field.as_str().to_ascii_lowercase();
        if name == "authorization" {
            if let Some(bearer) = header.value.as_str().strip_prefix("Bearer ") {
                return Some(bearer.trim());
            }
        }
    }
    None
}

/// Plain-text 401 for the browser landing page, pointing at the token URL.
fn respond_token_required(request: Request) -> std::io::Result<()> {
    let body = "voicr web UI requires its access token.\n\n\
        Open the URL printed by the daemon at startup, e.g.\n  \
        http://127.0.0.1:7898/?token=…\n\n\
        The token is stored in 'ui_token' in the voicr data directory.\n";
    let header_ct = Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
        .expect("valid header");
    request.respond(
        Response::from_string(body)
            .with_header(header_ct)
            .with_status_code(401),
    )
}

fn read_body(request: &mut Request) -> std::io::Result<String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    Ok(body)
}

fn respond_html(request: Request, html: &str) -> std::io::Result<()> {
    let header_ct = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
        .expect("valid header");
    let header_cc = Header::from_bytes(
        &b"Cache-Control"[..],
        &b"no-cache, no-store, must-revalidate"[..],
    )
    .expect("valid header");
    request.respond(
        Response::from_string(html)
            .with_header(header_ct)
            .with_header(header_cc),
    )
}

fn respond_png(request: Request, bytes: &[u8]) -> std::io::Result<()> {
    let header_ct =
        Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).expect("valid header");
    let header_cc = Header::from_bytes(
        &b"Cache-Control"[..],
        &b"no-cache, no-store, must-revalidate"[..],
    )
    .expect("valid header");
    request.respond(
        Response::from_data(bytes)
            .with_header(header_ct)
            .with_header(header_cc),
    )
}

fn respond_svg(request: Request, svg: &str) -> std::io::Result<()> {
    let header_ct =
        Header::from_bytes(&b"Content-Type"[..], &b"image/svg+xml"[..]).expect("valid header");
    let header_cc = Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=86400"[..])
        .expect("valid header");
    request.respond(
        Response::from_string(svg)
            .with_header(header_ct)
            .with_header(header_cc),
    )
}

fn respond_json<T: serde::Serialize>(
    request: Request,
    status: u16,
    value: &T,
) -> std::io::Result<()> {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    let header_ct =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("valid header");
    let header_cc = Header::from_bytes(
        &b"Cache-Control"[..],
        &b"no-cache, no-store, must-revalidate"[..],
    )
    .expect("valid header");
    let response = Response::from_string(body)
        .with_header(header_ct)
        .with_header(header_cc)
        .with_status_code(status);
    request.respond(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ab3Cdef6gh2jklmn4pqrst";

    #[test]
    fn split_query_extracts_token() {
        let (path, token) = split_query("/api/config?token=ab3Cdef6gh2jklmn4pqrst&x=1");
        assert_eq!(path, "/api/config");
        assert_eq!(token, Some(TOKEN));

        let (path, token) = split_query("/");
        assert_eq!(path, "/");
        assert_eq!(token, None);
    }

    #[test]
    fn token_generation_is_long_and_alnum() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), second.len());
        assert_ne!(first, second, "consecutive tokens must differ");
        assert!(first.len() >= 32);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(
            !first.contains("__"),
            "token must be safe to inline into HTML/JS"
        );
    }

    #[test]
    fn ui_html_placeholders_are_replaced() {
        let html = UI_HTML
            .replace(TOKEN_PLACEHOLDER, TOKEN)
            .replace(VERSION_PLACEHOLDER, env!("CARGO_PKG_VERSION"));
        assert!(!html.contains(TOKEN_PLACEHOLDER));
        assert!(!html.contains(VERSION_PLACEHOLDER));
        assert!(html.contains(env!("CARGO_PKG_VERSION")));
    }
}
