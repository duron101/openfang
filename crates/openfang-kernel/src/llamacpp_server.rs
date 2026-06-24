//! llama.cpp server — managed subprocess for local GGUF inference.
//!
//! When `[llamacpp] enabled = true`, the kernel spawns `llama-server` before
//! creating the default LLM driver and wires `[default_model]` to its
//! OpenAI-compatible `/v1` endpoint.

use openfang_types::config::LlamaCppConfig;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;

/// Apply llama.cpp URL/model defaults and optionally spawn the server process.
///
/// Returns the child PID when a managed process was started.
pub fn prepare(config: &mut openfang_types::config::KernelConfig) -> Result<Option<u32>, String> {
    if !config.llamacpp.is_active() {
        return Ok(None);
    }

    wire_default_model(config);

    if config.llamacpp.external {
        info!(
            url = %config.llamacpp.base_url(),
            "llama.cpp external mode — skipping process spawn"
        );
        return Ok(None);
    }

    if !config.llamacpp.model_path.exists() {
        return Err(format!(
            "llama.cpp model_path not found: {}",
            config.llamacpp.model_path.display()
        ));
    }

    let binary = resolve_binary_path(&config.llamacpp)?;
    let mut child = spawn_server(&binary, &config.llamacpp)?;
    let pid = child.id();
    wait_for_server_ready(
        &config.llamacpp,
        config.llamacpp.startup_timeout_secs,
        &mut child,
    )?;
    info!(
        pid,
        url = %config.llamacpp.base_url(),
        model = %config.llamacpp.model_path.display(),
        "llama.cpp server ready"
    );
    Ok(Some(pid))
}

fn wire_default_model(config: &mut openfang_types::config::KernelConfig) {
    let base_url = config.llamacpp.base_url();
    config
        .provider_urls
        .entry("llamacpp".to_string())
        .or_insert_with(|| base_url.clone());

    if config.default_model.provider == "llamacpp" {
        if config.default_model.base_url.is_none() {
            config.default_model.base_url = Some(base_url);
        }
        if config.default_model.api_key_env.is_empty() {
            config.default_model.api_key_env.clear();
        }
        if config.default_model.model.is_empty() {
            config.default_model.model = config.llamacpp.resolved_model_name();
        }
    }
}

fn resolve_binary_path(cfg: &LlamaCppConfig) -> Result<PathBuf, String> {
    if let Some(path) = &cfg.binary_path {
        if path.exists() {
            return Ok(path.clone());
        }
        return Err(format!(
            "llama.cpp binary_path not found: {}",
            path.display()
        ));
    }

    if let Ok(path) = std::env::var("LLAMACPP_SERVER_PATH") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    for candidate in bundled_binary_candidates() {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(
        "llama-server binary not found. Set [llamacpp].binary_path, LLAMACPP_SERVER_PATH, \
         or place llama-server under public/llamaCPP/"
            .to_string(),
    )
}

fn bundled_binary_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let exe_name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };

    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("public/llamaCPP").join(exe_name));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("public/llamaCPP").join(exe_name));
            candidates.push(dir.join(exe_name));
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("public/llamaCPP").join(exe_name));
                if let Some(grand) = parent.parent() {
                    candidates.push(grand.join("public/llamaCPP").join(exe_name));
                }
            }
        }
    }

    candidates
}

fn spawn_server(binary: &Path, cfg: &LlamaCppConfig) -> Result<Child, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("-m").arg(&cfg.model_path);
    cmd.arg("--host").arg(&cfg.host);
    cmd.arg("--port").arg(cfg.port.to_string());

    if let Some(ctx) = cfg.ctx_size {
        cmd.arg("-c").arg(ctx.to_string());
    }
    if let Some(threads) = cfg.threads {
        cmd.arg("-t").arg(threads.to_string());
    }
    if let Some(ngl) = cfg.n_gpu_layers {
        cmd.arg("-ngl").arg(ngl.to_string());
    }
    for arg in &cfg.extra_args {
        cmd.arg(arg);
    }

    // AMD iGPU (e.g. 880M) may fail vkCreateDevice with coopmat extensions enabled.
    // Respect an explicit parent override; otherwise disable coopmat for the child.
    if let Ok(value) = std::env::var("GGML_VK_DISABLE_COOPMAT") {
        cmd.env("GGML_VK_DISABLE_COOPMAT", value);
    } else {
        cmd.env("GGML_VK_DISABLE_COOPMAT", "1");
    }

    if let Some(dir) = binary.parent() {
        cmd.current_dir(dir);
    }

    let log_dir = crate::config::openfang_home().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("llamacpp-server.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open llama.cpp log {}: {e}", log_path.display()))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| format!("clone llama.cpp log handle: {e}"))?;

    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file_err));

    info!(
        binary = %binary.display(),
        model = %cfg.model_path.display(),
        host = %cfg.host,
        port = cfg.port,
        log = %log_path.display(),
        "starting llama.cpp server"
    );

    cmd.spawn()
        .map_err(|e| format!("spawn llama-server ({}): {e}", binary.display()))
}

fn wait_for_server_ready(
    cfg: &LlamaCppConfig,
    timeout_secs: u64,
    child: &mut Child,
) -> Result<(), String> {
    let base = format!("{}:{}", cfg.host, cfg.port);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let mut attempt = 0u32;

    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("poll llama-server process: {e}"))?
        {
            return Err(format!(
                "llama-server exited before becoming ready (status: {status}). \
                 Check ~/.openfang/logs/llamacpp-server.log for details"
            ));
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "llama.cpp server not ready after {timeout_secs}s (probed {base})"
            ));
        }

        if probe_http_endpoint(&cfg.host, cfg.port, "/health")?
            || probe_http_endpoint(&cfg.host, cfg.port, "/v1/models")?
            || probe_http_endpoint(&cfg.host, cfg.port, "/models")?
        {
            return Ok(());
        }

        attempt += 1;
        if attempt == 1 || attempt.is_multiple_of(15) {
            info!(
                attempt,
                url = %base,
                "waiting for llama.cpp server..."
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn probe_http_endpoint(host: &str, port: u16, path: &str) -> Result<bool, String> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve llama.cpp address {host}:{port}: {e}"))?;
    let Some(addr) = addrs.next() else {
        return Err(format!("resolve llama.cpp address {host}:{port}: no addresses"));
    };

    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(stream) => stream,
        Err(_) => return Ok(false),
    };

    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Ok(false);
    }

    let mut response = [0u8; 64];
    let bytes_read = match stream.read(&mut response) {
        Ok(n) => n,
        Err(_) => return Ok(false),
    };
    let status_line = String::from_utf8_lossy(&response[..bytes_read]);

    Ok(status_line.starts_with("HTTP/1.1 200")
        || status_line.starts_with("HTTP/1.0 200")
        || status_line.starts_with("HTTP/1.1 404")
        || status_line.starts_with("HTTP/1.0 404"))
}

/// Best-effort terminate a managed llama.cpp server process.
pub fn stop_server(pid: u32) {
    info!("Stopping llama.cpp server (PID {pid})...");
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_default_model_sets_base_url() {
        let mut config = openfang_types::config::KernelConfig::default();
        config.llamacpp.enabled = true;
        config.llamacpp.model_path = PathBuf::from("D:/models/demo.gguf");
        config.default_model.provider = "llamacpp".to_string();
        config.default_model.model.clear();
        wire_default_model(&mut config);
        assert_eq!(
            config.default_model.base_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
        assert_eq!(config.default_model.model, "demo");
    }
}
