use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

mod devnet;

#[derive(Debug, Parser)]
#[command(name = "evolving-dob-tools")]
struct Cli {
    #[arg(long, global = true)]
    profile_root: Option<PathBuf>,
    #[command(subcommand)]
    command: Tool,
}

#[derive(Debug, Subcommand)]
enum Tool {
    /// Run the offline package and registry pressure gate.
    RegistryPressure,
    /// Run the local CKB devnet deployment and registry workflow.
    DevnetWorkflow {
        #[arg(long)]
        ckb_repo: Option<PathBuf>,
        #[arg(long)]
        ckb_bin: Option<PathBuf>,
        #[arg(long)]
        network: Option<String>,
        #[arg(long)]
        run_dir: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        keep_node: bool,
        #[arg(long)]
        pretty: bool,
    },
}

fn profile_root(override_root: Option<&Path>) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return fs::canonicalize(root).with_context(|| format!("failed to resolve profile root {}", root.display()));
    }
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).parent().context("tools crate has no profile parent")?)
        .context("failed to resolve profile root")
}

pub(crate) fn repo_root(profile: &Path) -> Result<PathBuf> {
    Ok(profile.ancestors().nth(3).context("profile must live under proposals/evolving-dob")?.to_path_buf())
}

pub(crate) fn cellc(repo: &Path) -> Vec<String> {
    if let Some(configured) = std::env::var_os("CELLC") {
        return vec![configured.to_string_lossy().into_owned()];
    }
    let binary = repo.join("target/debug/cellc");
    if binary.exists() {
        return vec![binary.to_string_lossy().into_owned()];
    }
    vec![
        "cargo".into(),
        "run".into(),
        "--locked".into(),
        "-p".into(),
        "cellscript".into(),
        "--bin".into(),
        "cellc".into(),
        "--manifest-path".into(),
        repo.join("Cargo.toml").to_string_lossy().into_owned(),
        "--".into(),
    ]
}

fn run(profile: &Path, command: &[String], args: &[&str]) -> Result<Output> {
    let (program, prefix) = command.split_first().context("empty cellc command")?;
    Command::new(program).args(prefix).args(args).current_dir(profile).output().with_context(|| format!("failed to execute {program}"))
}

fn require(condition: bool, message: impl std::fmt::Display) -> Result<()> {
    if !condition {
        bail!("registry-pressure: {message}");
    }
    Ok(())
}

fn require_success(output: &Output, operation: &str) -> Result<()> {
    require(
        output.status.success(),
        format!(
            "{operation} failed\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn locked_source_hash(text: &str) -> Option<&str> {
    text.lines().find(|line| line.starts_with("source_hash = ")).and_then(|line| line.split('"').nth(1))
}

fn registry_pressure(profile: &Path) -> Result<()> {
    let repo = repo_root(profile)?;
    let command = cellc(&repo);
    let manifest_path = profile.join("Cell.toml");
    require(manifest_path.exists(), "Cell.toml is missing")?;
    let manifest_text = fs::read_to_string(&manifest_path)?;
    let manifest: toml::Value = toml::from_str(&manifest_text)?;
    for (needle, message) in [
        ("namespace = \"dob\"", "package namespace must remain dob"),
        ("production = true", "compiler production policy must remain enabled"),
        ("deny_fail_closed = true", "fail-closed denial must remain enabled"),
        ("deny_runtime_obligations = true", "runtime-obligation denial must remain enabled"),
        ("legacy_version_support = false", "legacy support must remain disabled"),
    ] {
        require(manifest_text.contains(needle), message)?;
    }

    let build = run(
        profile,
        &command,
        &["build", "--release", "--target", "riscv64-elf", "--target-profile", "ckb", "--primitive-strict", "0.16"],
    )?;
    require_success(&build, "cellc build")?;
    let check = run(profile, &command, &["check", "--target-profile", "ckb", "--primitive-strict", "0.16", "--json"])?;
    require_success(&check, "production check")?;
    let check_json: Value = serde_json::from_slice(&check.stdout)?;
    require(
        check_json.get("status").and_then(Value::as_str) == Some("ok"),
        format!("production check returned {:?}", check_json.get("status")),
    )?;
    require(
        check_json.pointer("/policy/production").and_then(Value::as_bool) == Some(true),
        "production check did not use production policy",
    )?;
    for target in check_json.get("checked_targets").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        require(
            !target.get("fail_closed_runtime_features").and_then(Value::as_array).is_some_and(|features| !features.is_empty()),
            "production check exposed fail-closed runtime features",
        )?;
        require(
            target.get("runtime_required_verifier_obligations").and_then(Value::as_u64) == Some(0),
            "production check exposed runtime-required verifier obligations",
        )?;
        require(
            target.get("runtime_required_transaction_runtime_input_requirements").and_then(Value::as_u64) == Some(0),
            "production check exposed runtime-required transaction runtime inputs",
        )?;
    }
    let package_verify = run(profile, &command, &["package", "verify", "--json"])?;
    require_success(&package_verify, "package verify")?;
    let package_json: Value = serde_json::from_slice(&package_verify.stdout)?;
    require(
        package_json.get("status").and_then(Value::as_str) == Some("ok"),
        format!("package verify returned {:?}", package_json.get("status")),
    )?;
    let publish = run(profile, &command, &["publish", "--dry-run"])?;
    require_success(&publish, "publish dry-run")?;

    let lock_path = profile.join("Cell.lock");
    require(lock_path.exists(), "Cell.lock was not written by build")?;
    let lock_text = fs::read_to_string(&lock_path)?;
    let lock: toml::Value = toml::from_str(&lock_text)?;
    for (needle, message) in [
        ("name = \"evolving-dob-profile-v1\"", "Cell.lock package name mismatch"),
        ("namespace = \"dob\"", "Cell.lock namespace mismatch"),
        ("[package_build]", "Cell.lock has no package_build identity"),
    ] {
        require(lock_text.contains(needle), message)?;
    }
    let manifest_version = manifest.get("package").and_then(|value| value.get("cellscript_version"));
    let compiler_version = lock.get("package_build").and_then(|value| value.get("compiler_version"));
    require(
        manifest_version == compiler_version,
        format!("Cell.toml cellscript_version {manifest_version:?} does not match Cell.lock compiler_version {compiler_version:?}"),
    )?;
    let registry_path = profile.join("registry.json");
    if registry_path.exists() {
        let registry: Value = serde_json::from_slice(&fs::read(&registry_path)?)?;
        require(
            registry.get("name").and_then(Value::as_str) == Some("evolving-dob-profile-v1"),
            "registry.json package name mismatch",
        )?;
        require(registry.get("namespace").and_then(Value::as_str) == Some("dob"), "registry.json namespace mismatch")?;
        let version = registry
            .get("versions")
            .and_then(Value::as_array)
            .and_then(|versions| versions.iter().find(|version| version.get("version").and_then(Value::as_str) == Some("1.0.0")));
        require(version.is_some(), "registry.json missing v1.0.0")?;
        require(
            version.and_then(|value| value.get("source_hash")).and_then(Value::as_str) == locked_source_hash(&lock_text),
            "registry.json source hash does not match Cell.lock",
        )?;
    }
    if profile.join("Deployed.toml").exists() {
        let verify = run(profile, &command, &["registry", "verify", "--json", "--require-audit-report"])?;
        require(
            verify.status.success(),
            format!(
                "registry verify failed with Deployed.toml present\nSTDOUT:\n{}\nSTDERR:\n{}",
                String::from_utf8_lossy(&verify.stdout),
                String::from_utf8_lossy(&verify.stderr)
            ),
        )?;
    }
    println!("registry-pressure: ok");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = profile_root(cli.profile_root.as_deref()).and_then(|profile| match cli.command {
        Tool::RegistryPressure => registry_pressure(&profile),
        Tool::DevnetWorkflow { ckb_repo, ckb_bin, network, run_dir, output, keep_node, pretty } => devnet::run(
            &profile,
            ckb_repo.as_deref(),
            ckb_bin.as_deref(),
            network.as_deref(),
            run_dir.as_deref(),
            output.as_deref(),
            keep_node,
            pretty,
        ),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
