// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::net::IpAddr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::Mutex;
use std::vec;

use anyhow::anyhow;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use signal_hook::consts::signal::*;
use signal_hook::iterator::Signals;
use slog::debug;
use slog::error;
use slog::info;
use slog::Drain;

use common::SwitchId;

mod api_server;
mod beacon;
mod network;
mod switch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RunMode {
    Switch,
    Sled,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate an OpenAPI specification for the pumpkind server.
    Openapi,
    /// Run a pumpkind instance
    Run(Opts),
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Opts {
    /// Name of the node.  This will default to the hostname.
    #[arg(long)]
    name: Option<String>,

    /// Addresses on which to open API servers
    #[arg(long)]
    listen_addresses: Vec<IpAddr>,

    /// Number of the port to open an API server.  This is also the port on
    /// which our own server will run.
    #[arg(short = 'a', long)]
    api_port: Option<u16>,

    /// Mode in which to run
    #[arg(short = 'm', long, value_enum)]
    mode: RunMode,

    /// Switch ID to assume when running in switch mode
    #[arg(short = 's', long)]
    switch_id: Option<u8>,

    /// For testing, set these node/switch endpoints to enable as pumpkins at
    /// startup
    #[arg(short = 'p', long)]
    pumpkins: Vec<String>,
    /// Don't reset dpd on failure detection - just log it.
    /// Note: this option is currently ignored, as all resets are suppressed.
    #[arg(long)]
    log_only: bool,
}

/// Data shared by multiple tasks
pub struct Global {
    /// The name of this sled/switch
    pub name: String,
    /// The root of the logger tree
    pub log: slog::Logger,
    /// When run in switch mode, this contains the ID of this switch.
    pub switch_id: Option<SwitchId>,
    /// This sled/switch's contact data for each of the other sleds in the rack
    pub sleds: Mutex<BTreeMap<String, beacon::SledContacts>>,
    /// This sled's contact data for the two switches in the rack
    pub switches: Mutex<[beacon::ContactData; 2]>,
    /// The full set of sled<->sled contact data
    pub matrix: Mutex<BTreeMap<SledPair, beacon::SledReport>>,
    /// The working state maintained on a switch
    pub switch_state: Mutex<switch::SwitchState>,
    /// The node/pairs that are runnning in pumpkin mode
    pub pumpkins: Mutex<HashSet<(String, SwitchId)>>,
}

pub type SledPair = (String, String);
trait SledPairNew {
    fn new(a: impl ToString, b: impl ToString) -> Self;
}
impl SledPairNew for SledPair {
    fn new(a: impl ToString, b: impl ToString) -> Self {
        (a.to_string(), b.to_string())
    }
}

fn hostname() -> anyhow::Result<String> {
    const HOST_MAX: usize = 255;
    let mut buf = vec![0u8; HOST_MAX + 1];

    let result = unsafe {
        libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, HOST_MAX)
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    let first_null = buf
        .iter()
        .position(|b| *b == 0)
        .expect("hostname should be < HOST_MAX");
    let os_str = OsStr::from_bytes(&buf[..first_null]);
    Ok(os_str.to_string_lossy().into_owned())
}

pub fn log_init() -> anyhow::Result<slog::Logger> {
    // Ask slog to allocate a substantial buffer for incoming messages to
    // avoid blocking.  Note: this is unlikely to be needed on this
    // daemon, as the amount of data logged is likely to be small, and no
    // log-generating thread will block for long.  This conservative default
    // is carried over from other daemons with different behavior, but seems
    // harmless.
    let drain = slog_async::Async::new(
        slog_bunyan::with_name("pumpkind", std::io::stdout())
            .build()
            .fuse(),
    )
    .chan_size(32768)
    .build()
    .fuse();
    Ok(slog::Logger::root(drain, slog::o!()))
}

#[allow(unused_variables)]
fn signal_handler(g: Arc<Global>) {
    const SIGNALS: &[std::ffi::c_int] =
        &[SIGTERM, SIGQUIT, SIGINT, SIGHUP, SIGUSR2];
    let mut sigs = Signals::new(SIGNALS).unwrap();

    let log = g.log.new(slog::o!("unit" => "signal-handler"));
    for signal in &mut sigs {
        info!(&log, "caught signal {}", signal);
        // XXX: if/when I add SMF support, SIGUSR2 will be the signal to
        // refresh my config.
        if signal != SIGUSR2 {
            break;
        }
    }
}

struct Task {
    name: String,
    tx: tokio::sync::watch::Sender<()>,
    hdl: tokio::task::JoinHandle<anyhow::Result<()>>,
}

macro_rules! spawn {
    ($name:literal, $global:ident, $func:path, $arg:expr) => {{
        let (tx, rx) = tokio::sync::watch::channel(());
        let g = $global.clone();
        let hdl = tokio::task::spawn(async move { $func(g, $arg, rx).await });
        info!(&$global.log, "{} launched", $name);
        Task {
            name: $name.to_string(),
            tx,
            hdl,
        }
    }};
}

// Attempt to parse a command line argument like BRM42220036/1 into a (sled,
// SwitchId) pair.
fn pumpkin_parse(p: &str) -> anyhow::Result<(String, SwitchId)> {
    let f: Vec<&str> = p.split('/').collect();
    if f.len() != 2 {
        Err(anyhow!("Invalid node/switch pair: {p}"))
    } else {
        let node = f[0].to_string();
        let switch_id = str::parse::<u8>(f[1])
            .map_err(|_| anyhow!("invalid switch ID: {}", f[1]))?;
        let switch_id = switch_id
            .try_into()
            .map_err(|_| anyhow!("invalid switch ID: {}", switch_id))?;
        Ok((node, switch_id))
    }
}

async fn run_daemon(opts: &Opts) -> anyhow::Result<()> {
    let log = log_init()?;

    let name = match &opts.name {
        Some(s) => s.clone(),
        None => hostname()?,
    };

    let switch_id = match opts.mode {
        RunMode::Sled => None,
        RunMode::Switch => match opts.switch_id {
            Some(id) => Some(id.try_into().map_err(|e| anyhow!("{e:?}"))?),
            None => Some(switch::fetch_id(&log).await?),
        },
    };
    info!(log, "switch_id: {switch_id:?}");

    let api_port = opts.api_port.unwrap_or(common::DEFAULT_API_PORT);

    let mut pumpkins = HashSet::new();
    for p in opts.pumpkins.iter() {
        pumpkins.insert(pumpkin_parse(p)?);
    }

    let global = Arc::new(Global {
        name,
        log: log.clone(),
        switch_id,
        sleds: Mutex::new(BTreeMap::new()),
        switches: Mutex::new([beacon::ContactData::default(); 2]),
        matrix: Mutex::new(BTreeMap::new()),
        switch_state: Mutex::new(switch::SwitchState::default()),
        pumpkins: Mutex::new(pumpkins),
    });

    let mut tasks = Vec::new();
    tasks.push(spawn!(
        "api server",
        global,
        api_server::api_server,
        api_port
    ));
    let mode = opts.mode;
    tasks.push(spawn!("beacon loop", global, beacon::beacon_loop, mode));
    if opts.mode == RunMode::Switch {
        //let log_only = opts.log_only;
        let log_only = true;
        tasks.push(spawn!(
            "switch loop",
            global,
            switch::switch_loop,
            log_only
        ));
    }

    signal_handler(global.clone());

    for task in tasks {
        debug!(&log, "shutting down {}", task.name);
        _ = task.tx.send(());
        match task.hdl.await {
            Err(e) => {
                error!(&log, "error shutting down {}: {:?}", task.name, e)
            }
            Ok(r) => match r {
                Err(e) => {
                    error!(&log, "task {} reported error: {:?}", task.name, e)
                }
                Ok(_) => info!(&log, "task {} shut down", task.name),
            },
        }
    }

    info!(&log, "exiting");

    Ok(())
}

fn print_openapi() -> anyhow::Result<()> {
    crate::api_server::http_api()
        .openapi("Oxide Pumpkind Daemon", semver::Version::new(0, 1, 0))
        .description("API for managing the pumpkind daemon")
        .contact_url("https://oxide.computer")
        .contact_email("api@oxide.computer")
        .write(&mut std::io::stdout())
        .map_err(|e| e.into())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Openapi => print_openapi(),
        Command::Run(opts) => run_daemon(opts).await,
    }
}
