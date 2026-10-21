//! `stormboot-serve`: boot nodes from one image on this machine (#474).
//!
//! Serves a golden's disk image (or an ISO) to named nodes over NVMe/TCP,
//! read-only, each node writing into its own copy-on-write overlay, and
//! answers the boothost intent and claim calls stormbootx and the initramfs
//! make. No forge, no kernel module, no administrator rights beyond opening
//! two ports: runs on macOS, Windows and Linux.
//!
//! ```text
//! stormboot-serve --image release.img --host server8
//! ```
//!
//! A node's stormbootx media names this machine as its `portal` (and
//! `name = server8`); the node claims, attaches and installs.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use stormblock_nvmeof::boothost::{Boothost, Intent, Policy};
use stormblock_nvmeof::image::Image;
use stormblock_nvmeof::server::{host_nqn, BootTarget};

#[derive(Parser, Debug)]
#[command(name = "stormboot-serve", version, about = "Serve one boot image to nodes over NVMe/TCP, with the boothost claim API")]
struct Args {
    /// The image: a golden's raw disk, or an ISO. Opened read-only.
    #[arg(long)]
    image: PathBuf,
    /// A node allowed to boot it, by the name it claims as; `name=MAC` also
    /// matches the machine's MAC-keyed claims. Repeat for several.
    #[arg(long = "host", value_name = "NAME[=MAC]")]
    hosts: Vec<String>,
    /// Serve a claim for any name (a bench of machines not named in advance).
    #[arg(long)]
    any_host: bool,
    /// What a node is told to do on its next boot: install, local or auto.
    /// An install that reports its disk laid moves the node to local.
    #[arg(long, default_value = "install")]
    intent: String,
    /// The address to listen on.
    #[arg(long, default_value = "0.0.0.0")]
    listen: IpAddr,
    /// The NVMe/TCP port.
    #[arg(long, default_value_t = 4420)]
    nvme_port: u16,
    /// The boothost API's port (stormbootx's `api_port`).
    #[arg(long, default_value_t = 9090)]
    http_port: u16,
    /// The address handed to nodes in a claim. Default: the one each node's
    /// request reached.
    #[arg(long)]
    advertise: Option<IpAddr>,
    /// Where each node's overlay is kept (removed when it is let go).
    #[arg(long)]
    scratch: Option<PathBuf>,
    /// The image's block size: 4096 (what releases are built at) or 512.
    #[arg(long, default_value_t = 4096)]
    block_size: u32,
    /// The subsystem names' prefix; each node's is `<nqn>:host:<name>`.
    #[arg(long, default_value = "nqn.2026-10.lo.storm:stormboot-serve")]
    nqn: String,
    /// The largest transfer one command may carry, in bytes.
    #[arg(long, default_value_t = 1 << 20)]
    max_transfer: usize,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("stormboot-serve: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stormboot-serve: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let intent = Intent::parse(&args.intent).ok_or_else(|| format!("--intent {}: install, local or auto", args.intent))?;
    if args.hosts.is_empty() && !args.any_host {
        return Err("name the nodes with --host NAME (or serve any with --any-host)".into());
    }
    let hosts: Vec<(String, Option<String>)> = args
        .hosts
        .iter()
        .map(|h| match h.split_once('=') {
            Some((n, m)) => (n.trim().to_string(), Some(m.trim().to_string())),
            None => (h.trim().to_string(), None),
        })
        .collect();
    if let Some((bad, _)) = hosts.iter().find(|(n, _)| n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')) {
        return Err(format!("--host {bad:?}: a name is letters, digits, '-' and '.'"));
    }
    let image = Arc::new(Image::open(&args.image, args.block_size).map_err(|e| format!("{}: {e}", args.image.display()))?);
    let scratch = args.scratch.unwrap_or_else(|| std::env::temp_dir().join("stormboot-serve"));
    let target = BootTarget::new(image.clone(), scratch.clone(), &args.nqn, args.max_transfer);

    let nvme = tokio::net::TcpListener::bind(SocketAddr::new(args.listen, args.nvme_port))
        .await
        .map_err(|e| format!("NVMe/TCP on {}:{}: {e}", args.listen, args.nvme_port))?;
    let http = tokio::net::TcpListener::bind(SocketAddr::new(args.listen, args.http_port))
        .await
        .map_err(|e| format!("the boothost API on {}:{}: {e}", args.listen, args.http_port))?;

    println!("stormboot-serve {}", env!("CARGO_PKG_VERSION"));
    println!("  image    : {} ({} bytes, {} blocks of {})", image.path().display(), image.len(), image.blocks(), image.block_size());
    println!("  nvme/tcp : {}:{}", args.listen, args.nvme_port);
    println!("  boothost : http://{}:{}", args.listen, args.http_port);
    println!("  overlays : {}", scratch.display());
    for (n, mac) in &hosts {
        println!(
            "  host     : {n}{} connects as {}, intent {}",
            mac.as_deref().map(|m| format!(" ({m})")).unwrap_or_default(),
            host_nqn(n),
            intent.as_str()
        );
    }
    if args.any_host {
        println!("  host     : any name that claims, intent {}", intent.as_str());
    }
    println!("A node's stormbootx media names this machine as its portal (api_port {}).", args.http_port);

    let boot = Boothost::new(
        target.clone(),
        Policy { hosts, any_host: args.any_host, intent, nvme_port: args.nvme_port, advertise: args.advertise },
    );
    let app = boot.router().into_make_service_with_connect_info::<SocketAddr>();
    let serve_http = async move { axum::serve(http, app).await.map_err(|e| format!("the boothost API: {e}")) };
    let held = target.clone();
    let serve_nvme = async move { target.run(nvme).await.map_err(|e| format!("NVMe/TCP: {e}")) };
    tokio::select! {
        r = serve_http => r,
        r = serve_nvme => r,
        _ = tokio::signal::ctrl_c() => {
            held.release_all();
            println!("stopped; the overlays are removed");
            Ok(())
        }
    }
}
