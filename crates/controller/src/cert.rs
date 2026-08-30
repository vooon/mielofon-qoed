//! `mielofon-controller cert` — mTLS certificate generation (nebula-cert style),
//! implemented as a controller subcommand and driven through `openssl`
//! subprocesses so no X.509 machinery is added to the daemon binary.
//!
//! Run on the operator's control plane only (a host with `openssl`); it never
//! ships keys — it only *generates* them. All names/IPs in help/examples are
//! placeholders ("spoke-1", "hub-a", RFC 5737 addresses).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_DAYS: u64 = 825;

struct Flags {
    name: String,
    ips: Vec<String>,
    hosts: Vec<String>,
    ca_key: String,
    ca_crt: String,
    key: String,
    crt: String,
    days: u64,
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(sub) = args.first().map(String::as_str) else {
        help();
        bail!("missing cert subcommand");
    };

    match sub {
        "ca" => cmd_ca(&parse_flags(&args[1..], "ca.key", "ca.crt")?),
        "node" => cmd_sign(&parse_flags(&args[1..], "", "")?, false),
        "agent" => cmd_sign(&parse_flags(&args[1..], "", "")?, true),
        "help" | "--help" | "-h" => {
            help();
            Ok(())
        }
        other => {
            help();
            bail!("unknown cert subcommand: {other}")
        }
    }
}

fn help() {
    eprintln!(
        "mielofon-controller cert - mTLS certificate generation (openssl-based)\n\
         \n\
           cert ca    -name <cn> [-key <f>] [-crt <f>] [-days <n>]\n\
           cert node  [-ip <addr>]... [-host <dns>]... -ca-key <f> -ca-crt <f>\n\
         \x20                    [-name <cn>] [-key <f>] [-crt <f>] [-days <n>]\n\
           cert agent [-ip <addr>]... [-host <dns>]... -ca-key <f> -ca-crt <f>\n\
         \x20                    [-name <cn>] [-key <f>] [-crt <f>] [-days <n>]\n\
         \n\
         Generate a dedicated CA once (cert ca), then a server+client leaf per\n\
         controller node (cert node, with -ip/-host SAN) and a client leaf per\n\
         agent (cert agent). Keys are EC P-256. Examples (placeholders):\n\
         \n\
           cert ca -name mielofon-ca\n\
           cert node -name hub-a -ip 203.0.113.1 -host hub-a \\\n\
         \x20            -ca-key ca.key -ca-crt ca.crt\n\
           cert agent -name spoke-1 -ca-key ca.key -ca-crt ca.crt\n"
    );
}

fn parse_flags(args: &[String], def_key: &str, def_crt: &str) -> Result<Flags> {
    let mut f = Flags {
        name: "mielofon".into(),
        ips: Vec::new(),
        hosts: Vec::new(),
        ca_key: "ca.key".into(),
        ca_crt: "ca.crt".into(),
        key: def_key.to_string(),
        crt: def_crt.to_string(),
        days: DEFAULT_DAYS,
    };

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: &mut usize| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .context(format!("missing value for {flag}"))
        };

        match flag {
            "-name" => f.name = value(&mut i)?,
            "-ip" => f.ips.push(value(&mut i)?),
            "-host" => f.hosts.push(value(&mut i)?),
            "-ca-key" => f.ca_key = value(&mut i)?,
            "-ca-crt" => f.ca_crt = value(&mut i)?,
            "-key" => f.key = value(&mut i)?,
            "-crt" => f.crt = value(&mut i)?,
            "-days" => f.days = value(&mut i)?.parse().context("invalid -days")?,
            "-h" | "--help" => {
                help();
                std::process::exit(0);
            }
            other => bail!("unknown flag: {other}"),
        }
        i += 1;
    }

    if f.key.is_empty() {
        f.key = format!("{}.key", f.name);
    }
    if f.crt.is_empty() {
        f.crt = format!("{}.crt", f.name);
    }

    Ok(f)
}

fn run_openssl(args: &[&str]) -> Result<()> {
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

fn gen_ec_key(key: &str) -> Result<()> {
    run_openssl(&[
        "genpkey",
        "-algorithm",
        "EC",
        "-pkeyopt",
        "ec_paramgen_curve:P-256",
        "-out",
        key,
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
        "req",
        "-x509",
        "-new",
        "-key",
        &f.key,
        "-sha256",
        "-days",
        &f.days.to_string(),
        "-subj",
        &format!("/CN={}", f.name),
        "-out",
        &f.crt,
    ])?;
    println!("wrote {}", f.key);
    println!("wrote {}", f.crt);
    Ok(())
}

fn cmd_sign(f: &Flags, agent: bool) -> Result<()> {
    if !std::path::Path::new(&f.ca_key).exists() {
        bail!("CA key not found: {} (run `cert ca` first)", f.ca_key);
    }
    if !std::path::Path::new(&f.ca_crt).exists() {
        bail!("CA cert not found: {} (run `cert ca` first)", f.ca_crt);
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

    let csr_args = [
        "req",
        "-new",
        "-key",
        &f.key,
        "-subj",
        &format!("/CN={}", f.name),
        "-out",
        csr.to_str().unwrap(),
    ];
    run_openssl(&csr_args)?;

    if !ext.is_empty() {
        std::fs::write(&extfile, format!("{}\n", ext.join("\n"))).context("write extfile")?;
    }

    let days = f.days.to_string();

    let mut x509: Vec<&str> = vec![
        "x509",
        "-req",
        "-in",
        csr.to_str().unwrap(),
        "-CA",
        &f.ca_crt,
        "-CAkey",
        &f.ca_key,
        "-CAcreateserial",
        "-sha256",
        "-days",
        &days,
    ];
    if !ext.is_empty() {
        x509.push("-extfile");
        x509.push(extfile.to_str().unwrap());
    }
    x509.push("-out");
    x509.push(&f.crt);
    run_openssl(&x509)?;

    let _ = std::fs::remove_file(&csr);
    let _ = std::fs::remove_file(&extfile);

    println!("wrote {}", f.key);
    println!("wrote {}", f.crt);
    Ok(())
}
