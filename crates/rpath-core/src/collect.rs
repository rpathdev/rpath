use crate::model::{
    BuildOptions, Diagnostic, DiagnosticSeverity, EnvironmentPlan, PathEntry, PathSource,
    PlanStats, RpathResult, ShellKind,
};
use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone)]
struct CandidatePath {
    raw: String,
    source: PathSource,
}

pub fn build_environment_plan(options: &BuildOptions) -> RpathResult<EnvironmentPlan> {
    let shell = options.shell.unwrap_or_else(detect_shell);
    let mut diagnostics = Vec::new();
    let mut variables = collect_current_variables();
    let original = split_path_value(&current_path_value(&variables));
    let mut candidates = Vec::new();

    candidates.extend(platform_path_candidates(&variables, &mut diagnostics));
    candidates.extend(package_manager_path_candidates(&variables));
    candidates.extend(shell_profile_candidates(shell, &variables, &mut diagnostics));
    candidates.extend(
        original.iter().cloned().map(|raw| CandidatePath { raw, source: PathSource::Current }),
    );
    candidates.extend(critical_path_candidates(&original));

    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut duplicates_removed = 0usize;
    let mut invalid_entries = 0usize;

    for candidate in candidates {
        let expanded = expand_variables(&candidate.raw, &variables);
        let key = path_key(&expanded);
        let empty = expanded.trim().is_empty();
        let malformed = has_unresolved_variable(&expanded);
        let exists = !empty && !malformed && Path::new(&expanded).exists();
        let critical = is_critical_path(&expanded);
        let valid = !empty && !malformed && (!options.remove_invalid || exists || critical);
        let duplicate = !key.is_empty() && seen.contains(&key);

        if empty || malformed || (options.remove_invalid && !exists && !critical) {
            invalid_entries += 1;
            diagnostics.push(
                Diagnostic::warning("invalid-path", "PATH entry is empty, unresolved, or missing")
                    .with_path(candidate.raw.clone()),
            );
        } else if options.validate_paths && !exists {
            invalid_entries += 1;
            let severity = if options.strict {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Warning
            };
            diagnostics.push(Diagnostic {
                severity,
                code: "missing-path".to_string(),
                message: "PATH entry does not exist on disk".to_string(),
                path: Some(expanded.clone()),
            });
        }

        if duplicate && !options.no_dedupe {
            duplicates_removed += 1;
            continue;
        }
        if !valid {
            continue;
        }
        if !key.is_empty() {
            seen.insert(key);
        }

        entries.push(PathEntry {
            raw: candidate.raw,
            expanded,
            source: candidate.source,
            exists,
            valid,
            critical,
            duplicate,
        });
    }

    if entries.is_empty() {
        diagnostics.push(Diagnostic::error(
            "empty-path-plan",
            "rpath refused to produce an empty PATH plan",
        ));
        entries.extend(original.iter().cloned().map(|raw| {
            let expanded = expand_variables(&raw, &variables);
            PathEntry {
                raw,
                exists: Path::new(&expanded).exists(),
                valid: true,
                critical: is_critical_path(&expanded),
                duplicate: false,
                expanded,
                source: PathSource::Current,
            }
        }));
    }

    let separator = if cfg!(windows) { ";" } else { ":" };
    let path =
        entries.iter().map(|entry| entry.expanded.as_str()).collect::<Vec<_>>().join(separator);
    let path_key_name = path_variable_name(&variables).to_string();
    variables.insert(path_key_name, path.clone());

    let original_keys = original.iter().map(|value| path_key(value)).collect::<HashSet<_>>();
    let computed_keys =
        entries.iter().map(|entry| path_key(&entry.expanded)).collect::<HashSet<_>>();
    let added_entries = computed_keys.difference(&original_keys).count();
    let removed_entries = original_keys.difference(&computed_keys).count();

    Ok(EnvironmentPlan {
        shell,
        platform: platform_name().to_string(),
        variables,
        path,
        path_entries: entries,
        diagnostics,
        stats: PlanStats {
            original_path_entries: original.len(),
            computed_path_entries: computed_keys.len(),
            duplicates_removed,
            invalid_entries,
            added_entries,
            removed_entries,
        },
        generated_at_unix: unix_now(),
    })
}

pub fn detect_shell() -> ShellKind {
    if cfg!(windows) {
        if let Some(shell) = detect_parent_process_shell() {
            return shell;
        }
        return detect_windows_shell_from_env();
    }

    let shell = env::var("SHELL")
        .ok()
        .and_then(|value| {
            Path::new(&value)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.to_ascii_lowercase())
        })
        .unwrap_or_default();

    match shell.as_str() {
        "fish" => ShellKind::Fish,
        "zsh" => ShellKind::Zsh,
        _ => ShellKind::Bash,
    }
}

pub fn shell_from_process_name(name: &str) -> Option<ShellKind> {
    let file_name = name
        .trim_matches(['"', '\''])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    match file_name.as_str() {
        "cmd" | "cmd.exe" => Some(ShellKind::Cmd),
        "powershell" | "powershell.exe" => Some(ShellKind::PowerShell),
        "pwsh" | "pwsh.exe" => Some(ShellKind::Pwsh),
        "bash" | "bash.exe" | "sh" | "sh.exe" => Some(ShellKind::Bash),
        "zsh" | "zsh.exe" => Some(ShellKind::Zsh),
        "fish" | "fish.exe" => Some(ShellKind::Fish),
        _ => None,
    }
}

#[cfg(windows)]
fn detect_parent_process_shell() -> Option<ShellKind> {
    parent_process_name().and_then(|name| shell_from_process_name(&name))
}

#[cfg(not(windows))]
fn detect_parent_process_shell() -> Option<ShellKind> {
    None
}

#[cfg(windows)]
fn parent_process_name() -> Option<String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }

        let mut entry = zeroed::<PROCESSENTRY32W>();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let current_pid = std::process::id();
        let mut parent_pid = None;

        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == current_pid {
                    parent_pid = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        let mut parent_name = None;
        if let Some(parent_pid) = parent_pid {
            entry = zeroed::<PROCESSENTRY32W>();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    if entry.th32ProcessID == parent_pid {
                        parent_name = Some(process_entry_name(&entry));
                        break;
                    }
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
        }

        CloseHandle(snapshot);
        parent_name
    }
}

#[cfg(windows)]
fn process_entry_name(
    entry: &windows_sys::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W,
) -> String {
    let end = entry.szExeFile.iter().position(|value| *value == 0).unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..end])
}

fn detect_windows_shell_from_env() -> ShellKind {
    if env::var_os("PWSH_VERSION").is_some() {
        ShellKind::Pwsh
    } else if env::var_os("ComSpec").is_some() {
        ShellKind::Cmd
    } else {
        ShellKind::PowerShell
    }
}

fn collect_current_variables() -> BTreeMap<String, String> {
    env::vars().collect()
}

fn current_path_value(variables: &BTreeMap<String, String>) -> String {
    variables
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

fn path_variable_name(variables: &BTreeMap<String, String>) -> &str {
    variables
        .keys()
        .find(|key| key.eq_ignore_ascii_case("PATH"))
        .map(String::as_str)
        .unwrap_or(if cfg!(windows) { "Path" } else { "PATH" })
}

fn split_path_value(value: &str) -> Vec<String> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    value
        .split(separator)
        .map(|part| part.trim_matches('"').trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn platform_path_candidates(
    variables: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<CandidatePath> {
    if cfg!(windows) {
        windows_registry_path_candidates(diagnostics)
    } else {
        unix_path_candidates(variables, diagnostics)
    }
}

#[cfg(windows)]
fn windows_registry_path_candidates(diagnostics: &mut Vec<Diagnostic>) -> Vec<CandidatePath> {
    let mut candidates = Vec::new();
    let machine = r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment".to_string();
    let user = r"HKCU\Environment".to_string();

    for (key, source) in [(machine, PathSource::RegistryMachine), (user, PathSource::RegistryUser)]
    {
        match query_windows_registry_value(&key, "Path") {
            Ok(Some(value)) => candidates.extend(
                split_path_value(&value)
                    .into_iter()
                    .map(|raw| CandidatePath { raw, source: source.clone() }),
            ),
            Ok(None) => {}
            Err(error) => diagnostics.push(Diagnostic::warning(
                "registry-read-failed",
                format!("could not read Windows registry PATH from {key}: {error}"),
            )),
        }
    }

    candidates
}

#[cfg(not(windows))]
fn windows_registry_path_candidates(_diagnostics: &mut Vec<Diagnostic>) -> Vec<CandidatePath> {
    Vec::new()
}

#[cfg(windows)]
fn query_windows_registry_value(key: &str, value: &str) -> std::io::Result<Option<String>> {
    let output = Command::new("reg")
        .args(["query", key, "/v", value])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.to_ascii_lowercase().starts_with(&value.to_ascii_lowercase()) {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let _name = parts.next();
        let _kind = parts.next();
        let remainder = parts.collect::<Vec<_>>().join(" ");
        if !remainder.is_empty() {
            return Ok(Some(remainder));
        }
    }
    Ok(None)
}

fn unix_path_candidates(
    variables: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<CandidatePath> {
    let mut candidates = Vec::new();
    let mut files = vec![PathBuf::from("/etc/environment"), PathBuf::from("/etc/profile")];

    if let Some(home) = dirs::home_dir() {
        files.extend([
            home.join(".profile"),
            home.join(".bashrc"),
            home.join(".zshrc"),
            home.join(".config/fish/config.fish"),
        ]);
    }

    for file in files {
        if !file.exists() {
            continue;
        }
        match fs::read_to_string(&file) {
            Ok(raw) => {
                let source = PathSource::File(file.to_string_lossy().to_string());
                candidates.extend(
                    parse_path_assignments(&raw, variables)
                        .into_iter()
                        .map(|raw| CandidatePath { raw, source: source.clone() }),
                );
            }
            Err(error) => diagnostics.push(Diagnostic::warning(
                "profile-read-failed",
                format!("could not read {}: {error}", file.display()),
            )),
        }
    }

    candidates
}

fn shell_profile_candidates(
    shell: ShellKind,
    _variables: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<CandidatePath> {
    if !shell.is_posix_shell() || cfg!(windows) {
        return Vec::new();
    }

    let executable = shell.as_str();
    if !command_exists(executable) {
        return Vec::new();
    }

    match capture_shell_environment(executable, Duration::from_millis(700)) {
        Ok(snapshot) => snapshot
            .get("PATH")
            .map(|value| {
                split_path_value(value)
                    .into_iter()
                    .map(|raw| CandidatePath {
                        raw,
                        source: PathSource::ShellProfile(shell.as_str().to_string()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(error) => {
            diagnostics.push(Diagnostic::warning(
                "profile-source-failed",
                format!("sandboxed {shell} profile sourcing failed: {error}"),
            ));
            Vec::new()
        }
    }
}

fn parse_path_assignments(raw: &str, variables: &BTreeMap<String, String>) -> Vec<String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line).trim();
            let line = line.strip_prefix("set -gx ").unwrap_or(line).trim();
            let (name, value) = line.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case("PATH") {
                return None;
            }
            Some(clean_assignment_value(value, variables))
        })
        .flat_map(|value| split_path_value(&value))
        .collect()
}

fn clean_assignment_value(value: &str, variables: &BTreeMap<String, String>) -> String {
    let mut value =
        value.trim().trim_end_matches(';').trim_matches('"').trim_matches('\'').to_string();
    if value.contains("$PATH") {
        value = value.replace("$PATH", &current_path_value(variables));
    }
    if value.contains("%PATH%") {
        value = value.replace("%PATH%", &current_path_value(variables));
    }
    expand_variables(&value, variables)
}

fn package_manager_path_candidates(variables: &BTreeMap<String, String>) -> Vec<CandidatePath> {
    let mut paths = Vec::new();

    if cfg!(windows) {
        for path in [
            r"C:\Program Files\Git\cmd",
            r"C:\Program Files\Git\usr\bin",
            r"C:\msys64\usr\bin",
            r"C:\msys64\mingw64\bin",
        ] {
            paths.push((path.to_string(), "windows-tools".to_string()));
        }
    } else {
        paths.extend([
            ("/opt/homebrew/bin".to_string(), "homebrew".to_string()),
            ("/usr/local/bin".to_string(), "homebrew".to_string()),
            ("/snap/bin".to_string(), "snap".to_string()),
            ("/var/lib/flatpak/exports/bin".to_string(), "flatpak".to_string()),
            ("/nix/var/nix/profiles/default/bin".to_string(), "nix".to_string()),
        ]);
        if let Some(home) = dirs::home_dir() {
            paths.extend([
                (home.join(".nix-profile/bin").to_string_lossy().to_string(), "nix".to_string()),
                (
                    home.join(".local/share/flatpak/exports/bin").to_string_lossy().to_string(),
                    "flatpak".to_string(),
                ),
                (home.join(".local/bin").to_string_lossy().to_string(), "user".to_string()),
            ]);
        }
    }

    paths
        .into_iter()
        .filter_map(|(raw, source)| {
            let raw = expand_variables(&raw, variables);
            Path::new(&raw)
                .exists()
                .then_some(CandidatePath { raw, source: PathSource::PackageManager(source) })
        })
        .collect()
}

fn critical_path_candidates(original: &[String]) -> Vec<CandidatePath> {
    original
        .iter()
        .filter(|value| is_critical_path(value))
        .map(|raw| CandidatePath { raw: raw.clone(), source: PathSource::System })
        .collect()
}

fn is_critical_path(value: &str) -> bool {
    let normalized = path_key(value);
    if cfg!(windows) {
        [r"c:\windows", r"c:\windows\system32", r"%systemroot%\system32", r"%systemroot%"]
            .iter()
            .any(|critical| normalized == *critical)
    } else {
        ["/bin", "/usr/bin", "/usr/local/bin", "/sbin", "/usr/sbin"]
            .iter()
            .any(|critical| normalized == *critical)
    }
}

pub fn expand_variables(value: &str, variables: &BTreeMap<String, String>) -> String {
    if cfg!(windows) {
        expand_windows_variables(value, variables)
    } else {
        expand_unix_variables(value, variables)
    }
}

fn expand_windows_variables(value: &str, variables: &BTreeMap<String, String>) -> String {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        let mut name = String::new();
        while let Some(next) = chars.peek().copied() {
            chars.next();
            if next == '%' {
                break;
            }
            name.push(next);
        }
        if name.is_empty() {
            output.push('%');
        } else if let Some(value) = get_case_insensitive(variables, &name) {
            output.push_str(value);
        } else {
            output.push('%');
            output.push_str(&name);
            output.push('%');
        }
    }
    output
}

fn expand_unix_variables(value: &str, variables: &BTreeMap<String, String>) -> String {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            output.push(ch);
            continue;
        }

        if chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                name.push(next);
            }
            if let Some(value) = variables.get(&name) {
                output.push_str(value);
            } else {
                output.push_str("${");
                output.push_str(&name);
                output.push('}');
            }
            continue;
        }

        let mut name = String::new();
        while let Some(next) = chars.peek().copied() {
            if !(next == '_' || next.is_ascii_alphanumeric()) {
                break;
            }
            chars.next();
            name.push(next);
        }
        if name.is_empty() {
            output.push('$');
        } else if let Some(value) = variables.get(&name) {
            output.push_str(value);
        } else {
            output.push('$');
            output.push_str(&name);
        }
    }
    output
}

fn has_unresolved_variable(value: &str) -> bool {
    if cfg!(windows) {
        let bytes = value.as_bytes();
        bytes.iter().filter(|byte| **byte == b'%').count() >= 2
    } else {
        value.contains("${") || value.contains('$')
    }
}

fn get_case_insensitive<'a>(
    variables: &'a BTreeMap<String, String>,
    name: &str,
) -> Option<&'a String> {
    variables.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value)
}

fn path_key(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"').trim_matches('\'');
    if cfg!(windows) {
        trimmed.trim_end_matches(['\\', '/']).to_ascii_lowercase()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

fn command_exists(executable: &str) -> bool {
    env::var_os("PATH")
        .map(|path| {
            env::split_paths(&path).any(|dir| {
                let candidate = dir.join(executable);
                candidate.exists() || candidate.with_extension("exe").exists()
            })
        })
        .unwrap_or(false)
}

fn capture_shell_environment(
    executable: &str,
    timeout: Duration,
) -> std::io::Result<BTreeMap<String, String>> {
    let mut child = Command::new(executable)
        .args(["-lc", "env -0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            let mut stdout = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                pipe.read_to_end(&mut stdout)?;
            }
            let mut env = BTreeMap::new();
            for entry in stdout.split(|byte| *byte == 0) {
                if entry.is_empty() {
                    continue;
                }
                if let Some(index) = entry.iter().position(|byte| *byte == b'=') {
                    let key = String::from_utf8_lossy(&entry[..index]).to_string();
                    let value = String::from_utf8_lossy(&entry[index + 1..]).to_string();
                    env.insert(key, value);
                }
            }
            return Ok(env);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "shell profile collection timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::{expand_variables, parse_path_assignments, shell_from_process_name};
    use crate::ShellKind;
    use std::collections::BTreeMap;

    #[test]
    fn expands_unix_style_variables() {
        let mut variables = BTreeMap::new();
        variables.insert("HOME".to_string(), "/home/test".to_string());

        let expanded = expand_variables("$HOME/bin:${HOME}/.cargo/bin", &variables);

        if cfg!(windows) {
            assert_eq!(expanded, "$HOME/bin:${HOME}/.cargo/bin");
        } else {
            assert_eq!(expanded, "/home/test/bin:/home/test/.cargo/bin");
        }
    }

    #[test]
    fn parses_simple_path_assignments() {
        let mut variables = BTreeMap::new();
        variables.insert("PATH".to_string(), "/bin".to_string());
        variables.insert("HOME".to_string(), "/home/test".to_string());

        let entries = parse_path_assignments(
            r#"
            # ignored
            export PATH="$PATH:$HOME/bin"
            "#,
            &variables,
        );

        if cfg!(windows) {
            assert!(entries.is_empty() || entries.iter().any(|entry| entry.contains("$HOME")));
        } else {
            assert!(entries.contains(&"/bin".to_string()));
            assert!(entries.contains(&"/home/test/bin".to_string()));
        }
    }

    #[test]
    fn maps_parent_process_names_to_shells() {
        assert_eq!(shell_from_process_name("cmd.exe"), Some(ShellKind::Cmd));
        assert_eq!(shell_from_process_name("powershell.exe"), Some(ShellKind::PowerShell));
        assert_eq!(shell_from_process_name("pwsh.exe"), Some(ShellKind::Pwsh));
        assert_eq!(
            shell_from_process_name(r"C:\Program Files\Git\usr\bin\bash.exe"),
            Some(ShellKind::Bash)
        );
        assert_eq!(shell_from_process_name("zsh"), Some(ShellKind::Zsh));
        assert_eq!(shell_from_process_name("fish.exe"), Some(ShellKind::Fish));
        assert_eq!(shell_from_process_name("explorer.exe"), None);
    }
}
