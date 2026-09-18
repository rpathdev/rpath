use anyhow::{bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Cursor, Read},
    path::Path,
};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/rpathdev/rpath/releases/latest";

#[derive(Debug, Clone, Copy)]
pub struct UpgradeOptions {
    pub check_only: bool,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct UpgradeReport {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub upgraded: bool,
    pub dry_run: bool,
    pub check_only: bool,
    pub artifact: Option<String>,
    pub binary_path: Option<String>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlatformAsset {
    artifact: &'static str,
    archive_name: String,
    checksum_name: String,
    binary_name: &'static str,
    archive_kind: ArchiveKind,
}

pub fn run(options: UpgradeOptions) -> Result<UpgradeReport> {
    let platform = platform_asset_for(env::consts::OS, env::consts::ARCH)?;
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let Some(release) = fetch_latest_release()? else {
        return Ok(UpgradeReport {
            current_version: current_version.clone(),
            latest_version: current_version,
            update_available: false,
            upgraded: false,
            dry_run: options.dry_run,
            check_only: options.check_only,
            artifact: Some(platform.archive_name.to_string()),
            binary_path: env::current_exe().ok().map(|path| path.to_string_lossy().to_string()),
            message: "no GitHub Releases are published for rpath yet".to_string(),
        });
    };
    let latest_version = release.tag_name.trim_start_matches('v').to_string();
    let update_available = is_newer_version(&release.tag_name, &current_version)?;
    let binary_path = env::current_exe().ok().map(|path| path.to_string_lossy().to_string());

    if !update_available {
        return Ok(UpgradeReport {
            current_version,
            latest_version,
            update_available,
            upgraded: false,
            dry_run: options.dry_run,
            check_only: options.check_only,
            artifact: Some(platform.archive_name.to_string()),
            binary_path,
            message: "rpath is already up to date".to_string(),
        });
    }

    if options.check_only {
        return Ok(UpgradeReport {
            current_version,
            latest_version,
            update_available,
            upgraded: false,
            dry_run: options.dry_run,
            check_only: true,
            artifact: Some(platform.archive_name.to_string()),
            binary_path,
            message: format!("rpath {} is available", release.tag_name),
        });
    }

    if options.dry_run {
        return Ok(UpgradeReport {
            current_version,
            latest_version,
            update_available,
            upgraded: false,
            dry_run: true,
            check_only: false,
            artifact: Some(platform.archive_name.to_string()),
            binary_path,
            message: format!("dry run: would upgrade rpath to {}", release.tag_name),
        });
    }

    let archive_url = release_asset_url(&release, &platform.archive_name)?;
    let checksum_url = release_asset_url(&release, &platform.checksum_name)?;
    let archive = download_bytes(archive_url)
        .with_context(|| format!("failed to download {}", platform.archive_name))?;
    let checksum_text = download_text(checksum_url)
        .with_context(|| format!("failed to download {}", platform.checksum_name))?;
    verify_checksum(&archive, &checksum_text)?;
    let binary = extract_binary(&archive, &platform)?;
    let target = env::current_exe().context("failed to resolve current rpath executable")?;
    replace_current_exe(&target, platform.binary_name, &binary)?;

    Ok(UpgradeReport {
        current_version,
        latest_version,
        update_available,
        upgraded: true,
        dry_run: false,
        check_only: false,
        artifact: Some(platform.archive_name.to_string()),
        binary_path: Some(target.to_string_lossy().to_string()),
        message: format!("upgraded rpath to {}", release.tag_name),
    })
}

fn fetch_latest_release() -> Result<Option<GitHubRelease>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("rpath/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client.get(LATEST_RELEASE_URL).send()?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(response.error_for_status()?.json()?))
}

fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("rpath/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    Ok(client.get(url).send()?.error_for_status()?.bytes()?.to_vec())
}

fn download_text(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("rpath/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    Ok(client.get(url).send()?.error_for_status()?.text()?)
}

fn platform_asset_for(os: &str, arch: &str) -> Result<PlatformAsset> {
    let normalized_arch = match arch {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    };

    let (artifact, kind, binary) = match (os, normalized_arch) {
        ("linux", "x86_64") => ("rpath-linux-x86_64", ArchiveKind::TarGz, "rpath"),
        ("linux", "aarch64") => ("rpath-linux-aarch64", ArchiveKind::TarGz, "rpath"),
        ("macos", "x86_64") => ("rpath-macos-x86_64", ArchiveKind::TarGz, "rpath"),
        ("macos", "aarch64") => ("rpath-macos-aarch64", ArchiveKind::TarGz, "rpath"),
        ("windows", "x86_64") => ("rpath-windows-x86_64", ArchiveKind::Zip, "rpath.exe"),
        ("windows", "aarch64") => ("rpath-windows-aarch64", ArchiveKind::Zip, "rpath.exe"),
        _ => bail!("unsupported platform for rpath upgrade: {os}/{arch}"),
    };

    let archive_name = match kind {
        ArchiveKind::TarGz => format!("{artifact}.tar.gz"),
        ArchiveKind::Zip => format!("{artifact}.zip"),
    };
    let checksum_name = format!("{archive_name}.sha256");

    Ok(PlatformAsset {
        artifact,
        archive_name,
        checksum_name,
        binary_name: binary,
        archive_kind: kind,
    })
}

fn release_asset_url<'a>(release: &'a GitHubRelease, name: &str) -> Result<&'a str> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.as_str())
        .with_context(|| format!("release asset not found: {name}"))
}

fn is_newer_version(latest: &str, current: &str) -> Result<bool> {
    Ok(parse_version(latest)? > parse_version(current)?)
}

fn parse_version(input: &str) -> Result<Version> {
    Version::parse(input.trim().trim_start_matches('v'))
        .with_context(|| format!("invalid version: {input}"))
}

fn verify_checksum(bytes: &[u8], checksum_text: &str) -> Result<()> {
    let expected = parse_sha256(checksum_text)?;
    let actual = Sha256::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect::<String>();
    if actual != expected {
        bail!("checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn parse_sha256(input: &str) -> Result<String> {
    input
        .split_whitespace()
        .find(|part| part.len() == 64 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(|part| part.to_ascii_lowercase())
        .context("sha256 file did not contain a valid checksum")
}

fn extract_binary(bytes: &[u8], platform: &PlatformAsset) -> Result<Vec<u8>> {
    match platform.archive_kind {
        ArchiveKind::TarGz => extract_tar_gz_binary(bytes, platform.binary_name),
        ArchiveKind::Zip => extract_zip_binary(bytes, platform.binary_name),
    }
}

fn extract_tar_gz_binary(bytes: &[u8], binary_name: &str) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!("archive did not contain {binary_name}")
}

fn extract_zip_binary(bytes: &[u8], binary_name: &str) -> Result<Vec<u8>> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if Path::new(file.name()).file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut binary = Vec::new();
            file.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!("archive did not contain {binary_name}")
}

fn replace_current_exe(target: &Path, binary_name: &str, bytes: &[u8]) -> Result<()> {
    if cfg!(windows) {
        schedule_windows_replacement(target, binary_name, bytes)
    } else {
        replace_unix_executable(target, bytes)
    }
}

#[cfg(not(windows))]
fn replace_unix_executable(target: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let temp_path = temp_path_next_to(target)?;
    fs::write(&temp_path, bytes)?;
    fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))?;
    fs::rename(&temp_path, target).with_context(|| {
        format!("failed to replace {} with {}", target.display(), temp_path.display())
    })?;
    Ok(())
}

#[cfg(windows)]
fn replace_unix_executable(_target: &Path, _bytes: &[u8]) -> Result<()> {
    unreachable!("unix replacement is not used on Windows")
}

#[cfg(windows)]
fn schedule_windows_replacement(target: &Path, binary_name: &str, bytes: &[u8]) -> Result<()> {
    let temp_root = env::temp_dir().join(format!("rpath-upgrade-{}", std::process::id()));
    fs::create_dir_all(&temp_root)?;
    let source = temp_root.join(binary_name);
    let helper = temp_root.join("replace-rpath.ps1");
    fs::write(&source, bytes)?;
    fs::write(&helper, windows_helper_script())?;
    std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&helper)
        .arg("-Source")
        .arg(&source)
        .arg("-Target")
        .arg(target)
        .arg("-Pid")
        .arg(std::process::id().to_string())
        .spawn()
        .context("failed to schedule Windows executable replacement")?;
    Ok(())
}

#[cfg(not(windows))]
fn schedule_windows_replacement(_target: &Path, _binary_name: &str, _bytes: &[u8]) -> Result<()> {
    unreachable!("Windows replacement is not used on Unix")
}

#[cfg(windows)]
fn windows_helper_script() -> &'static str {
    r#"
param(
  [Parameter(Mandatory = $true)][string]$Source,
  [Parameter(Mandatory = $true)][string]$Target,
  [Parameter(Mandatory = $true)][int]$Pid
)
$ErrorActionPreference = "Stop"
try {
  Wait-Process -Id $Pid -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 300
  Copy-Item -LiteralPath $Source -Destination $Target -Force
} finally {
  Remove-Item -LiteralPath $Source -Force -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue
}
"#
}

#[cfg(not(windows))]
fn temp_path_next_to(target: &Path) -> Result<std::path::PathBuf> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("current executable has no file name")?;
    let parent = target.parent().context("current executable has no parent directory")?;
    Ok(parent.join(format!(".{file_name}.upgrade-{}", std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::{
        is_newer_version, parse_sha256, platform_asset_for, release_asset_url, ArchiveKind,
        GitHubRelease,
    };

    #[test]
    fn maps_release_artifacts() {
        let linux = platform_asset_for("linux", "x86_64").unwrap();
        assert_eq!(linux.artifact, "rpath-linux-x86_64");
        assert_eq!(linux.archive_name, "rpath-linux-x86_64.tar.gz");
        assert_eq!(linux.binary_name, "rpath");
        assert_eq!(linux.archive_kind, ArchiveKind::TarGz);

        let windows = platform_asset_for("windows", "arm64").unwrap();
        assert_eq!(windows.artifact, "rpath-windows-aarch64");
        assert_eq!(windows.archive_name, "rpath-windows-aarch64.zip");
        assert_eq!(windows.binary_name, "rpath.exe");
        assert_eq!(windows.archive_kind, ArchiveKind::Zip);
    }

    #[test]
    fn rejects_unsupported_artifacts() {
        assert!(platform_asset_for("freebsd", "x86_64").is_err());
        assert!(platform_asset_for("linux", "riscv64").is_err());
    }

    #[test]
    fn compares_versions_with_v_prefix() {
        assert!(is_newer_version("v0.2.0", "0.1.1").unwrap());
        assert!(!is_newer_version("v0.1.1", "0.1.1").unwrap());
        assert!(!is_newer_version("v0.0.9", "0.1.1").unwrap());
    }

    #[test]
    fn parses_sha256_files() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(parse_sha256(&format!("{hash}  dist/rpath-linux-x86_64.tar.gz")).unwrap(), hash);
    }

    #[test]
    fn parses_release_json_and_finds_assets() {
        let release: GitHubRelease = serde_json::from_str(
            r#"{
                "tag_name": "v0.2.0",
                "assets": [
                    {
                        "name": "rpath-linux-x86_64.tar.gz",
                        "browser_download_url": "https://example.com/rpath-linux-x86_64.tar.gz"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            release_asset_url(&release, "rpath-linux-x86_64.tar.gz").unwrap(),
            "https://example.com/rpath-linux-x86_64.tar.gz"
        );
        assert!(release_asset_url(&release, "missing.tar.gz").is_err());
    }
}
