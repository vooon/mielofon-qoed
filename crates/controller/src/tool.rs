//! `mielofon-controller tool` — operator and test tooling that speaks the
//! agent protocol. `hello` registers a synthetic agent over a real QUIC
//! session and holds it open, which is how the e2e harness gets a link
//! owner onto a node without a router.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use mielofon_proto::{AgentRequest, LinkKey, RegisterReq, RegisterResp, WorkCmd};
use mielofon_transport::{parse_endpoint, request, resolve_all, respond, Dialer, Session};
use std::time::Duration;

#[derive(Subcommand)]
pub enum ToolCli {
    /// Register a synthetic agent on a controller and hold the session open.
    Hello(HelloArgs),
}

#[derive(clap::Args)]
pub struct HelloArgs {
    /// Controller client endpoint, `host:port` (port defaults to 9552).
    #[arg(long)]
    pub endpoint: String,
    /// CA certificate (PEM).
    #[arg(long)]
    pub ca: String,
    /// Client certificate (PEM).
    #[arg(long)]
    pub cert: String,
    /// Client private key (PEM).
    #[arg(long)]
    pub key: String,
    /// Agent name to register as.
    #[arg(long)]
    pub agent: String,
    /// Links to own, as `from,to,interface`; repeatable.
    #[arg(long = "link")]
    pub links: Vec<String>,
    /// Mesh loopback to report (for traces).
    #[arg(long)]
    pub loopback: Option<String>,
    /// How long to keep the session open before exiting.
    #[arg(long, default_value_t = 300)]
    pub hold_secs: u64,
}

fn parse_link(s: &str) -> Result<LinkKey> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    match parts.as_slice() {
        [from, to, iface] if !from.is_empty() && !to.is_empty() && !iface.is_empty() => {
            Ok(LinkKey::new(*from, *to, *iface))
        }
        _ => bail!("link `{s}`: want from,to,interface"),
    }
}

pub async fn run(cli: ToolCli) -> Result<()> {
    match cli {
        ToolCli::Hello(args) => hello(args).await,
    }
}

async fn hello(args: HelloArgs) -> Result<()> {
    let links = args
        .links
        .iter()
        .map(|l| parse_link(l))
        .collect::<Result<Vec<_>>>()?;
    let tls = crate::config::Tls {
        ca: args.ca.clone(),
        cert: args.cert.clone(),
        key: args.key.clone(),
    };
    let client_tls = crate::tls::client_config(&tls, &[&args.ca])?;
    let dialer = Dialer::new(client_tls)?;
    let endpoint = parse_endpoint(&args.endpoint, 9552).map_err(anyhow::Error::msg)?;
    let candidates = resolve_all(std::slice::from_ref(&endpoint)).await;
    let Some((addr, server_name)) = candidates.into_iter().next() else {
        bail!("{} did not resolve", args.endpoint);
    };
    let session = dialer
        .dial(addr, &server_name, Duration::from_secs(5))
        .await
        .context("dial")?;
    let req = AgentRequest::Register(RegisterReq {
        agent: args.agent.clone(),
        links,
        loopback: args.loopback.clone(),
    });
    let resp: RegisterResp = request(&session, &req, Duration::from_secs(10))
        .await
        .context("register")?;
    if !resp.ok {
        bail!("controller refused: {}", resp.error.unwrap_or_default());
    }
    println!("registered {} at {addr}", args.agent);

    // Hold the session: every command stream is closed without a reply
    // (the controller treats that as "no measurement").
    let deadline = tokio::time::sleep(Duration::from_secs(args.hold_secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            accepted = session.accept() => match accepted {
                Ok(stream) => {
                    let _ = respond(stream, |_cmd: WorkCmd| async { None::<RegisterResp> }).await;
                }
                Err(e) => bail!("session ended: {e}"),
            },
            _ = &mut deadline => break,
        }
    }
    session.close("hold elapsed");
    Ok(())
}
