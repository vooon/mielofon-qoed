//! `mielofon-controller cert` — mTLS certificate generation (nebula-cert style),
//! implemented as a controller subcommand and driven through `openssl`
//! subprocesses so no X.509 machinery is added to the daemon binary.
//!
//! Run on the operator's control plane only (a host with `openssl`); it never
//! ships keys — it only *generates* them. All names/IPs in help/examples are
//! placeholders ("spoke-1", "hub-a", RFC 5737 addresses).

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_DAYS: u64 = 825;

/// Certificate subcommands. Parsed by clap from `cert <ca|node|agent> ...`.
#[derive(Subcommand)]
pub enum CertCli {
    /// Generate a self-signed CA.
    Ca(CaArgs),
    /// Generate a server+client leaf for a controller node.
    Node(SignArgs),
    /// Generate a client leaf for an agent.
    Agent(SignArgs),
}

#[derive(clap::Args)]
pub struct CaArgs {
    /// Certificate common name.
    #[arg(long, default_value = "mielofon")]
    name: String,
    /// CA key output path.
    #[arg(long)]
    key: Option<PathBuf>,
    /// CA certificate output path.
    #[arg(long)]
    crt: Option<PathBuf>,
    /// Validity in days.
    #[arg(long, default_value_t = DEFAULT_DAYS)]
    days: u64,
}

#[derive(clap::Args)]
pub struct SignArgs {
    /// Certificate common name.
    #[arg(long, default_value = "mielofon")]
    name: String,
    /// Subject alternative name IP (repeatable).
    #[arg(long)]
    ip: Vec<String>,
    /// Subject alternative name DNS (repeatable).
    #[arg(long)]
    host: Vec<String>,
    /// CA key path.
    #[arg(long)]
    ca_key: PathBuf,
    /// CA certificate path.
    #[arg(long)]
    ca_crt: PathBuf,
    /// Leaf key output path.
    #[arg(long)]
    key: Option<PathBuf>,
    /// Leaf certificate output path.
    #[arg(long)]
    crt: Option<PathBuf>,
    /// Validity in days.
    #[arg(long, default_value_t = DEFAULT_DAYS)]
    days: u64,
}

/// Resolved signing material shared by the openssl helpers.
struct Flags {
    name: String,
    ips: Vec<String>,
    hosts: Vec<String>,
    ca_key: PathBuf,
    ca_crt: PathBuf,
    key: PathBuf,
    crt: PathBuf,
    days: u64,
}

pub fn run(cli: CertCli) -> Result<()> {
    match cli {
        CertCli::Ca(a) => cmd_ca(&Flags {
            name: a.name,
            ips: Vec::new(),
            hosts: Vec::new(),
            ca_key: PathBuf::from("ca.key"),
            ca_crt: PathBuf::from("ca.crt"),
            key: a.key.unwrap_or_else(|| "ca.key".into()),
            crt: a.crt.unwrap_or_else(|| "ca.crt".into()),
            days: a.days,
        }),
        CertCli::Node(a) => cmd_sign(&resolve_sign(a), false),
        CertCli::Agent(a) => cmd_sign(&resolve_sign(a), true),
    }
}

fn resolve_sign(a: SignArgs) -> Flags {
    let key = a
        .key
        .unwrap_or_else(|| PathBuf::from(format!("{}.key", a.name)));
    let crt = a
        .crt
        .unwrap_or_else(|| PathBuf::from(format!("{}.crt", a.name)));
    Flags {
        name: a.name,
        ips: a.ip,
        hosts: a.host,
        ca_key: a.ca_key,
        ca_crt: a.ca_crt,
        key,
        crt,
        days: a.days,
    }
}

fn run_openssl(args: &[String]) -> Result<()> {
    let out = Command::new("openssl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("spawn openssl")?;

    if !out.status.success() {
        bail!(
            "openssl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn gen_ec_key(key: &std::path::Path) -> Result<()> {
    run_openssl(&[
        "genpkey".to_string(),
        "-algorithm".to_string(),
        "EC".to_string(),
        "-pkeyopt".to_string(),
        "ec_paramgen_curve:P-256".to_string(),
        "-out".to_string(),
        key.to_string_lossy().into_owned(),
    ])
}

fn temp_path(suffix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "mielofon-cert-{}-{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        suffix
    ));
    p
}

fn cmd_ca(f: &Flags) -> Result<()> {
    gen_ec_key(&f.key)?;
    run_openssl(&[
        "req".to_string(),
        "-x509".to_string(),
        "-new".to_string(),
        "-key".to_string(),
        f.key.to_string_lossy().into_owned(),
        "-sha256".to_string(),
        "-days".to_string(),
        f.days.to_string(),
        "-subj".to_string(),
        format!("/CN={}", f.name),
        "-out".to_string(),
        f.crt.to_string_lossy().into_owned(),
    ])?;
    println!("wrote {}", f.key.display());
    println!("wrote {}", f.crt.display());
    Ok(())
}

fn cmd_sign(f: &Flags, agent: bool) -> Result<()> {
    if !f.ca_key.exists() {
        bail!(
            "CA key not found: {} (run `cert ca` first)",
            f.ca_key.display()
        );
    }
    if !f.ca_crt.exists() {
        bail!(
            "CA cert not found: {} (run `cert ca` first)",
            f.ca_crt.display()
        );
    }

    let mut ext = Vec::new();
    ext.push("keyUsage = digitalSignature".to_string());
    ext.push(format!(
        "extendedKeyUsage = {}",
        if agent {
            "clientAuth".to_string()
        } else {
            "serverAuth, clientAuth".to_string()
        }
    ));
    if !f.ips.is_empty() || !f.hosts.is_empty() {
        let sans = f
            .ips
            .iter()
            .map(|ip| format!("IP:{ip}"))
            .chain(f.hosts.iter().map(|h| format!("DNS:{h}")))
            .collect::<Vec<_>>()
            .join(",");
        ext.push(format!("subjectAltName = {sans}"));
    }

    let csr = temp_path(".csr");
    let extfile = temp_path(".ext");

    gen_ec_key(&f.key)?;

    let csr_args = vec![
        "req".to_string(),
        "-new".to_string(),
        "-key".to_string(),
        f.key.to_string_lossy().into_owned(),
        "-subj".to_string(),
        format!("/CN={}", f.name),
        "-out".to_string(),
        csr.to_string_lossy().into_owned(),
    ];
    run_openssl(&csr_args)?;

    if !ext.is_empty() {
        std::fs::write(&extfile, format!("{}\n", ext.join("\n"))).context("write extfile")?;
    }

    let days = f.days.to_string();

    let mut x509: Vec<String> = vec![
        "x509".to_string(),
        "-req".to_string(),
        "-in".to_string(),
        csr.to_string_lossy().into_owned(),
        "-CA".to_string(),
        f.ca_crt.to_string_lossy().into_owned(),
        "-CAkey".to_string(),
        f.ca_key.to_string_lossy().into_owned(),
        "-CAcreateserial".to_string(),
        "-sha256".to_string(),
        "-days".to_string(),
        days,
    ];
    if !ext.is_empty() {
        x509.push("-extfile".to_string());
        x509.push(extfile.to_string_lossy().into_owned());
    }
    x509.push("-out".to_string());
    x509.push(f.crt.to_string_lossy().into_owned());
    run_openssl(&x509)?;

    let _ = std::fs::remove_file(&csr);
    let _ = std::fs::remove_file(&extfile);

    println!("wrote {}", f.key.display());
    println!("wrote {}", f.crt.display());
    Ok(())
}
