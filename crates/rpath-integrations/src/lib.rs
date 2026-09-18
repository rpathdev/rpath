use rpath_core::{state_dir, RpathError, RpathResult, ShellKind};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationTarget {
    Vscode,
    Explorer,
    Wsl,
    GitBash,
}

impl IntegrationTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vscode => "vscode",
            Self::Explorer => "explorer",
            Self::Wsl => "wsl",
            Self::GitBash => "git-bash",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationAction {
    Install,
    Uninstall,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationReport {
    pub target: IntegrationTarget,
    pub action: IntegrationAction,
    pub changed: bool,
    pub supported: bool,
    pub path: Option<String>,
    pub message: String,
}

pub fn run_integration(
    target: IntegrationTarget,
    action: IntegrationAction,
    shell: ShellKind,
    dry_run: bool,
) -> RpathResult<IntegrationReport> {
    match target {
        IntegrationTarget::Vscode => vscode(action, shell, dry_run),
        IntegrationTarget::Explorer => explorer(action, dry_run),
        IntegrationTarget::Wsl => wsl(action, dry_run),
        IntegrationTarget::GitBash => git_bash(action, dry_run),
    }
}

pub fn install_watch_service(dry_run: bool) -> RpathResult<IntegrationReport> {
    if cfg!(windows) {
        if dry_run {
            return Ok(report(
                IntegrationTarget::Explorer,
                IntegrationAction::Install,
                false,
                true,
                None,
                "dry run: scheduled task rpath-watch would be created",
            ));
        }
        let status = Command::new("schtasks")
            .args(["/Create", "/TN", "rpath-watch", "/SC", "ONLOGON", "/TR", "rpath watch", "/F"])
            .status();
        return Ok(report(
            IntegrationTarget::Explorer,
            IntegrationAction::Install,
            status.map(|s| s.success()).unwrap_or(false),
            true,
            None,
            "attempted to install Windows scheduled task rpath-watch",
        ));
    }

    let service_path = watch_service_path()?;
    if dry_run {
        return Ok(report(
            IntegrationTarget::Vscode,
            IntegrationAction::Install,
            false,
            true,
            Some(service_path.to_string_lossy().to_string()),
            "dry run: user watch service file would be written",
        ));
    }

    if let Some(parent) = service_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&service_path, watch_service_contents())?;
    Ok(report(
        IntegrationTarget::Vscode,
        IntegrationAction::Install,
        true,
        true,
        Some(service_path.to_string_lossy().to_string()),
        "installed user watch service definition",
    ))
}

pub fn uninstall_watch_service(dry_run: bool) -> RpathResult<IntegrationReport> {
    if cfg!(windows) {
        if !dry_run {
            let _ = Command::new("schtasks").args(["/Delete", "/TN", "rpath-watch", "/F"]).status();
        }
        return Ok(report(
            IntegrationTarget::Explorer,
            IntegrationAction::Uninstall,
            !dry_run,
            true,
            None,
            "attempted to remove Windows scheduled task rpath-watch",
        ));
    }

    let service_path = watch_service_path()?;
    let changed = service_path.exists();
    if changed && !dry_run {
        fs::remove_file(&service_path)?;
    }
    Ok(report(
        IntegrationTarget::Vscode,
        IntegrationAction::Uninstall,
        changed,
        true,
        Some(service_path.to_string_lossy().to_string()),
        if dry_run {
            "dry run: user watch service file would be removed"
        } else {
            "removed user watch service definition"
        },
    ))
}

pub fn watch_service_status() -> RpathResult<IntegrationReport> {
    if cfg!(windows) {
        let installed = Command::new("schtasks")
            .args(["/Query", "/TN", "rpath-watch"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        return Ok(report(
            IntegrationTarget::Explorer,
            IntegrationAction::Status,
            false,
            true,
            None,
            if installed {
                "Windows scheduled task rpath-watch is installed"
            } else {
                "Windows scheduled task rpath-watch is not installed"
            },
        ));
    }

    let service_path = watch_service_path()?;
    Ok(report(
        IntegrationTarget::Vscode,
        IntegrationAction::Status,
        false,
        true,
        Some(service_path.to_string_lossy().to_string()),
        if service_path.exists() {
            "user watch service definition exists"
        } else {
            "user watch service definition is not installed"
        },
    ))
}

fn vscode(
    action: IntegrationAction,
    shell: ShellKind,
    dry_run: bool,
) -> RpathResult<IntegrationReport> {
    let marker = integration_marker_path(IntegrationTarget::Vscode)?;
    match action {
        IntegrationAction::Install => {
            let shell_report = rpath_shell::install_shell(shell, dry_run)
                .map_err(|error| RpathError::State(error.to_string()))?;
            if !dry_run {
                write_marker(&marker, "vscode shell profile hook installed")?;
            }
            Ok(report(
                IntegrationTarget::Vscode,
                action,
                shell_report.changed,
                true,
                Some(marker.to_string_lossy().to_string()),
                "VS Code integration uses the installed shell wrapper for integrated terminals",
            ))
        }
        IntegrationAction::Uninstall => {
            if marker.exists() && !dry_run {
                fs::remove_file(&marker)?;
            }
            Ok(report(
                IntegrationTarget::Vscode,
                action,
                marker.exists(),
                true,
                Some(marker.to_string_lossy().to_string()),
                "removed VS Code integration marker; shell wrapper is left untouched",
            ))
        }
        IntegrationAction::Status => Ok(report(
            IntegrationTarget::Vscode,
            action,
            false,
            true,
            Some(marker.to_string_lossy().to_string()),
            if marker.exists() {
                "VS Code integration marker is installed"
            } else {
                "VS Code integration marker is not installed"
            },
        )),
    }
}

fn explorer(action: IntegrationAction, dry_run: bool) -> RpathResult<IntegrationReport> {
    if !cfg!(windows) {
        return Ok(report(
            IntegrationTarget::Explorer,
            action,
            false,
            false,
            None,
            "Windows Explorer integration is only supported on Windows",
        ));
    }

    match action {
        IntegrationAction::Install => {
            if !dry_run {
                let _ = Command::new("reg")
                    .args([
                        "add",
                        r"HKCU\Software\Classes\Directory\Background\shell\rpath",
                        "/ve",
                        "/d",
                        "Refresh environment with rpath",
                        "/f",
                    ])
                    .status();
                let _ = Command::new("reg")
                    .args([
                        "add",
                        r"HKCU\Software\Classes\Directory\Background\shell\rpath\command",
                        "/ve",
                        "/d",
                        r#"powershell -NoProfile -ExecutionPolicy Bypass -Command "rpath --emit --shell powershell | Invoke-Expression""#,
                        "/f",
                    ])
                    .status();
                broadcast_environment_change();
            }
            Ok(report(
                IntegrationTarget::Explorer,
                action,
                !dry_run,
                true,
                None,
                "attempted to install HKCU Windows Explorer context menu integration",
            ))
        }
        IntegrationAction::Uninstall => {
            if !dry_run {
                let _ = Command::new("reg")
                    .args([
                        "delete",
                        r"HKCU\Software\Classes\Directory\Background\shell\rpath",
                        "/f",
                    ])
                    .status();
                broadcast_environment_change();
            }
            Ok(report(
                IntegrationTarget::Explorer,
                action,
                !dry_run,
                true,
                None,
                "attempted to remove HKCU Windows Explorer context menu integration",
            ))
        }
        IntegrationAction::Status => {
            let installed = Command::new("reg")
                .args(["query", r"HKCU\Software\Classes\Directory\Background\shell\rpath"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            Ok(report(
                IntegrationTarget::Explorer,
                action,
                false,
                true,
                None,
                if installed {
                    "Explorer integration is installed"
                } else {
                    "Explorer integration is not installed"
                },
            ))
        }
    }
}

fn wsl(action: IntegrationAction, dry_run: bool) -> RpathResult<IntegrationReport> {
    let target = integration_marker_path(IntegrationTarget::Wsl)?.with_extension("sh");
    match action {
        IntegrationAction::Install => {
            if !dry_run {
                write_shell_snippet(&target, wsl_snippet())?;
            }
            Ok(report(
                IntegrationTarget::Wsl,
                action,
                !dry_run,
                true,
                Some(target.to_string_lossy().to_string()),
                "installed WSL sync snippet in rpath state; source it from WSL shell startup",
            ))
        }
        IntegrationAction::Uninstall => {
            let changed = target.exists();
            if changed && !dry_run {
                fs::remove_file(&target)?;
            }
            Ok(report(
                IntegrationTarget::Wsl,
                action,
                changed,
                true,
                Some(target.to_string_lossy().to_string()),
                "removed WSL sync snippet",
            ))
        }
        IntegrationAction::Status => Ok(report(
            IntegrationTarget::Wsl,
            action,
            false,
            true,
            Some(target.to_string_lossy().to_string()),
            if target.exists() {
                "WSL sync snippet exists"
            } else {
                "WSL sync snippet is not installed"
            },
        )),
    }
}

fn git_bash(action: IntegrationAction, dry_run: bool) -> RpathResult<IntegrationReport> {
    match action {
        IntegrationAction::Install => {
            let shell_report = rpath_shell::install_shell(ShellKind::Bash, dry_run)
                .map_err(|error| RpathError::State(error.to_string()))?;
            Ok(report(
                IntegrationTarget::GitBash,
                action,
                shell_report.changed,
                true,
                shell_report.target,
                "installed bash wrapper for Git Bash/MSYS-compatible shells",
            ))
        }
        IntegrationAction::Uninstall => {
            let shell_report = rpath_shell::uninstall_shell(ShellKind::Bash, dry_run)
                .map_err(|error| RpathError::State(error.to_string()))?;
            Ok(report(
                IntegrationTarget::GitBash,
                action,
                shell_report.changed,
                true,
                shell_report.target,
                "removed bash wrapper used by Git Bash/MSYS-compatible shells",
            ))
        }
        IntegrationAction::Status => {
            let path = dirs::home_dir().map(|home| home.join(".bashrc"));
            let installed = path
                .as_ref()
                .and_then(|path| fs::read_to_string(path).ok())
                .map(|contents| contents.contains("rpath initialize"))
                .unwrap_or(false);
            Ok(report(
                IntegrationTarget::GitBash,
                action,
                false,
                true,
                path.map(|path| path.to_string_lossy().to_string()),
                if installed {
                    "Git Bash wrapper appears to be installed"
                } else {
                    "Git Bash wrapper is not installed"
                },
            ))
        }
    }
}

fn integration_marker_path(target: IntegrationTarget) -> RpathResult<PathBuf> {
    Ok(state_dir()?.join("integrations").join(format!("{}.json", target.as_str())))
}

fn write_marker(path: &PathBuf, message: &str) -> RpathResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({
        "message": message,
        "created_at_unix": unix_now(),
    });
    fs::write(path, serde_json::to_string_pretty(&payload)?)?;
    Ok(())
}

fn write_shell_snippet(path: &PathBuf, snippet: &str) -> RpathResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, snippet)?;
    Ok(())
}

fn wsl_snippet() -> &'static str {
    r#"# rpath WSL sync helper
rpath-sync-windows() {
    if command -v powershell.exe >/dev/null 2>&1; then
        powershell.exe -NoProfile -Command 'rpath --emit --shell bash' | tr -d '\r' | sed 's#\\#/#g'
    fi
}
"#
}

fn broadcast_environment_change() {
    if !cfg!(windows) {
        return;
    }
    let _ = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[Environment]::SetEnvironmentVariable('RPATH_LAST_BROADCAST', [DateTimeOffset]::UtcNow.ToUnixTimeSeconds().ToString(), 'User')",
        ])
        .status();
}

fn watch_service_path() -> RpathResult<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = dirs::home_dir()
            .ok_or_else(|| RpathError::State("could not resolve home directory".to_string()))?;
        Ok(home.join("Library").join("LaunchAgents").join("dev.byjonas.rpath.watch.plist"))
    } else {
        let config = dirs::config_dir()
            .ok_or_else(|| RpathError::State("could not resolve config directory".to_string()))?;
        Ok(config.join("systemd").join("user").join("rpath-watch.service"))
    }
}

fn watch_service_contents() -> String {
    if cfg!(target_os = "macos") {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.byjonas.rpath.watch</string>
  <key>ProgramArguments</key>
  <array><string>rpath</string><string>watch</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
"#
        .to_string()
    } else {
        "[Unit]\nDescription=rpath environment watcher\n\n[Service]\nExecStart=rpath watch\nRestart=always\n\n[Install]\nWantedBy=default.target\n".to_string()
    }
}

fn report(
    target: IntegrationTarget,
    action: IntegrationAction,
    changed: bool,
    supported: bool,
    path: Option<String>,
    message: impl Into<String>,
) -> IntegrationReport {
    IntegrationReport { target, action, changed, supported, path, message: message.into() }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
