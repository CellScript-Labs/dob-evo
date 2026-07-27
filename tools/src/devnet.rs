use std::fs::{self, File};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use blake2b_ref::Blake2bBuilder;
use regex::Regex;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{cellc, repo_root};

const CKB_PERSONAL: &[u8] = b"ckb-default-hash";
const ALWAYS_SUCCESS_CODE_HASH: &str = "0x28e83a1277d48add8e72fadaa9248559e1b632bab2bd60b27955ebc4c03800a5";
const ALWAYS_SUCCESS_INDEX: u64 = 1;
const SHANNONS: u64 = 100_000_000;
const FEE: u64 = 100_000;
const ACTIONS: &[&str] = &["mint_dob_state", "evolve_dob_state", "finalise_dob_state"];

fn execute(command: &[String], args: &[String], cwd: &Path) -> Result<Output> {
    let (program, prefix) = command.split_first().context("empty command")?;
    Command::new(program).args(prefix).args(args).current_dir(cwd).output().with_context(|| format!("failed to execute {program}"))
}

fn checked(command: &[String], args: &[String], cwd: &Path) -> Result<Output> {
    let output = execute(command, args, cwd)?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = stdout.chars().rev().take(5000).collect::<String>().chars().rev().collect::<String>();
        let stderr = stderr.chars().rev().take(5000).collect::<String>().chars().rev().collect::<String>();
        bail!("command failed: {} {}\nSTDOUT:\n{}\nSTDERR:\n{}", command.join(" "), args.join(" "), stdout, stderr);
    }
    Ok(output)
}

fn pick_port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn copy_template(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_template(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn prepare_node(ckb_repo: &Path, ckb_dir: &Path, rpc_port: u16, p2p_port: u16) -> Result<()> {
    let template = ckb_repo.join("test/template");
    let template_config = template.join("ckb.toml");
    if !template_config.exists() {
        bail!("CKB test template missing: {}", template.display());
    }
    copy_template(&template, ckb_dir)?;
    let config = ckb_dir.join("ckb.toml");
    let text = fs::read_to_string(&config)?;
    let rpc = Regex::new(r#"listen_address = "127\.0\.0\.1:\d+""#)?;
    let p2p = Regex::new(r#"listen_addresses = \["/ip4/0\.0\.0\.0/tcp/\d+"\]"#)?;
    let text = rpc.replacen(&text, 1, format!("listen_address = \"127.0.0.1:{rpc_port}\"").as_str());
    let text = p2p.replacen(&text, 1, format!("listen_addresses = [\"/ip4/127.0.0.1/tcp/{p2p_port}\"]").as_str());
    fs::write(config, text.as_bytes())?;
    Ok(())
}

fn rpc(client: &Client, url: &str, method: &str, params: Vec<Value>) -> Result<Value> {
    let payload: Value = client
        .post(url)
        .json(&json!({"id": 42, "jsonrpc": "2.0", "method": method, "params": params}))
        .send()
        .with_context(|| format!("RPC {method} failed to connect"))?
        .error_for_status()?
        .json()?;
    if !payload["error"].is_null() {
        bail!("RPC {method} returned error: {}", payload["error"]);
    }
    Ok(payload.get("result").cloned().unwrap_or(Value::Null))
}

fn wait_ready(url: &str) -> Result<Client> {
    let client = Client::builder().timeout(Duration::from_secs(20)).build()?;
    let short = Client::builder().timeout(Duration::from_secs(2)).build()?;
    let mut last = String::new();
    for _ in 0..160 {
        match rpc(&short, url, "get_tip_header", vec![]) {
            Ok(value) if !value.is_null() => return Ok(client),
            Ok(_) => {}
            Err(error) => last = error.to_string(),
        }
        thread::sleep(Duration::from_millis(250));
    }
    bail!("CKB RPC did not become ready at {url}: {last}")
}

fn out_point(hash: &str, index: u64) -> Value {
    json!({"tx_hash": hash, "index": format!("0x{index:x}")})
}

fn wait_live(client: &Client, url: &str, hash: &str, index: u64, attempts: usize, delay_ms: u64) -> Result<Value> {
    let mut last = Value::Null;
    for _ in 0..attempts {
        last = rpc(client, url, "get_live_cell", vec![out_point(hash, index), json!(true)])?;
        if last["status"] == "live" {
            return Ok(last);
        }
        thread::sleep(Duration::from_millis(delay_ms));
    }
    Ok(last)
}

fn collect_funding(client: &Client, url: &str, required: u64) -> Result<(Vec<Value>, u64)> {
    let mut cells = Vec::new();
    let mut total = 0_u64;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..512 {
        let block_hash = rpc(client, url, "generate_block", vec![])?;
        let block = rpc(client, url, "get_block", vec![block_hash])?;
        let cellbase = &block["transactions"][0];
        let hash = cellbase["hash"].as_str().context("cellbase hash missing")?;
        for (index, output) in cellbase["outputs"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().enumerate() {
            if !seen.insert((hash.to_owned(), index)) {
                continue;
            }
            let live = wait_live(client, url, hash, index as u64, 6, 50)?;
            if live["status"] != "live" {
                continue;
            }
            let capacity = u64::from_str_radix(output["capacity"].as_str().context("capacity missing")?.trim_start_matches("0x"), 16)?;
            cells.push(json!({"tx_hash": hash, "index": index, "capacity": capacity}));
            total += capacity;
            if total >= required {
                return Ok((cells, total));
            }
        }
    }
    bail!("insufficient generated devnet capacity: need {required}, collected {total}")
}

fn transaction(inputs: &[Value], output: Value, output_data: &str, deps: Vec<Value>) -> Value {
    json!({
        "version": "0x0", "cell_deps": deps, "header_deps": [],
        "inputs": inputs.iter().map(|cell| json!({
            "previous_output": out_point(cell["tx_hash"].as_str().unwrap(), cell["index"].as_u64().unwrap()), "since": "0x0"
        })).collect::<Vec<_>>(),
        "outputs": [output], "outputs_data": [output_data], "witnesses": ["0x0000000000000000"]
    })
}

fn ckb_hash(bytes: &[u8]) -> String {
    let mut state = Blake2bBuilder::new(32).personal(CKB_PERSONAL).build();
    state.update(bytes);
    let mut digest = [0_u8; 32];
    state.finalize(&mut digest);
    format!("0x{}", hex::encode(digest))
}

fn sha256(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(Sha256::digest(bytes)))
}

fn toml_str(value: &toml::Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn write_deployed(
    profile: &Path,
    lock: &toml::Value,
    chain: &str,
    network: &str,
    tx_hash: &str,
    data_hash: &str,
    audit: &str,
) -> Result<()> {
    let package = &lock["package"];
    let build = &lock["package_build"];
    let text = format!(
        r#"version = 1
schema = "cellscript-deployed-v0.19"

[package]
name = "{}"
version = "{}"
source_hash = "{}"

[build]
compiler_version = "{}"
artifact_hash = "{}"
metadata_hash = "{}"
schema_hash = "{}"
cell_data_codec_manifest_hash = "{}"
abi_hash = "{}"
constraints_hash = "{}"

[[deployments]]
name = "evolving-dob-profile-v1-local-devnet"
status = "active"
network = "{}"
chain_id = "{}"
tx_hash = "{}"
output_index = 0
code_hash = "{}"
data_hash = "{}"
hash_type = "data1"
dep_type = "code"
out_point = "{}:0"
artifact_hash = "{}"
metadata_hash = "{}"
schema_hash = "{}"
cell_data_codec_manifest_hash = "{}"
abi_hash = "{}"
constraints_hash = "{}"
compiler_version = "{}"
audit_report_hash = "{}"
"#,
        toml_str(&package["name"]),
        toml_str(&package["version"]),
        toml_str(&package["source_hash"]),
        toml_str(&build["compiler_version"]),
        toml_str(&build["artifact_hash"]),
        toml_str(&build["metadata_hash"]),
        toml_str(&build["schema_hash"]),
        toml_str(&build["cell_data_codec_manifest_hash"]),
        toml_str(&build["abi_hash"]),
        toml_str(&build["constraints_hash"]),
        network,
        chain,
        tx_hash,
        data_hash,
        data_hash,
        tx_hash,
        toml_str(&build["artifact_hash"]),
        toml_str(&build["metadata_hash"]),
        toml_str(&build["schema_hash"]),
        toml_str(&build["cell_data_codec_manifest_hash"]),
        toml_str(&build["abi_hash"]),
        toml_str(&build["constraints_hash"]),
        toml_str(&build["compiler_version"]),
        audit,
    );
    fs::write(profile.join("Deployed.toml"), text)?;
    Ok(())
}

fn action_summary(path: &Path) -> Result<Value> {
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    let draft = &value["transaction_draft"];
    let ckb = &value["ckb"];
    Ok(json!({
        "path": path.display().to_string(), "status": value["status"], "action": value["action"], "policy": value["policy"],
        "artifact_hash": value["artifact_hash"], "requires_live_cell_resolution": draft["requires_live_cell_resolution"],
        "requires_packed_materialization": draft["requires_packed_materialization"],
        "dry_run_required_for_production": ckb["dry_run_required_for_production"], "required_evidence": draft["required_evidence"]
    }))
}

fn resolve_ckb(repo: &Path, configured: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = configured {
        let path = fs::canonicalize(path).with_context(|| format!("CKB_BIN is not executable: {}", path.display()))?;
        if !path.is_file() {
            bail!("CKB_BIN is not executable: {}", path.display());
        }
        return Ok(path);
    }
    for candidate in [repo.join("target/debug/ckb"), repo.join("target/release/ckb")] {
        if candidate.is_file() {
            return Ok(fs::canonicalize(candidate)?);
        }
    }
    bail!("no CKB executable found under {}; set CKB_BIN or build ckb first", repo.display())
}

fn stop_node(child: &mut Child, keep: bool) -> Option<i32> {
    if child.try_wait().ok().flatten().is_none() && !keep {
        let _ = Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
        for _ in 0..100 {
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        return child.wait().ok().and_then(|status| status.code());
    }
    child.try_wait().ok().flatten().and_then(|status| status.code())
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    profile: &Path,
    ckb_repo: Option<&Path>,
    ckb_bin: Option<&Path>,
    network: Option<&str>,
    run_dir: Option<&Path>,
    output: Option<&Path>,
    keep_node: bool,
    pretty: bool,
) -> Result<()> {
    let repo = repo_root(profile)?;
    let network =
        network.map(str::to_owned).or_else(|| std::env::var("DOB_EVO_DEVNET_NETWORK").ok()).unwrap_or_else(|| "devnet".into());
    let ckb_repo = ckb_repo
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CKB_REPO").map(PathBuf::from))
        .unwrap_or_else(|| repo.parent().unwrap().join("ckb"));
    let ckb_bin = ckb_bin.map(Path::to_path_buf).or_else(|| std::env::var_os("CKB_BIN").map(PathBuf::from));
    let stamp = Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|| "unknown-time".into());
    let run_dir = run_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| profile.join(format!("target/devnet-workflow/{stamp}-{}", std::process::id())));
    fs::create_dir_all(&run_dir)?;
    let run_dir = fs::canonicalize(run_dir)?;
    let report_path = match output {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => profile.join(path),
        None => profile.join("target/devnet-workflow/report.json"),
    };
    fs::create_dir_all(report_path.parent().context("report path has no parent")?)?;
    let mut report = json!({"schema": "dob-evo-local-devnet-workflow-v1", "status": "started", "network": network, "run_dir": run_dir.display().to_string()});
    let mut node: Option<Child> = None;
    let workflow = (|| -> Result<()> {
        let cellc = cellc(&repo);
        let strict = vec!["build", "--release", "--target", "riscv64-elf", "--target-profile", "ckb", "--primitive-strict", "0.16"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        checked(&cellc, &strict, profile)?;
        let check = checked(
            &cellc,
            &vec!["check", "--target-profile", "ckb", "--primitive-strict", "0.16", "--json"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            profile,
        )?;
        let check_json: Value = serde_json::from_slice(&check.stdout)?;
        if check_json["status"] != "ok" {
            bail!("strict production check did not return ok");
        }
        checked(&cellc, &vec!["package", "verify", "--json"].into_iter().map(str::to_owned).collect::<Vec<_>>(), profile)?;
        checked(&cellc, &vec!["publish", "--dry-run"].into_iter().map(str::to_owned).collect::<Vec<_>>(), profile)?;
        let lock: toml::Value = toml::from_str(&fs::read_to_string(profile.join("Cell.lock"))?)?;
        let artifact = profile.join("build/evolving_dob_type.elf");
        let metadata = profile.join("build/evolving_dob_type.elf.meta.json");
        if !artifact.exists() {
            bail!("compiled artifact missing: {}", artifact.display());
        }
        if !metadata.exists() {
            bail!("compile metadata missing: {}", metadata.display());
        }
        let artifact_bytes = fs::read(&artifact)?;
        let data_hash = ckb_hash(&artifact_bytes);
        if data_hash.trim_start_matches("0x") != lock["package_build"]["artifact_hash"].as_str().unwrap_or_default() {
            bail!("artifact data hash does not match Cell.lock artifact_hash");
        }
        let mut action_summaries = Vec::new();
        let mut recommended = 0_u64;
        for action in ACTIONS {
            let path = run_dir.join(format!("action-{action}.json"));
            let args = vec![
                "action",
                "build",
                ".",
                "--action",
                action,
                "--target-profile",
                "ckb",
                "--json",
                "--output",
                path.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
            checked(&cellc, &args, profile)?;
            action_summaries.push(action_summary(&path)?);
            let plan: Value = serde_json::from_slice(&fs::read(&path)?)?;
            if let Some(capacity) =
                plan.pointer("/ckb/capacity_evidence_contract/recommended_code_cell_capacity_shannons").and_then(Value::as_u64)
            {
                recommended = recommended.max(capacity);
            }
        }
        let ckb_repo = fs::canonicalize(&ckb_repo)?;
        let ckb_bin = resolve_ckb(&ckb_repo, ckb_bin.as_deref())?;
        let ckb_dir = run_dir.join("ckb-node");
        let rpc_port = pick_port()?;
        let p2p_port = pick_port()?;
        let rpc_url = format!("http://127.0.0.1:{rpc_port}");
        prepare_node(&ckb_repo, &ckb_dir, rpc_port, p2p_port)?;
        let log_path = run_dir.join("ckb.log");
        let log = File::create(&log_path)?;
        node = Some(
            Command::new(&ckb_bin)
                .args(["-C", ckb_dir.to_str().unwrap(), "run", "--ba-advanced"])
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()?,
        );
        let client = wait_ready(&rpc_url)?;
        let chain_info = rpc(&client, &rpc_url, "get_blockchain_info", vec![])?;
        let chain = chain_info.get("chain").or_else(|| chain_info.get("chain_id")).and_then(Value::as_str).unwrap_or("ckb-dev");
        let genesis = rpc(&client, &rpc_url, "get_block_by_number", vec![json!("0x0")])?;
        let genesis_hash = genesis["transactions"][0]["hash"].as_str().context("genesis cellbase hash missing")?;
        let dep = json!({"out_point": out_point(genesis_hash, ALWAYS_SUCCESS_INDEX), "dep_type": "code"});
        let min_capacity = recommended.max((artifact_bytes.len() as u64 + 1024) * SHANNONS);
        let (funding, total) = collect_funding(&client, &rpc_url, min_capacity + FEE)?;
        let output_capacity = total - FEE;
        let code_output = json!({"capacity": format!("0x{output_capacity:x}"), "lock": {"code_hash": ALWAYS_SUCCESS_CODE_HASH, "hash_type": "data", "args": "0x"}, "type": Value::Null});
        let tx = transaction(&funding, code_output, &format!("0x{}", hex::encode(&artifact_bytes)), vec![dep]);
        let estimate = rpc(&client, &rpc_url, "estimate_cycles", vec![tx.clone()])?;
        let pool = rpc(&client, &rpc_url, "test_tx_pool_accept", vec![tx.clone(), json!("passthrough")])?;
        let tx_hash = rpc(&client, &rpc_url, "send_transaction", vec![tx, json!("passthrough")])?
            .as_str()
            .context("send_transaction result missing")?
            .to_owned();
        let mut live = Value::Null;
        let mut commit = "unknown";
        for _ in 0..20 {
            rpc(&client, &rpc_url, "generate_block", vec![])?;
            live = wait_live(&client, &rpc_url, &tx_hash, 0, 5, 100)?;
            if live["status"] == "live" {
                commit = "committed";
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        if commit != "committed" {
            bail!("deploy transaction {tx_hash} did not produce a live code cell");
        }
        let live_hash = live.pointer("/cell/data/hash").and_then(Value::as_str).unwrap_or_default();
        if live_hash != data_hash {
            bail!("live code cell data hash does not match compiled artifact");
        }
        let source_hash = lock["package"]["source_hash"].as_str().unwrap_or_default();
        let artifact_hash = lock["package_build"]["artifact_hash"].as_str().unwrap_or_default();
        let audit_payload = format!("{{\"artifact_hash\": \"{artifact_hash}\", \"network\": \"{network}\", \"schema\": \"dob-evo-local-devnet-audit-presence-v1\", \"source_hash\": \"{source_hash}\", \"tx_hash\": \"{tx_hash}\"}}");
        let audit_hash = sha256(audit_payload.as_bytes());
        write_deployed(profile, &lock, chain, &network, &tx_hash, &data_hash, &audit_hash)?;
        let deployed_copy = run_dir.join("Deployed.toml");
        fs::copy(profile.join("Deployed.toml"), &deployed_copy)?;
        checked(&cellc, &strict, profile)?;
        let offline = checked(
            &cellc,
            &vec!["registry", "verify", "--json", "--require-audit-report"].into_iter().map(str::to_owned).collect::<Vec<_>>(),
            profile,
        )?;
        let live_verify = checked(
            &cellc,
            &vec!["registry", "verify", "--json", "--live", "--rpc-url", &rpc_url, "--network", &network, "--require-audit-report"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            profile,
        )?;
        let builder = run_dir.join("generated-builder");
        let builder_args = vec![
            "gen-builder",
            ".",
            "--target",
            "typescript",
            "--metadata",
            metadata.to_str().unwrap(),
            "--lockfile",
            profile.join("Cell.lock").to_str().unwrap(),
            "--deployed",
            profile.join("Deployed.toml").to_str().unwrap(),
            "--deployment-network",
            &network,
            "--output",
            builder.to_str().unwrap(),
            "--package-name",
            "@dob/evolving-dob-profile-v1-builder",
            "--json",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let builder_summary = checked(&cellc, &builder_args, profile)?;
        checked(
            &["npm".into()],
            &["--prefix".into(), builder.display().to_string(), "install".into(), "--ignore-scripts".into()],
            profile,
        )?;
        checked(&["npm".into()], &["--prefix".into(), builder.display().to_string(), "test".into()], profile)?;
        report = json!({
            "schema": "dob-evo-local-devnet-workflow-v1", "status": "passed", "network": network, "run_dir": run_dir.display().to_string(),
            "package": {"name": lock["package"]["name"].as_str(), "version": lock["package"]["version"].as_str(), "namespace": lock["package"]["namespace"].as_str(), "source_hash": lock["package"]["source_hash"].as_str()},
            "artifact": {"path": artifact.display().to_string(), "size_bytes": artifact_bytes.len(), "data_hash": data_hash, "cell_lock_artifact_hash": artifact_hash},
            "local_devnet": {"rpc_url": rpc_url, "chain_id": chain, "ckb_bin": ckb_bin.display().to_string(), "ckb_log": log_path.display().to_string(),
                "funding_input_count": funding.len(), "funding_capacity_shannons": total, "code_output_capacity_shannons": output_capacity,
                "fee_shannons": FEE, "estimate_cycles": estimate, "test_tx_pool_accept": pool, "deploy_tx_hash": tx_hash,
                "deploy_out_point": format!("{tx_hash}:0"), "live_data_hash": live_hash, "commit_status": commit},
            "deployed_toml": deployed_copy.display().to_string(), "registry": {"offline": serde_json::from_slice::<Value>(&offline.stdout)?, "live": serde_json::from_slice::<Value>(&live_verify.stdout)?},
            "action_plans": action_summaries, "generated_builder": {"path": builder.display().to_string(), "summary": serde_json::from_slice::<Value>(&builder_summary.stdout)?, "npm_install": "passed", "npm_test": "passed"}
        });
        Ok(())
    })();
    if let Err(error) = workflow {
        report["status"] = json!("failed");
        report["error"] = json!(error.to_string());
    }
    if let Some(child) = node.as_mut() {
        let exit = stop_node(child, keep_node);
        if report.get("local_devnet").and_then(Value::as_object).is_none() {
            report["local_devnet"] = json!({});
        }
        report["local_devnet"]["node_exit_status"] = exit.map_or(Value::Null, Value::from);
    }
    let mut text = if pretty { serde_json::to_string_pretty(&report)? } else { serde_json::to_string(&report)? };
    text.push('\n');
    File::create(&report_path)?.write_all(text.as_bytes())?;
    println!("wrote {} status={}", report_path.display(), report["status"].as_str().unwrap_or("failed"));
    if report["status"] == "passed" {
        Ok(())
    } else {
        bail!("devnet workflow failed; see {}", report_path.display())
    }
}
