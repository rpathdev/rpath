mod upgrade;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rpath_core::{
    build_environment_plan, delete_snapshot, diff_path_entries, diff_plan_against_current,
    list_snapshots, list_versions, load_snapshot, save_snapshot, save_version, snapshot_from_plan,
    state_dir, BuildOptions, DiagnosticSeverity, EnvironmentPlan, EnvironmentSnapshot, ShellKind,
};
use rpath_integrations::{
    install_watch_service, run_integration, uninstall_watch_service, watch_service_status,
    IntegrationAction, IntegrationTarget,
};
use rpath_shell::{emit_environment, emit_snapshot, init_snippet, install_shell, uninstall_shell};
use serde::Serialize;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Parser)]
#[command(name = "rpath")]
#[command(version, about = "Refresh shell PATH and environment state without restarting.")]
struct Cli {
    #[arg(long, value_enum, global = true)]
    shell: Option<CliShell>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    dry_run: bool,
    #[arg(long, global = true)]
    no_dedupe: bool,
    #[arg(long, global = true)]
    strict: bool,
    #[arg(long, global = true)]
    emit: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, ValueEnum)]
enum CliShell {
    Cmd,
    Powershell,
    Pwsh,
    Bash,
    Zsh,
    Fish,
}

impl From<CliShell> for ShellKind {
    fn from(value: CliShell) -> Self {
        match value {
            CliShell::Cmd => Self::Cmd,
            CliShell::Powershell => Self::PowerShell,
            CliShell::Pwsh => Self::Pwsh,
            CliShell::Bash => Self::Bash,
            CliShell::Zsh => Self::Zsh,
            CliShell::Fish => Self::Fish,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Diagnose invalid, duplicate, missing, and risky PATH entries.
    Doctor(DoctorArgs),
    /// Show added, removed, and reordered PATH entries.
    Diff,
    /// Save, list, restore, or delete environment snapshots.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// List, diff, or restore automatically tracked environment versions.
    Version {
        #[command(subcommand)]
        command: VersionCommand,
    },
    /// Print the computed PATH only.
    Print,
    /// Build a repaired PATH plan with invalid entries removed.
    Repair,
    /// Check for and install a newer rpath release.
    Upgrade(UpgradeArgs),
    /// Install shell wrappers so bare `rpath` applies to the current session.
    Install(InstallArgs),
    /// Remove installed shell wrappers.
    Uninstall(InstallArgs),
    /// Print the shell initialization snippet.
    Init,
    /// Watch for environment changes and save versions when PATH changes.
    Watch(WatchArgs),
    /// Install, uninstall, or inspect local integrations.
    Integrate {
        #[arg(value_enum)]
        target: CliIntegrationTarget,
        #[arg(value_enum)]
        action: CliIntegrationAction,
    },
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Include simple security checks such as world-writable Unix PATH entries.
    #[arg(long)]
    security: bool,
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Install or uninstall all shell wrappers that are relevant to this OS.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Args)]
struct UpgradeArgs {
    /// Check whether an update is available without installing it.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Args)]
struct WatchArgs {
    /// Check once, save a version if needed, and exit.
    #[arg(long)]
    once: bool,
    /// Poll interval in seconds for foreground watch mode.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Install the user watch service or scheduled task.
    #[arg(long)]
    install_service: bool,
    /// Uninstall the user watch service or scheduled task.
    #[arg(long)]
    uninstall_service: bool,
    /// Show watch service status.
    #[arg(long)]
    status: bool,
}

#[derive(Debug, Subcommand)]
enum SnapshotCommand {
    Save {
        /// Optional human-readable reason.
        reason: Option<String>,
    },
    List,
    Restore {
        /// Snapshot id. If omitted, the most recent snapshot is used.
        id: Option<String>,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum VersionCommand {
    List,
    Diff { from: String, to: String },
    Restore { id: String },
}

#[derive(Debug, Clone, ValueEnum)]
enum CliIntegrationTarget {
    Vscode,
    Explorer,
    Wsl,
    GitBash,
}

impl From<CliIntegrationTarget> for IntegrationTarget {
    fn from(value: CliIntegrationTarget) -> Self {
        match value {
            CliIntegrationTarget::Vscode => Self::Vscode,
            CliIntegrationTarget::Explorer => Self::Explorer,
            CliIntegrationTarget::Wsl => Self::Wsl,
            CliIntegrationTarget::GitBash => Self::GitBash,
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum CliIntegrationAction {
    Install,
    Uninstall,
    Status,
}

impl From<CliIntegrationAction> for IntegrationAction {
    fn from(value: CliIntegrationAction) -> Self {
        match value {
            CliIntegrationAction::Install => Self::Install,
            CliIntegrationAction::Uninstall => Self::Uninstall,
            CliIntegrationAction::Status => Self::Status,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        None => default_refresh(&cli),
        Some(Command::Doctor(args)) => doctor(&cli, args),
        Some(Command::Diff) => diff(&cli),
        Some(Command::Snapshot { command }) => snapshot(&cli, command),
        Some(Command::Version { command }) => version(&cli, command),
        Some(Command::Print) => print_path(&cli),
        Some(Command::Repair) => repair(&cli),
        Some(Command::Upgrade(args)) => upgrade(&cli, args),
        Some(Command::Install(args)) => install(&cli, args),
        Some(Command::Uninstall(args)) => uninstall(&cli, args),
        Some(Command::Init) => init(&cli),
        Some(Command::Watch(args)) => watch(&cli, args),
        Some(Command::Integrate { target, action }) => integrate(&cli, target, action),
    }
}

fn default_refresh(cli: &Cli) -> Result<()> {
    let plan = build_plan(cli, false)?;
    if !cli.dry_run {
        let _ = save_version(&plan);
    }

    if cli.emit {
        let emit = emit_environment(&plan, plan.shell)?;
        print!("{}", emit.commands);
        return Ok(());
    }

    if cli.json {
        print_json(&plan)
    } else {
        println!("Environment plan ready");
        println!(
            "PATH entries: {} -> {}",
            plan.stats.original_path_entries, plan.stats.computed_path_entries
        );
        println!("Duplicates removed: {}", plan.stats.duplicates_removed);
        if plan.stats.invalid_entries > 0 {
            println!("Diagnostics: {} issue(s)", plan.stats.invalid_entries);
        }
        if std::env::var_os("RPATH_WRAPPED").is_none() {
            println!(
                "Printed a plan only. Installed shell wrappers apply changes silently; direct binary runs can use `rpath --emit`."
            );
        }
        Ok(())
    }
}

fn doctor(cli: &Cli, args: &DoctorArgs) -> Result<()> {
    let plan = build_plan(cli, false)?;
    let mut diagnostics = plan.diagnostics.clone();
    diagnostics.extend(duplicate_diagnostics(&plan));
    if args.security {
        diagnostics.extend(security_diagnostics(&plan));
    }

    if cli.json {
        print_json(&diagnostics)
    } else if diagnostics.is_empty() {
        println!("No PATH problems found");
        Ok(())
    } else {
        for diagnostic in diagnostics {
            let path = diagnostic.path.map(|path| format!(" ({path})")).unwrap_or_default();
            println!(
                "[{}] {}: {}{}",
                severity_label(&diagnostic.severity),
                diagnostic.code,
                diagnostic.message,
                path
            );
        }
        Ok(())
    }
}

fn diff(cli: &Cli) -> Result<()> {
    let plan = build_plan(cli, false)?;
    let diff = diff_plan_against_current(&plan);
    if cli.json {
        print_json(&diff)
    } else {
        print_list("Added", &diff.added);
        print_list("Removed", &diff.removed);
        print_list("Reordered", &diff.reordered);
        println!("Unchanged: {}", diff.unchanged_count);
        Ok(())
    }
}

fn snapshot(cli: &Cli, command: &SnapshotCommand) -> Result<()> {
    match command {
        SnapshotCommand::Save { reason } => {
            let plan = build_plan(cli, false)?;
            let snapshot = snapshot_from_plan(
                &plan,
                reason.clone().unwrap_or_else(|| "manual snapshot".to_string()),
            );
            let path = if cli.dry_run { None } else { Some(save_snapshot(&snapshot)?) };
            if cli.json {
                print_json(&snapshot)
            } else {
                println!("Saved snapshot {}", snapshot.id);
                if let Some(path) = path {
                    println!("{}", path.display());
                }
                Ok(())
            }
        }
        SnapshotCommand::List => list_snapshot_like(cli, list_snapshots()?),
        SnapshotCommand::Restore { id } => {
            let snapshot = load_or_latest_snapshot(id.as_deref())?;
            restore_snapshot(cli, &snapshot)
        }
        SnapshotCommand::Delete { id } => {
            if !cli.dry_run {
                delete_snapshot(id)?;
            }
            println!("Deleted snapshot {id}");
            Ok(())
        }
    }
}

fn version(cli: &Cli, command: &VersionCommand) -> Result<()> {
    match command {
        VersionCommand::List => list_snapshot_like(cli, list_versions()?),
        VersionCommand::Diff { from, to } => {
            let versions = list_versions()?;
            let left = versions
                .iter()
                .find(|snapshot| snapshot.id == *from)
                .with_context(|| format!("version not found: {from}"))?;
            let right = versions
                .iter()
                .find(|snapshot| snapshot.id == *to)
                .with_context(|| format!("version not found: {to}"))?;
            let diff = diff_path_entries(&split_snapshot_path(left), &split_snapshot_path(right));
            if cli.json {
                print_json(&diff)
            } else {
                print_list("Added", &diff.added);
                print_list("Removed", &diff.removed);
                print_list("Reordered", &diff.reordered);
                println!("Unchanged: {}", diff.unchanged_count);
                Ok(())
            }
        }
        VersionCommand::Restore { id } => {
            let snapshot = list_versions()?
                .into_iter()
                .find(|snapshot| snapshot.id == *id)
                .with_context(|| format!("version not found: {id}"))?;
            restore_snapshot(cli, &snapshot)
        }
    }
}

fn print_path(cli: &Cli) -> Result<()> {
    let plan = build_plan(cli, false)?;
    if cli.json {
        print_json(&serde_json::json!({ "path": plan.path }))
    } else {
        println!("{}", plan.path);
        Ok(())
    }
}

fn repair(cli: &Cli) -> Result<()> {
    let plan = build_plan(cli, true)?;
    if cli.emit {
        let emit = emit_environment(&plan, plan.shell)?;
        print!("{}", emit.commands);
    } else if cli.json {
        print_json(&plan)?;
    } else {
        println!("Repair plan ready");
        println!("Invalid entries removed: {}", plan.stats.invalid_entries);
        println!("PATH entries: {}", plan.stats.computed_path_entries);
        println!("Use `rpath repair --emit` through your shell wrapper to apply this plan.");
    }
    Ok(())
}

fn upgrade(cli: &Cli, args: &UpgradeArgs) -> Result<()> {
    let report =
        upgrade::run(upgrade::UpgradeOptions { check_only: args.check, dry_run: cli.dry_run })?;
    if cli.json {
        print_json(&report)
    } else {
        println!("{}", report.message);
        if let Some(artifact) = &report.artifact {
            println!("artifact: {artifact}");
        }
        if let Some(path) = &report.binary_path {
            println!("binary: {path}");
        }
        Ok(())
    }
}

fn install(cli: &Cli, args: &InstallArgs) -> Result<()> {
    let shells = shells_for_install(cli, args.all);
    let reports = shells
        .into_iter()
        .map(|shell| install_shell(shell, cli.dry_run))
        .collect::<Result<Vec<_>, _>>()?;
    if cli.json {
        print_json(&reports)
    } else {
        for report in reports {
            println!("{}: {}", report.shell, report.message);
            if let Some(target) = report.target {
                println!("  {target}");
            }
            if let Some(backup) = report.backup {
                println!("  backup: {backup}");
            }
        }
        Ok(())
    }
}

fn uninstall(cli: &Cli, args: &InstallArgs) -> Result<()> {
    let shells = shells_for_install(cli, args.all);
    let reports = shells
        .into_iter()
        .map(|shell| uninstall_shell(shell, cli.dry_run))
        .collect::<Result<Vec<_>, _>>()?;
    if cli.json {
        print_json(&reports)
    } else {
        for report in reports {
            println!("{}: {}", report.shell, report.message);
            if let Some(target) = report.target {
                println!("  {target}");
            }
        }
        Ok(())
    }
}

fn init(cli: &Cli) -> Result<()> {
    let shell = selected_shell(cli);
    let report = init_snippet(shell);
    if cli.json {
        print_json(&report)
    } else {
        println!("{}", report.snippet);
        Ok(())
    }
}

fn watch(cli: &Cli, args: &WatchArgs) -> Result<()> {
    if args.install_service {
        let report = install_watch_service(cli.dry_run)?;
        return print_report(cli, &report);
    }
    if args.uninstall_service {
        let report = uninstall_watch_service(cli.dry_run)?;
        return print_report(cli, &report);
    }
    if args.status {
        let report = watch_service_status()?;
        return print_report(cli, &report);
    }

    let mut last_path = None::<String>;
    loop {
        let started = Instant::now();
        let plan = build_plan(cli, false)?;
        let changed = last_path.as_ref().map(|path| path != &plan.path).unwrap_or(true);
        if changed && !cli.dry_run {
            let _ = save_version(&plan);
        }
        if cli.json {
            print_json(&serde_json::json!({
                "changed": changed,
                "path_entries": plan.stats.computed_path_entries,
                "generated_at_unix": plan.generated_at_unix
            }))?;
        } else if changed && cli.dry_run {
            println!(
                "PATH version changed ({} entries); dry run did not save state",
                plan.stats.computed_path_entries
            );
        } else if changed {
            println!("PATH version saved ({} entries)", plan.stats.computed_path_entries);
        }
        last_path = Some(plan.path);
        if args.once {
            break;
        }
        let elapsed = started.elapsed();
        let interval = Duration::from_secs(args.interval.max(1));
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }
    }
    Ok(())
}

fn integrate(
    cli: &Cli,
    target: &CliIntegrationTarget,
    action: &CliIntegrationAction,
) -> Result<()> {
    let report = run_integration(
        target.clone().into(),
        action.clone().into(),
        selected_shell(cli),
        cli.dry_run,
    )?;
    print_report(cli, &report)
}

fn build_plan(cli: &Cli, remove_invalid: bool) -> Result<EnvironmentPlan> {
    let options = BuildOptions {
        shell: cli.shell.clone().map(Into::into),
        no_dedupe: cli.no_dedupe,
        strict: cli.strict,
        validate_paths: true,
        remove_invalid,
    };
    build_environment_plan(&options).context("failed to build environment plan")
}

fn selected_shell(cli: &Cli) -> ShellKind {
    cli.shell.clone().map(Into::into).unwrap_or_else(rpath_core::detect_shell)
}

fn shells_for_install(cli: &Cli, all: bool) -> Vec<ShellKind> {
    if !all {
        return vec![selected_shell(cli)];
    }
    if cfg!(windows) {
        vec![ShellKind::PowerShell, ShellKind::Pwsh, ShellKind::Cmd, ShellKind::Bash]
    } else {
        vec![ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish]
    }
}

fn load_or_latest_snapshot(id: Option<&str>) -> Result<EnvironmentSnapshot> {
    if let Some(id) = id {
        return Ok(load_snapshot(id)?);
    }
    list_snapshots()?.into_iter().last().context("no snapshots found")
}

fn restore_snapshot(cli: &Cli, snapshot: &EnvironmentSnapshot) -> Result<()> {
    let shell = selected_shell(cli);
    let emit = emit_snapshot(snapshot, shell);
    if cli.emit {
        print!("{}", emit.commands);
        return Ok(());
    }
    if cli.json {
        print_json(&emit)
    } else {
        println!("Snapshot {} is ready to restore", snapshot.id);
        println!(
            "Use `rpath snapshot restore {} --emit` from a shell wrapper to apply it.",
            snapshot.id
        );
        Ok(())
    }
}

fn list_snapshot_like(cli: &Cli, snapshots: Vec<EnvironmentSnapshot>) -> Result<()> {
    if cli.json {
        print_json(&snapshots)
    } else {
        for snapshot in snapshots {
            println!(
                "{}  {}  {} entries  {}",
                snapshot.id,
                snapshot.platform,
                snapshot.path_entries.len(),
                snapshot.reason
            );
        }
        Ok(())
    }
}

fn split_snapshot_path(snapshot: &EnvironmentSnapshot) -> Vec<String> {
    let separator = if snapshot.platform == "windows" { ';' } else { ':' };
    snapshot
        .path
        .split(separator)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn duplicate_diagnostics(plan: &EnvironmentPlan) -> Vec<rpath_core::Diagnostic> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for entry in &plan.path_entries {
        let key = if cfg!(windows) {
            entry.expanded.to_ascii_lowercase()
        } else {
            entry.expanded.clone()
        };
        if !seen.insert(key) {
            duplicates.push(
                rpath_core::Diagnostic::warning("duplicate-path", "duplicate PATH entry")
                    .with_path(entry.expanded.clone()),
            );
        }
    }
    duplicates
}

fn security_diagnostics(plan: &EnvironmentPlan) -> Vec<rpath_core::Diagnostic> {
    let mut diagnostics = Vec::new();
    for entry in &plan.path_entries {
        if is_world_writable(Path::new(&entry.expanded)) {
            diagnostics.push(
                rpath_core::Diagnostic::warning(
                    "world-writable-path",
                    "PATH entry is writable by other users",
                )
                .with_path(entry.expanded.clone()),
            );
        }
    }
    diagnostics
}

#[cfg(unix)]
fn is_world_writable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map(|metadata| metadata.permissions().mode() & 0o002 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_world_writable(_path: &Path) -> bool {
    false
}

fn severity_label(severity: &DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Info => "info",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    }
}

fn print_report<T>(cli: &Cli, report: &T) -> Result<()>
where
    T: Serialize,
{
    if cli.json {
        print_json(report)
    } else {
        let value = serde_json::to_value(report)?;
        if let Some(message) = value.get("message").and_then(|value| value.as_str()) {
            println!("{message}");
        } else {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        if let Some(supported) = value.get("supported").and_then(|value| value.as_bool()) {
            println!("supported: {supported}");
        }
        if let Some(changed) = value.get("changed").and_then(|value| value.as_bool()) {
            println!("changed: {changed}");
        }
        if let Some(path) = value.get("path").and_then(|value| value.as_str()) {
            println!("{path}");
        }
        Ok(())
    }
}

fn print_list(label: &str, items: &[String]) {
    println!("{label}:");
    if items.is_empty() {
        println!("  none");
    } else {
        for item in items {
            println!("  {item}");
        }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[allow(dead_code)]
fn ensure_state_dir_exists() -> Result<()> {
    fs::create_dir_all(state_dir()?)?;
    Ok(())
}
