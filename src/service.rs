//! Platform-aware service management for evo-gateway.
//!
//! Generates and manages launchd plists (macOS) or systemd units (Linux)
//! so the gateway can run as a background service.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Install evo-gateway as a system service
    Install,
    /// Show service status
    Status,
    /// Start the service
    Start,
    /// Stop the service
    Stop,
    /// Restart the service
    Restart,
    /// Uninstall the service
    Uninstall,
}

pub async fn run_service_command(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install_service().await,
        ServiceAction::Status => service_status().await,
        ServiceAction::Start => start_service().await,
        ServiceAction::Stop => stop_service().await,
        ServiceAction::Restart => restart_service().await,
        ServiceAction::Uninstall => uninstall_service().await,
    }
}

// ── macOS launchd ───────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
const SERVICE_LABEL: &str = "com.evo.gateway";

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join("Library/LaunchAgents")
        .join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn gateway_binary() -> Result<String> {
    // Prefer the binary in PATH (symlinked via install.sh or brew)
    let output = std::process::Command::new("which")
        .arg("evo-gateway")
        .output()
        .context("failed to locate evo-gateway binary")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        // Fallback: binary next to this running executable
        let exe = std::env::current_exe().context("cannot determine current executable path")?;
        Ok(exe.to_string_lossy().to_string())
    }
}

#[cfg(target_os = "macos")]
fn data_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".evo-gateway")
}

#[cfg(target_os = "macos")]
fn generate_plist(binary: &str) -> String {
    let data = data_dir();
    let config = data.join("gateway.json");
    let log_dir = data.join("logs");
    let db_path = data.join("gateway.db");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{data}</string>
    <key>StandardOutPath</key>
    <string>{log_dir}/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/stderr.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>GATEWAY_CONFIG</key>
        <string>{config}</string>
        <key>EVO_LOG_DIR</key>
        <string>{log_dir}</string>
        <key>EVO_GATEWAY_DB_PATH</key>
        <string>{db_path}</string>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>"#,
        data = data.display(),
        config = config.display(),
        log_dir = log_dir.display(),
        db_path = db_path.display(),
    )
}

#[cfg(target_os = "macos")]
async fn install_service() -> Result<()> {
    let binary = gateway_binary()?;
    let data = data_dir();

    // Create data directories
    std::fs::create_dir_all(data.join("logs")).context("failed to create data directory")?;

    // Copy config if it doesn't exist yet
    let config_path = data.join("gateway.json");
    if !config_path.exists() {
        // Check current directory for gateway.json
        if std::path::Path::new("gateway.json").exists() {
            std::fs::copy("gateway.json", &config_path)
                .context("failed to copy gateway.json to data directory")?;
            println!("  Copied gateway.json -> {}", config_path.display());
        } else {
            println!("  Note: No gateway.json found. The service will create a default config.");
        }
    }

    // Generate and write plist
    let plist = generate_plist(&binary);
    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap())
        .context("failed to create LaunchAgents directory")?;
    std::fs::write(&path, &plist)
        .with_context(|| format!("failed to write plist to {}", path.display()))?;

    println!("Service installed:");
    println!("  Plist:   {}", path.display());
    println!("  Binary:  {binary}");
    println!("  Data:    {}", data.display());
    println!("  Config:  {}", data.join("gateway.json").display());
    println!("  Logs:    {}", data.join("logs").display());
    println!();
    println!("Start with: evo-gateway service start");
    println!("Or:         launchctl load {}", path.display());

    Ok(())
}

#[cfg(target_os = "macos")]
async fn service_status() -> Result<()> {
    let path = plist_path();

    if !path.exists() {
        println!("Service not installed. Run: evo-gateway service install");
        return Ok(());
    }

    let output = std::process::Command::new("launchctl")
        .args(["list", SERVICE_LABEL])
        .output()
        .context("failed to run launchctl list")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        println!("Service: {SERVICE_LABEL}");
        println!("Status:  running");
        // Parse PID from output
        for line in stdout.lines() {
            if line.contains("PID") {
                println!("  {line}");
            }
        }
    } else {
        println!("Service: {SERVICE_LABEL}");
        println!("Status:  stopped");
    }
    println!("Plist:   {}", path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
async fn start_service() -> Result<()> {
    let path = plist_path();
    if !path.exists() {
        bail!("Service not installed. Run: evo-gateway service install");
    }

    let status = std::process::Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status()
        .context("failed to run launchctl load")?;

    if status.success() {
        println!("Service started.");
        println!("Check status: evo-gateway service status");
        println!(
            "View logs:    tail -f {}",
            data_dir().join("logs/stdout.log").display()
        );
    } else {
        bail!("launchctl load failed (exit code: {status})");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn stop_service() -> Result<()> {
    let path = plist_path();
    if !path.exists() {
        bail!("Service not installed.");
    }

    let status = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status()
        .context("failed to run launchctl unload")?;

    if status.success() {
        println!("Service stopped.");
    } else {
        bail!("launchctl unload failed (exit code: {status})");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn restart_service() -> Result<()> {
    let path = plist_path();
    if !path.exists() {
        bail!("Service not installed. Run: evo-gateway service install");
    }

    // Unload (ignore failure — might not be loaded)
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();

    let status = std::process::Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status()
        .context("failed to run launchctl load")?;

    if status.success() {
        println!("Service restarted.");
    } else {
        bail!("launchctl load failed (exit code: {status})");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn uninstall_service() -> Result<()> {
    let path = plist_path();

    // Stop if running
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();

    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
        println!("Service uninstalled (plist removed).");
        println!("Data directory preserved at: {}", data_dir().display());
    } else {
        println!("Service was not installed.");
    }
    Ok(())
}

// ── Linux systemd ───────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
const SERVICE_NAME: &str = "evo-gateway";

#[cfg(target_os = "linux")]
fn unit_path() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".config/systemd/user")
        .join(format!("{SERVICE_NAME}.service"))
}

#[cfg(target_os = "linux")]
fn data_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".evo-gateway")
}

#[cfg(target_os = "linux")]
fn gateway_binary() -> Result<String> {
    let output = std::process::Command::new("which")
        .arg("evo-gateway")
        .output()
        .context("failed to locate evo-gateway binary")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let exe = std::env::current_exe().context("cannot determine current executable path")?;
        Ok(exe.to_string_lossy().to_string())
    }
}

#[cfg(target_os = "linux")]
fn generate_unit(binary: &str) -> String {
    let data = data_dir();
    let config = data.join("gateway.json");
    let log_dir = data.join("logs");
    let db_path = data.join("gateway.db");

    format!(
        r#"[Unit]
Description=evo-gateway - Unified LLM API Gateway
After=network.target

[Service]
Type=simple
ExecStart={binary}
WorkingDirectory={data}
Restart=on-failure
RestartSec=5
Environment=GATEWAY_CONFIG={config}
Environment=EVO_LOG_DIR={log_dir}
Environment=EVO_GATEWAY_DB_PATH={db_path}
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#,
        data = data.display(),
        config = config.display(),
        log_dir = log_dir.display(),
        db_path = db_path.display(),
    )
}

#[cfg(target_os = "linux")]
async fn install_service() -> Result<()> {
    let binary = gateway_binary()?;
    let data = data_dir();

    std::fs::create_dir_all(data.join("logs")).context("failed to create data directory")?;

    let config_path = data.join("gateway.json");
    if !config_path.exists() && std::path::Path::new("gateway.json").exists() {
        std::fs::copy("gateway.json", &config_path)
            .context("failed to copy gateway.json to data directory")?;
        println!("  Copied gateway.json -> {}", config_path.display());
    }

    let unit = generate_unit(&binary);
    let path = unit_path();
    std::fs::create_dir_all(path.parent().unwrap())
        .context("failed to create systemd user directory")?;
    std::fs::write(&path, &unit)
        .with_context(|| format!("failed to write unit to {}", path.display()))?;

    // Reload systemd
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    println!("Service installed:");
    println!("  Unit:    {}", path.display());
    println!("  Binary:  {binary}");
    println!("  Data:    {}", data.display());
    println!();
    println!("Start with: evo-gateway service start");
    println!("Or:         systemctl --user start {SERVICE_NAME}");

    Ok(())
}

#[cfg(target_os = "linux")]
async fn service_status() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "status", SERVICE_NAME])
        .status()
        .context("failed to run systemctl status")?;

    if !status.success() {
        println!("Service may not be installed. Run: evo-gateway service install");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn start_service() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", SERVICE_NAME])
        .status()
        .context("failed to start service")?;

    if status.success() {
        println!("Service started.");
    } else {
        bail!("systemctl start failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn stop_service() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "stop", SERVICE_NAME])
        .status()
        .context("failed to stop service")?;

    if status.success() {
        println!("Service stopped.");
    } else {
        bail!("systemctl stop failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn restart_service() -> Result<()> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", SERVICE_NAME])
        .status()
        .context("failed to restart service")?;

    if status.success() {
        println!("Service restarted.");
    } else {
        bail!("systemctl restart failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn uninstall_service() -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", SERVICE_NAME])
        .status();

    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", SERVICE_NAME])
        .status();

    let path = unit_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("Service uninstalled.");
        println!("Data directory preserved at: {}", data_dir().display());
    } else {
        println!("Service was not installed.");
    }
    Ok(())
}

// ── Unsupported platforms ───────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn install_service() -> Result<()> {
    bail!("Service management is only supported on macOS (launchd) and Linux (systemd).");
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn service_status() -> Result<()> {
    bail!("Service management is only supported on macOS and Linux.");
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn start_service() -> Result<()> {
    bail!("Service management is only supported on macOS and Linux.");
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn stop_service() -> Result<()> {
    bail!("Service management is only supported on macOS and Linux.");
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn restart_service() -> Result<()> {
    bail!("Service management is only supported on macOS and Linux.");
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn uninstall_service() -> Result<()> {
    bail!("Service management is only supported on macOS and Linux.");
}
