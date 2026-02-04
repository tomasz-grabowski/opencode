mod cli;
#[cfg(windows)]
mod job_object;
mod markdown;
mod window_customizer;

use cli::{install_cli, sync_cli};
use futures::FutureExt;
use futures::future;
#[cfg(windows)]
use job_object::*;
use std::{
    collections::VecDeque,
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, LogicalSize, Manager, RunEvent, State, WebviewWindowBuilder};
#[cfg(windows)]
use tauri_plugin_decorum::WebviewWindowExt;
#[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_store::StoreExt;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};
use tokio::sync::{mpsc, oneshot};

use crate::window_customizer::PinchZoomDisablePlugin;

const SETTINGS_STORE: &str = "opencode.settings.dat";
const DEFAULT_SERVER_URL_KEY: &str = "defaultServerUrl";
const WEB_MIRROR_KEY: &str = "webMirror";

fn window_state_flags() -> StateFlags {
    StateFlags::all() - StateFlags::DECORATIONS
}

#[derive(Clone, serde::Serialize, specta::Type)]
struct ServerReadyData {
    url: String,
    password: Option<String>,
}

#[derive(Clone)]
struct ServerState {
    child: Arc<Mutex<Option<CommandChild>>>,
    status: future::Shared<oneshot::Receiver<Result<ServerReadyData, String>>>,
}

impl ServerState {
    pub fn new(
        child: Option<CommandChild>,
        status: oneshot::Receiver<Result<ServerReadyData, String>>,
    ) -> Self {
        Self {
            child: Arc::new(Mutex::new(child)),
            status: status.shared(),
        }
    }

    pub fn set_child(&self, child: Option<CommandChild>) {
        *self.child.lock().unwrap() = child;
    }
}

#[derive(Clone)]
struct LogState(Arc<Mutex<VecDeque<String>>>);

#[derive(Clone, Default, serde::Serialize, serde::Deserialize, specta::Type)]
struct WebMirrorConfig {
    enabled: bool,
    port: Option<u32>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Clone, serde::Serialize, specta::Type)]
struct WebMirrorStatus {
    running: bool,
    /// Local URL (http://localhost:<port>)
    local_url: Option<String>,
    /// Network URL (http://<lan-ip>:<port>) for remote access
    network_url: Option<String>,
    /// The resolved username used for authentication
    username: String,
    /// The resolved password used for authentication
    password: String,
    config: WebMirrorConfig,
}

struct WebMirrorState {
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    local_url: Arc<Mutex<Option<String>>>,
    network_url: Arc<Mutex<Option<String>>>,
}

const MAX_LOG_ENTRIES: usize = 200;

#[tauri::command]
#[specta::specta]
fn kill_sidecar(app: AppHandle) {
    let Some(server_state) = app.try_state::<ServerState>() else {
        println!("Server not running");
        return;
    };

    let Some(server_state) = server_state
        .child
        .lock()
        .expect("Failed to acquire mutex lock")
        .take()
    else {
        println!("Server state missing");
        return;
    };

    let _ = server_state.kill();

    println!("Killed server");
}

async fn get_logs(app: AppHandle) -> Result<String, String> {
    let log_state = app.try_state::<LogState>().ok_or("Log state not found")?;

    let logs = log_state
        .0
        .lock()
        .map_err(|_| "Failed to acquire log lock")?;

    Ok(logs.iter().cloned().collect::<Vec<_>>().join(""))
}

#[tauri::command]
#[specta::specta]
async fn ensure_server_ready(state: State<'_, ServerState>) -> Result<ServerReadyData, String> {
    state
        .status
        .clone()
        .await
        .map_err(|_| "Failed to get server status".to_string())?
}

#[tauri::command]
#[specta::specta]
fn get_default_server_url(app: AppHandle) -> Result<Option<String>, String> {
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("Failed to open settings store: {}", e))?;

    let value = store.get(DEFAULT_SERVER_URL_KEY);
    match value {
        Some(v) => Ok(v.as_str().map(String::from)),
        None => Ok(None),
    }
}

#[tauri::command]
#[specta::specta]
async fn set_default_server_url(app: AppHandle, url: Option<String>) -> Result<(), String> {
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("Failed to open settings store: {}", e))?;

    match url {
        Some(u) => {
            store.set(DEFAULT_SERVER_URL_KEY, serde_json::Value::String(u));
        }
        None => {
            store.delete(DEFAULT_SERVER_URL_KEY);
        }
    }

    store
        .save()
        .map_err(|e| format!("Failed to save settings: {}", e))?;

    Ok(())
}

fn get_web_mirror_config(app: &AppHandle) -> WebMirrorConfig {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        return WebMirrorConfig::default();
    };
    store
        .get(WEB_MIRROR_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Returns the first non-internal IPv4 address, skipping Docker bridges (172.x).
/// Mirrors the logic from `packages/opencode/src/cli/cmd/web.ts` `getNetworkIPs()`.
fn get_network_ip() -> Option<String> {
    let Ok(addrs) = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect("8.8.8.8:80")?;
        s.local_addr()
    }) else {
        return None;
    };
    let ip = addrs.ip().to_string();
    if ip.starts_with("172.") {
        return None;
    }
    Some(ip)
}

/// Try to kill any process listening on the given port.
/// Returns true if a process was found and killed.
fn try_kill_port_holder(port: u32) -> bool {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("lsof")
            .args(["-ti", &format!(":{port}")])
            .output();
        if let Ok(out) = output {
            let pids = String::from_utf8_lossy(&out.stdout);
            for pid_str in pids.split_whitespace() {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    if pid == std::process::id() {
                        continue;
                    }
                    println!("[web-mirror] Killing orphan process {pid} on port {port}");
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .output();
                    std::thread::sleep(Duration::from_millis(500));
                    return true;
                }
            }
        }
    }
    false
}

/// Starts a TCP reverse proxy that forwards all traffic from `0.0.0.0:<port>`
/// to the local desktop server. This gives a true 1:1 mirror — same sessions,
/// same live updates (SSE/WebSocket), same everything — because it just pipes
/// raw TCP bytes through without interpreting them.
fn spawn_web_mirror_proxy(
    target: &str,
    config: &WebMirrorConfig,
) -> Result<(tokio::task::JoinHandle<()>, String, Option<String>), String> {
    let port = config.port.unwrap_or(4096);
    let target_addr = target
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .to_string();

    // First attempt to bind
    let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{port}")) {
        Ok(l) => l,
        Err(_) => {
            // Port occupied — try to kill orphan process and retry once
            if try_kill_port_holder(port) {
                std::net::TcpListener::bind(format!("0.0.0.0:{port}"))
                    .map_err(|e| format!("Port {port} is still in use after cleanup: {e}"))?
            } else {
                return Err(format!(
                    "Port {port} is already in use by another application. Choose a different port in settings."
                ));
            }
        }
    };
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set non-blocking: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| format!("Failed to create tokio listener: {e}"))?;

    let network_ip = get_network_ip();
    let network_url = network_ip.as_ref().map(|ip| format!("http://{ip}:{port}"));

    println!("[web-mirror] TCP proxy listening on 0.0.0.0:{port} -> {target_addr}");
    if let Some(ref url) = network_url {
        println!("[web-mirror] Network access: {url}");
    }

    let handle = tokio::spawn(async move {
        loop {
            let Ok((inbound, peer)) = listener.accept().await else {
                continue;
            };
            let target = target_addr.clone();
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(&target).await else {
                    eprintln!("[web-mirror] failed to connect to {target} for {peer}");
                    return;
                };
                let (mut ri, mut wi) = inbound.into_split();
                let (mut ro, mut wo) = outbound.into_split();
                let a = tokio::io::copy(&mut ri, &mut wo);
                let b = tokio::io::copy(&mut ro, &mut wi);
                let _ = tokio::try_join!(a, b);
            });
        }
    });

    let local_url = format!("http://localhost:{port}");
    Ok((handle, local_url, network_url))
}

#[tauri::command]
#[specta::specta]
async fn start_web_mirror(app: AppHandle, config: WebMirrorConfig) -> Result<WebMirrorStatus, String> {
    // Save config to store
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("Failed to open settings store: {}", e))?;
    store.set(
        WEB_MIRROR_KEY,
        serde_json::to_value(&config).map_err(|e| format!("Failed to serialize config: {}", e))?,
    );
    store.save().map_err(|e| format!("Failed to save settings: {}", e))?;

    let state = app.state::<WebMirrorState>();

    // Abort existing proxy if running
    if let Some(handle) = state.handle.lock().unwrap().take() {
        handle.abort();
    }

    let (username, password) = resolve_credentials(&config, &app);

    if !config.enabled {
        *state.local_url.lock().unwrap() = None;
        *state.network_url.lock().unwrap() = None;
        return Ok(WebMirrorStatus {
            running: false,
            local_url: None,
            network_url: None,
            username,
            password,
            config,
        });
    }

    // Get the desktop server URL to proxy to
    let server_state = app.state::<ServerState>();
    let server_data = server_state
        .status
        .clone()
        .await
        .map_err(|_| "Server not ready yet".to_string())?
        .map_err(|e| format!("Server failed: {e}"))?;

    let (handle, local_url, network_url) = spawn_web_mirror_proxy(&server_data.url, &config)?;
    *state.handle.lock().unwrap() = Some(handle);
    *state.local_url.lock().unwrap() = Some(local_url.clone());
    *state.network_url.lock().unwrap() = network_url.clone();
    Ok(WebMirrorStatus {
        running: true,
        local_url: Some(local_url),
        network_url,
        username,
        password,
        config,
    })
}

#[tauri::command]
#[specta::specta]
async fn stop_web_mirror(app: AppHandle) -> Result<(), String> {
    let state = app.state::<WebMirrorState>();
    if let Some(handle) = state.handle.lock().unwrap().take() {
        handle.abort();
    }
    *state.local_url.lock().unwrap() = None;
    *state.network_url.lock().unwrap() = None;

    // Update config in store
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("Failed to open settings store: {}", e))?;
    let mut config = get_web_mirror_config(&app);
    config.enabled = false;
    store.set(
        WEB_MIRROR_KEY,
        serde_json::to_value(&config).map_err(|e| format!("Failed to serialize config: {}", e))?,
    );
    store.save().map_err(|e| format!("Failed to save settings: {}", e))?;
    Ok(())
}

/// Resolves the server credentials using this priority:
/// 1. Config values (user set in Settings UI)
/// 2. Shell env vars (from .zshrc etc.)
/// 3. Defaults (username="opencode", password=random UUID)
fn resolve_credentials(config: &WebMirrorConfig, app: &AppHandle) -> (String, String) {
    let username = config
        .username
        .clone()
        .filter(|v| !v.is_empty())
        .or_else(|| cli::probe_shell_env(app, "OPENCODE_SERVER_USERNAME"))
        .unwrap_or_else(|| "opencode".to_string());

    let password = config
        .password
        .clone()
        .filter(|v| !v.is_empty())
        .or_else(|| cli::probe_shell_env(app, "OPENCODE_SERVER_PASSWORD"))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    (username, password)
}

#[tauri::command]
#[specta::specta]
fn get_web_mirror_status(app: AppHandle) -> WebMirrorStatus {
    let state = app.state::<WebMirrorState>();
    let running = state
        .handle
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|h| !h.is_finished());
    let (local_url, network_url) = if running {
        (
            state.local_url.lock().unwrap().clone(),
            state.network_url.lock().unwrap().clone(),
        )
    } else {
        (None, None)
    };
    let config = get_web_mirror_config(&app);
    let (username, password) = resolve_credentials(&config, &app);
    WebMirrorStatus {
        running,
        local_url,
        network_url,
        username,
        password,
        config,
    }
}

fn get_sidecar_port() -> u32 {
    option_env!("OPENCODE_PORT")
        .map(|s| s.to_string())
        .or_else(|| std::env::var("OPENCODE_PORT").ok())
        .and_then(|port_str| port_str.parse().ok())
        .unwrap_or_else(|| {
            TcpListener::bind("127.0.0.1:0")
                .expect("Failed to bind to find free port")
                .local_addr()
                .expect("Failed to get local address")
                .port()
        }) as u32
}

fn spawn_sidecar(app: &AppHandle, hostname: &str, port: u32, password: &str) -> CommandChild {
    let log_state = app.state::<LogState>();
    let log_state_clone = log_state.inner().clone();

    let mirror_config = get_web_mirror_config(app);
    let (username, _) = resolve_credentials(&mirror_config, app);

    // Resolve the bundled web UI directory for the sidecar to serve
    let web_dir = app
        .path()
        .resolve("web-ui", tauri::path::BaseDirectory::Resource)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    println!("spawning sidecar on port {port} with username={username} web_dir={web_dir}");

    // Always inline credentials + web dir so they override anything the shell config sets
    let (mut rx, child) = cli::create_command_with_env(
        app,
        format!("serve --hostname {hostname} --port {port}").as_str(),
        &[
            ("OPENCODE_SERVER_USERNAME", &username),
            ("OPENCODE_SERVER_PASSWORD", password),
            ("OPENCODE_WEB_DIR", &web_dir),
        ],
    )
    .spawn()
    .expect("Failed to spawn opencode");

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    print!("{line}");

                    // Store log in shared state
                    if let Ok(mut logs) = log_state_clone.0.lock() {
                        logs.push_back(format!("[STDOUT] {}", line));
                        // Keep only the last MAX_LOG_ENTRIES
                        while logs.len() > MAX_LOG_ENTRIES {
                            logs.pop_front();
                        }
                    }
                }
                CommandEvent::Stderr(line_bytes) => {
                    let line = String::from_utf8_lossy(&line_bytes);
                    eprint!("{line}");

                    // Store log in shared state
                    if let Ok(mut logs) = log_state_clone.0.lock() {
                        logs.push_back(format!("[STDERR] {}", line));
                        // Keep only the last MAX_LOG_ENTRIES
                        while logs.len() > MAX_LOG_ENTRIES {
                            logs.pop_front();
                        }
                    }
                }
                _ => {}
            }
        }
    });

    child
}

fn url_is_localhost(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

async fn check_server_health(url: &str, password: Option<&str>) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };

    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(3));

    if url_is_localhost(&url) {
        // Some environments set proxy variables (HTTP_PROXY/HTTPS_PROXY/ALL_PROXY) without
        // excluding loopback. reqwest respects these by default, which can prevent the desktop
        // app from reaching its own local sidecar server.
        builder = builder.no_proxy();
    };

    let Ok(client) = builder.build() else {
        return false;
    };
    let Ok(health_url) = url.join("/global/health") else {
        return false;
    };

    let mut req = client.get(health_url);

    if let Some(password) = password {
        req = req.basic_auth("opencode", Some(password));
    }

    req.send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let updater_enabled = option_env!("TAURI_SIGNING_PRIVATE_KEY").is_some();

    let builder = tauri_specta::Builder::<tauri::Wry>::new()
        // Then register them (separated by a comma)
        .commands(tauri_specta::collect_commands![
            kill_sidecar,
            install_cli,
            ensure_server_ready,
            get_default_server_url,
            set_default_server_url,
            start_web_mirror,
            stop_web_mirror,
            get_web_mirror_status,
            markdown::parse_markdown_command
        ])
        .error_handling(tauri_specta::ErrorHandlingMode::Throw);

    #[cfg(debug_assertions)] // <- Only export on non-release builds
    builder
        .export(
            specta_typescript::Typescript::default(),
            "../src/bindings.ts",
        )
        .expect("Failed to export typescript bindings");

    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    let _ = std::process::Command::new("killall")
        .arg("opencode-cli")
        .output();

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Focus existing window when another instance is launched
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
                let _ = window.unminimize();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_os::init())
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(PinchZoomDisablePlugin)
        .plugin(tauri_plugin_decorum::init())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);

            #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
            app.deep_link().register_all().ok();

            let app = app.handle().clone();

            // Initialize log state
            app.manage(LogState(Arc::new(Mutex::new(VecDeque::new()))));

            // Initialize web mirror state
            app.manage(WebMirrorState {
                handle: Mutex::new(None),
                local_url: Arc::new(Mutex::new(None)),
                network_url: Arc::new(Mutex::new(None)),
            });

            #[cfg(windows)]
            app.manage(JobObjectState::new());

            let primary_monitor = app.primary_monitor().ok().flatten();
            let size = primary_monitor
                .map(|m| m.size().to_logical(m.scale_factor()))
                .unwrap_or(LogicalSize::new(1920, 1080));

            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .expect("main window config missing");

            let window_builder = WebviewWindowBuilder::from_config(&app, config)
                .expect("Failed to create window builder from config")
                .inner_size(size.width as f64, size.height as f64)
                .initialization_script(format!(
                    r#"
                      window.__OPENCODE__ ??= {{}};
                      window.__OPENCODE__.updaterEnabled = {updater_enabled};
                    "#
                ));

            #[cfg(target_os = "macos")]
            let window_builder = window_builder
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true);

            #[cfg(windows)]
            let window_builder = window_builder
                // Some VPNs set a global/system proxy that WebView2 applies even for loopback
                // connections, which breaks the app's localhost sidecar server.
                // Note: when setting additional args, we must re-apply wry's default
                // `--disable-features=...` flags.
                .additional_browser_args(
                    "--proxy-bypass-list=<-loopback> --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection",
                )
                .decorations(false);

            let window = window_builder.build().expect("Failed to create window");

            setup_window_state_listener(&app, &window);

            #[cfg(windows)]
            let _ = window.create_overlay_titlebar();

            let (tx, rx) = oneshot::channel();
            app.manage(ServerState::new(None, rx));

            {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let mut custom_url = None;

                    if let Some(url) = get_default_server_url(app.clone()).ok().flatten() {
                        println!("Using desktop-specific custom URL: {url}");
                        custom_url = Some(url);
                    }

                    if custom_url.is_none()
                        && let Some(cli_config) = cli::get_config(&app).await
                        && let Some(url) = get_server_url_from_config(&cli_config)
                    {
                        println!("Using custom server URL from config: {url}");
                        custom_url = Some(url);
                    }

                    let res = match setup_server_connection(&app, custom_url).await {
                        Ok((child, url)) => {
                            #[cfg(windows)]
                            if let Some(child) = &child {
                                let job_state = app.state::<JobObjectState>();
                                job_state.assign_pid(child.pid());
                            }

                            app.state::<ServerState>().set_child(child);

                            Ok(url)
                        }
                        Err(e) => Err(e),
                    };

                    // Auto-start web mirror if server is ready and mirror is enabled
                    if let Ok(ref server_data) = res {
                        let mirror_config = get_web_mirror_config(&app);
                        if mirror_config.enabled {
                            println!("Auto-starting web mirror proxy");
                            match spawn_web_mirror_proxy(&server_data.url, &mirror_config) {
                                Ok((handle, local_url, network_url)) => {
                                    let state = app.state::<WebMirrorState>();
                                    *state.handle.lock().unwrap() = Some(handle);
                                    *state.local_url.lock().unwrap() = Some(local_url.clone());
                                    *state.network_url.lock().unwrap() = network_url.clone();
                                    println!("Web mirror proxy started at {local_url}");
                                    if let Some(ref url) = network_url {
                                        println!("Web mirror network access: {url}");
                                    }
                                }
                                Err(e) => eprintln!("Failed to auto-start web mirror proxy: {e}"),
                            }
                        }
                    }

                    let _ = tx.send(res);
                });
            }

            {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = sync_cli(app) {
                        eprintln!("Failed to sync CLI: {e}");
                    }
                });
            }

            Ok(())
        });

    if updater_enabled {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }

    builder
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                println!("Received Exit");

                // Stop web mirror proxy if running
                if let Some(state) = app.try_state::<WebMirrorState>() {
                    if let Some(handle) = state.handle.lock().unwrap().take() {
                        handle.abort();
                        println!("Stopped web mirror proxy");
                    }
                }

                kill_sidecar(app.clone());
            }
        });
}

/// Converts a bind address hostname to a valid URL hostname for connection.
/// - `0.0.0.0` and `::` are wildcard bind addresses, not valid connect targets
/// - IPv6 addresses need brackets in URLs (e.g., `::1` -> `[::1]`)
fn normalize_hostname_for_url(hostname: &str) -> String {
    // Wildcard bind addresses -> localhost equivalents
    if hostname == "0.0.0.0" {
        return "127.0.0.1".to_string();
    }
    if hostname == "::" {
        return "[::1]".to_string();
    }

    // IPv6 addresses need brackets in URLs
    if hostname.contains(':') && !hostname.starts_with('[') {
        return format!("[{}]", hostname);
    }

    hostname.to_string()
}

fn get_server_url_from_config(config: &cli::Config) -> Option<String> {
    let server = config.server.as_ref()?;
    let port = server.port?;
    println!("server.port found in OC config: {port}");
    let hostname = server
        .hostname
        .as_ref()
        .map(|v| normalize_hostname_for_url(v))
        .unwrap_or_else(|| "127.0.0.1".to_string());

    Some(format!("http://{}:{}", hostname, port))
}

async fn setup_server_connection(
    app: &AppHandle,
    custom_url: Option<String>,
) -> Result<(Option<CommandChild>, ServerReadyData), String> {
    if let Some(url) = custom_url {
        loop {
            if check_server_health(&url, None).await {
                println!("Connected to custom server: {}", url);
                return Ok((
                    None,
                    ServerReadyData {
                        url: url.clone(),
                        password: None,
                    },
                ));
            }

            const RETRY: &str = "Retry";

            let res = app.dialog()
              .message(format!("Could not connect to configured server:\n{}\n\nWould you like to retry or start a local server instead?", url))
              .title("Connection Failed")
              .buttons(MessageDialogButtons::OkCancelCustom(RETRY.to_string(), "Start Local".to_string()))
              .blocking_show_with_result();

            match res {
                MessageDialogResult::Custom(name) if name == RETRY => {
                    continue;
                }
                _ => {
                    break;
                }
            }
        }
    }

    let local_port = get_sidecar_port();
    let hostname = "127.0.0.1";
    let local_url = format!("http://{hostname}:{local_port}");

    if !check_server_health(&local_url, None).await {
        // Resolve credentials: Config (Settings UI) → Shell env → generated UUID
        let mirror_config = get_web_mirror_config(app);
        let (_, password) = resolve_credentials(&mirror_config, app);

        match spawn_local_server(app, hostname, local_port, &password).await {
            Ok(child) => Ok((
                Some(child),
                ServerReadyData {
                    url: local_url,
                    password: Some(password),
                },
            )),
            Err(err) => Err(err),
        }
    } else {
        Ok((
            None,
            ServerReadyData {
                url: local_url,
                password: None,
            },
        ))
    }
}

async fn spawn_local_server(
    app: &AppHandle,
    hostname: &str,
    port: u32,
    password: &str,
) -> Result<CommandChild, String> {
    let child = spawn_sidecar(app, hostname, port, password);
    let url = format!("http://{hostname}:{port}");

    let timestamp = Instant::now();
    loop {
        if timestamp.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            break Err(format!(
                "Failed to spawn OpenCode Server. Logs:\n{}",
                get_logs(app.clone()).await.unwrap()
            ));
        }

        tokio::time::sleep(Duration::from_millis(10)).await;

        if check_server_health(&url, Some(password)).await {
            println!("Server ready after {:?}", timestamp.elapsed());
            break Ok(child);
        }
    }
}

fn setup_window_state_listener(app: &tauri::AppHandle, window: &tauri::WebviewWindow) {
    let (tx, mut rx) = mpsc::channel::<()>(1);

    window.on_window_event(move |event| {
        use tauri::WindowEvent;
        if !matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
            return;
        }
        let _ = tx.try_send(());
    });

    tauri::async_runtime::spawn({
        let app = app.clone();

        async move {
            let save = || {
                let handle = app.clone();
                let app = app.clone();
                let _ = handle.run_on_main_thread(move || {
                    let _ = app.save_window_state(window_state_flags());
                });
            };

            while rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(200)).await;

                save();
            }
        }
    });
}
