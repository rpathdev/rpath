use rpath_core::{DiagnosticSeverity, EnvironmentPlan, EnvironmentSnapshot, ShellKind};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot emit shell commands because the environment plan contains errors")]
    UnsafePlan,
    #[error("unsupported shell operation for {0}")]
    UnsupportedShell(ShellKind),
    #[error("could not resolve home directory")]
    MissingHome,
}

pub type ShellResult<T> = Result<T, ShellError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitPlan {
    pub shell: ShellKind,
    pub commands: String,
    pub path: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    pub shell: ShellKind,
    pub target: Option<String>,
    pub backup: Option<String>,
    pub changed: bool,
    pub message: String,
    pub snippet: String,
}

const MARKER_START: &str = "# >>> rpath initialize >>>";
const MARKER_END: &str = "# <<< rpath initialize <<<";
const CMD_MARKER_START: &str = "rem >>> rpath initialize >>>";
const CMD_MARKER_END: &str = "rem <<< rpath initialize <<<";

pub fn emit_environment(plan: &EnvironmentPlan, shell: ShellKind) -> ShellResult<EmitPlan> {
    if plan.has_errors() {
        return Err(ShellError::UnsafePlan);
    }

    let commands = match shell {
        ShellKind::Cmd => emit_cmd_path(&plan.path),
        ShellKind::PowerShell | ShellKind::Pwsh => emit_powershell_path(&plan.path),
        ShellKind::Bash | ShellKind::Zsh => emit_posix_path(&plan.path),
        ShellKind::Fish => emit_fish_path(&plan.path),
    };

    Ok(EmitPlan {
        shell,
        commands,
        path: plan.path.clone(),
        warnings: plan
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
            .map(|diagnostic| diagnostic.message.clone())
            .collect(),
    })
}

pub fn emit_snapshot(snapshot: &EnvironmentSnapshot, shell: ShellKind) -> EmitPlan {
    let commands = match shell {
        ShellKind::Cmd => emit_cmd_path(&snapshot.path),
        ShellKind::PowerShell | ShellKind::Pwsh => emit_powershell_path(&snapshot.path),
        ShellKind::Bash | ShellKind::Zsh => emit_posix_path(&snapshot.path),
        ShellKind::Fish => emit_fish_path(&snapshot.path),
    };

    EmitPlan { shell, commands, path: snapshot.path.clone(), warnings: Vec::new() }
}

pub fn wrapper_snippet(shell: ShellKind) -> String {
    match shell {
        ShellKind::PowerShell | ShellKind::Pwsh => format!(
            r#"{MARKER_START}
function rpath {{
    $rpathCommand = Get-Command rpath.exe -CommandType Application -ErrorAction SilentlyContinue
    if (-not $rpathCommand) {{
        Write-Error "rpath.exe was not found on PATH"
        return
    }}
    if ($args.Count -eq 0) {{
        $env:RPATH_WRAPPED = "1"
        try {{
            Invoke-Expression (& $rpathCommand.Source --emit --shell {shell})
        }} finally {{
            Remove-Item Env:RPATH_WRAPPED -ErrorAction SilentlyContinue
        }}
    }} else {{
        & $rpathCommand.Source @args
    }}
}}
{MARKER_END}"#,
            shell = shell.as_str()
        ),
        ShellKind::Bash | ShellKind::Zsh => format!(
            r#"{MARKER_START}
rpath() {{
    if [ "$#" -eq 0 ]; then
        eval "$(RPATH_WRAPPED=1 command rpath --emit --shell {shell})"
    else
        command rpath "$@"
    fi
}}
{MARKER_END}"#,
            shell = shell.as_str()
        ),
        ShellKind::Fish => format!(
            r#"{MARKER_START}
function rpath
    if test (count $argv) -eq 0
        set -lx RPATH_WRAPPED 1
        command rpath --emit --shell fish | source
    else
        command rpath $argv
    end
end
{MARKER_END}"#
        ),
        ShellKind::Cmd => format!(
            r#"@echo off
{CMD_MARKER_START}
doskey rpath=if "$*" == "" (for /f "delims=" %%i in ('rpath.exe --emit --shell cmd') do @%%i) else rpath.exe $*
{CMD_MARKER_END}"#
        ),
    }
}

pub fn install_shell(shell: ShellKind, dry_run: bool) -> ShellResult<InstallReport> {
    if shell == ShellKind::Cmd {
        return install_cmd_wrapper(dry_run);
    }

    let target = profile_path(shell)?;
    let snippet = wrapper_snippet(shell);
    if dry_run {
        return Ok(InstallReport {
            shell,
            target: Some(target.to_string_lossy().to_string()),
            backup: None,
            changed: false,
            message: "dry run: profile would be updated".to_string(),
            snippet,
        });
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = fs::read_to_string(&target).unwrap_or_default();
    let backup = if target.exists() {
        let backup = target.with_extension(format!("bak.{}", unix_now()));
        fs::copy(&target, &backup)?;
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    };

    let updated = replace_marked_block(&existing, &snippet);
    let changed = existing != updated;
    fs::write(&target, updated)?;

    Ok(InstallReport {
        shell,
        target: Some(target.to_string_lossy().to_string()),
        backup,
        changed,
        message: if changed {
            "installed rpath shell wrapper".to_string()
        } else {
            "rpath shell wrapper was already installed".to_string()
        },
        snippet,
    })
}

pub fn uninstall_shell(shell: ShellKind, dry_run: bool) -> ShellResult<InstallReport> {
    if shell == ShellKind::Cmd {
        return uninstall_cmd_wrapper(dry_run);
    }

    let target = profile_path(shell)?;
    let snippet = wrapper_snippet(shell);
    if !target.exists() {
        return Ok(InstallReport {
            shell,
            target: Some(target.to_string_lossy().to_string()),
            backup: None,
            changed: false,
            message: "profile does not exist; nothing to uninstall".to_string(),
            snippet,
        });
    }

    let existing = fs::read_to_string(&target)?;
    let updated = remove_marked_block(&existing);
    let changed = existing != updated;
    let backup = if changed && !dry_run {
        let backup = target.with_extension(format!("bak.{}", unix_now()));
        fs::copy(&target, &backup)?;
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    };

    if changed && !dry_run {
        fs::write(&target, updated)?;
    }

    Ok(InstallReport {
        shell,
        target: Some(target.to_string_lossy().to_string()),
        backup,
        changed,
        message: if dry_run {
            "dry run: profile marker would be removed".to_string()
        } else if changed {
            "removed rpath shell wrapper".to_string()
        } else {
            "rpath shell wrapper was not installed".to_string()
        },
        snippet,
    })
}

pub fn init_snippet(shell: ShellKind) -> InstallReport {
    InstallReport {
        shell,
        target: profile_path(shell).ok().map(|path| path.to_string_lossy().to_string()),
        backup: None,
        changed: false,
        message: "copy this snippet into your shell profile".to_string(),
        snippet: wrapper_snippet(shell),
    }
}

fn emit_cmd_path(path: &str) -> String {
    format!("set \"PATH={}\"\r\n", path.replace('"', "\"\""))
}

fn emit_powershell_path(path: &str) -> String {
    format!("$env:Path = '{}'\n", path.replace('\'', "''"))
}

fn emit_posix_path(path: &str) -> String {
    format!("export PATH='{}'\n", escape_single_quoted(path))
}

fn emit_fish_path(path: &str) -> String {
    let entries = path
        .split(if cfg!(windows) { ';' } else { ':' })
        .filter(|entry| !entry.is_empty())
        .map(|entry| format!("'{}'", escape_single_quoted(entry)))
        .collect::<Vec<_>>()
        .join(" ");
    format!("set -gx PATH {entries}\n")
}

fn escape_single_quoted(value: &str) -> String {
    value.replace('\'', r#"'\''"#)
}

fn profile_path(shell: ShellKind) -> ShellResult<PathBuf> {
    let home = dirs::home_dir().ok_or(ShellError::MissingHome)?;
    match shell {
        ShellKind::PowerShell => Ok(home
            .join("Documents")
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1")),
        ShellKind::Pwsh => {
            Ok(home.join("Documents").join("PowerShell").join("Microsoft.PowerShell_profile.ps1"))
        }
        ShellKind::Bash => Ok(home.join(".bashrc")),
        ShellKind::Zsh => Ok(home.join(".zshrc")),
        ShellKind::Fish => Ok(home.join(".config").join("fish").join("config.fish")),
        ShellKind::Cmd => Err(ShellError::UnsupportedShell(shell)),
    }
}

fn replace_marked_block(existing: &str, snippet: &str) -> String {
    let without = remove_marked_block(existing);
    let mut output = without.trim_end().to_string();
    if !output.is_empty() {
        output.push_str("\n\n");
    }
    output.push_str(snippet);
    output.push('\n');
    output
}

fn remove_marked_block(existing: &str) -> String {
    let mut output = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        if line.trim() == MARKER_START {
            in_block = true;
            continue;
        }
        if line.trim() == MARKER_END {
            in_block = false;
            continue;
        }
        if !in_block {
            output.push(line);
        }
    }
    let mut joined = output.join("\n");
    if existing.ends_with('\n') && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

fn install_cmd_wrapper(dry_run: bool) -> ShellResult<InstallReport> {
    let snippet = wrapper_snippet(ShellKind::Cmd);
    let target = cmd_wrapper_path()?;
    if dry_run {
        return Ok(InstallReport {
            shell: ShellKind::Cmd,
            target: Some(target.to_string_lossy().to_string()),
            backup: None,
            changed: false,
            message: "dry run: cmd wrapper would be written and AutoRun would be configured"
                .to_string(),
            snippet,
        });
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup = if target.exists() {
        let backup = target.with_extension(format!("bak.{}", unix_now()));
        fs::copy(&target, &backup)?;
        Some(backup.to_string_lossy().to_string())
    } else {
        None
    };
    fs::write(&target, snippet.as_bytes())?;
    configure_cmd_autorun(&target);

    Ok(InstallReport {
        shell: ShellKind::Cmd,
        target: Some(target.to_string_lossy().to_string()),
        backup,
        changed: true,
        message: "installed cmd wrapper and attempted HKCU AutoRun registration".to_string(),
        snippet,
    })
}

fn uninstall_cmd_wrapper(dry_run: bool) -> ShellResult<InstallReport> {
    let snippet = wrapper_snippet(ShellKind::Cmd);
    let target = cmd_wrapper_path()?;
    let changed = target.exists();
    if changed && !dry_run {
        fs::remove_file(&target)?;
    }
    if !dry_run {
        remove_cmd_autorun();
    }

    Ok(InstallReport {
        shell: ShellKind::Cmd,
        target: Some(target.to_string_lossy().to_string()),
        backup: None,
        changed,
        message: if dry_run {
            "dry run: cmd wrapper and AutoRun registration would be removed".to_string()
        } else {
            "removed cmd wrapper and attempted AutoRun cleanup".to_string()
        },
        snippet,
    })
}

fn cmd_wrapper_path() -> ShellResult<PathBuf> {
    let base = dirs::data_dir().ok_or(ShellError::MissingHome)?;
    Ok(base.join("rpath").join("cmd-init.cmd"))
}

fn configure_cmd_autorun(path: &Path) {
    if !cfg!(windows) {
        return;
    }
    let segment = cmd_autorun_segment(path);
    let command = add_cmd_autorun_segment(&read_cmd_autorun().unwrap_or_default(), &segment);
    let _ = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Command Processor",
            "/v",
            "AutoRun",
            "/t",
            "REG_SZ",
            "/d",
            &command,
            "/f",
        ])
        .status();
}

fn remove_cmd_autorun() {
    if !cfg!(windows) {
        return;
    }
    let updated = remove_cmd_autorun_segment(&read_cmd_autorun().unwrap_or_default());
    if updated.trim().is_empty() {
        let _ = Command::new("reg")
            .args(["delete", r"HKCU\Software\Microsoft\Command Processor", "/v", "AutoRun", "/f"])
            .status();
    } else {
        let _ = Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Command Processor",
                "/v",
                "AutoRun",
                "/t",
                "REG_SZ",
                "/d",
                &updated,
                "/f",
            ])
            .status();
    }
}

fn cmd_autorun_segment(path: &Path) -> String {
    format!("if exist \"{}\" call \"{}\"", path.display(), path.display())
}

fn add_cmd_autorun_segment(existing: &str, segment: &str) -> String {
    let mut segments = cmd_autorun_segments_without_rpath(existing);
    segments.push(segment.trim().to_string());
    segments.join(" & ")
}

fn remove_cmd_autorun_segment(existing: &str) -> String {
    cmd_autorun_segments_without_rpath(existing).join(" & ")
}

fn cmd_autorun_segments_without_rpath(existing: &str) -> Vec<String> {
    existing
        .split(" & ")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .filter(|segment| {
            let lower = segment.to_ascii_lowercase();
            !(lower.contains("rpath") && lower.contains("cmd-init.cmd"))
        })
        .map(ToString::to_string)
        .collect()
}

fn read_cmd_autorun() -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let output = Command::new("reg")
        .args(["query", r"HKCU\Software\Microsoft\Command Processor", "/v", "AutoRun"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.to_ascii_lowercase().starts_with("autorun") {
            continue;
        }
        for value_type in ["REG_EXPAND_SZ", "REG_SZ"] {
            if let Some(index) = trimmed.find(value_type) {
                let value = trimmed[index + value_type.len()..].trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        add_cmd_autorun_segment, cmd_autorun_segment, emit_environment, remove_cmd_autorun_segment,
        wrapper_snippet,
    };
    use rpath_core::{
        build_environment_plan, model::BuildOptions, EnvironmentPlan, PathEntry, PathSource,
        PlanStats, ShellKind,
    };
    use std::collections::BTreeMap;

    #[test]
    fn emits_bash_export() {
        let plan = plan_with_path("/usr/bin:/bin");
        let emit = emit_environment(&plan, ShellKind::Bash).unwrap();
        assert_eq!(emit.commands, "export PATH='/usr/bin:/bin'\n");
    }

    #[test]
    fn emits_fish_list() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let plan = plan_with_path(&format!("/usr/bin{separator}/bin"));
        let emit = emit_environment(&plan, ShellKind::Fish).unwrap();
        assert_eq!(emit.commands, "set -gx PATH '/usr/bin' '/bin'\n");
    }

    #[test]
    fn wrapper_contains_marker() {
        assert!(wrapper_snippet(ShellKind::Bash).contains("rpath initialize"));
    }

    #[test]
    fn cmd_wrapper_is_quiet_and_cmd_safe() {
        let snippet = wrapper_snippet(ShellKind::Cmd);

        assert!(snippet.starts_with("@echo off"));
        assert!(!snippet.contains("# >>>"));
        assert!(snippet.contains("doskey rpath="));
        assert!(!snippet.to_ascii_lowercase().contains("rpath initialized"));
    }

    #[test]
    fn cmd_autorun_helpers_preserve_unrelated_commands() {
        let path = std::path::Path::new(r"C:\Users\anett\AppData\Roaming\rpath\cmd-init.cmd");
        let segment = cmd_autorun_segment(path);
        let existing = r#"echo hello & set FOO=bar"#;

        let added = add_cmd_autorun_segment(existing, &segment);
        assert!(added.contains("echo hello"));
        assert!(added.contains("set FOO=bar"));
        assert!(added.contains("cmd-init.cmd"));

        let replaced = add_cmd_autorun_segment(&added, &segment);
        assert_eq!(replaced.matches("cmd-init.cmd").count(), 2);

        let removed = remove_cmd_autorun_segment(&added);
        assert_eq!(removed, existing);
    }

    #[test]
    fn current_plan_can_emit_for_detected_shell() {
        let plan = build_environment_plan(&BuildOptions::default()).unwrap();
        let _ = emit_environment(&plan, plan.shell);
    }

    fn plan_with_path(path: &str) -> EnvironmentPlan {
        let mut variables = BTreeMap::new();
        variables.insert("PATH".to_string(), path.to_string());
        EnvironmentPlan {
            shell: ShellKind::Bash,
            platform: "test".to_string(),
            variables,
            path: path.to_string(),
            path_entries: path
                .split(':')
                .map(|entry| PathEntry {
                    raw: entry.to_string(),
                    expanded: entry.to_string(),
                    source: PathSource::Current,
                    exists: true,
                    valid: true,
                    critical: false,
                    duplicate: false,
                })
                .collect(),
            diagnostics: Vec::new(),
            stats: PlanStats {
                original_path_entries: 2,
                computed_path_entries: 2,
                duplicates_removed: 0,
                invalid_entries: 0,
                added_entries: 0,
                removed_entries: 0,
            },
            generated_at_unix: 0,
        }
    }
}
