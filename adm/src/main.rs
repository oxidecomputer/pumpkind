// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::convert::TryFrom;
use std::io::stdout;
use std::io::Write;

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use colored::*;
use tabwriter::TabWriter;

use common::SwitchId;
use pumpkind_client::types;
use pumpkind_client::Client;

/// Tool to interact with pumpkind
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(long, help = "pumpkin daemon's hostname or IP address")]
    host: Option<String>,

    #[arg(help = "daemon's TCP port", long)]
    port: Option<u16>,

    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum PumpkinCmd {
    /// Enable pumpkin mode (i.e., stop sending beacons) to this sled over
    /// this switch.
    On { sled_name: String, switch: u8 },
    /// Disable pumpkin mode for this (sled, switch)
    Off { sled_name: String, switch: u8 },
    /// List the (sled, switch) pairs for which pumpkin mode is enabled
    #[command(visible_alias = "ls")]
    List,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Fetch the build infomation from the daemon
    #[command(visible_alias = "bi")]
    Buildinfo,
    /// Fetch this sled/switch's direct contact info for all sleds
    Contacts,
    /// For a sled, return the contact info for the two switches it is connected
    /// to
    #[command(visible_alias = "sw")]
    Switches,
    /// For the given sled, fetch its reported contact data for all other sleds
    #[command(visible_alias = "r")]
    Reports { sled_name: String },
    /// Debug command for manipulating the sled's pumpkin-like behavior
    #[clap(subcommand, visible_alias = "p")]
    Pumpkin(PumpkinCmd),

    /// Trigger pumpkind to capture a full set of diagnostics
    Diags,
}

fn age(age: u32) -> String {
    if age == u32::MAX {
        "never".to_string()
    } else {
        age.to_string()
    }
}

fn display_endpoint(
    tw: &mut TabWriter<std::io::Stdout>,
    name: &str,
    network: &str,
    ep: &Option<types::NetworkReport>,
) {
    if let Some(ep) = ep {
        for switch in 0..2 {
            writeln!(
                tw,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                name,
                network,
                switch,
                age(ep.contacts[switch].first_age),
                age(ep.contacts[switch].latest_age),
                ep.contacts[switch].beacons_sent,
                ep.contacts[switch].beacons_seen,
                ep.contacts[switch].beacons_missed,
            )
            .unwrap();
        }
    }
}

fn display_switch_contacts(
    tw: &mut TabWriter<std::io::Stdout>,
    switch: u8,
    data: &types::ContactReport,
) {
    writeln!(
        tw,
        "{}\t{}\t{}\t{}\t{}\t{}",
        switch,
        age(data.first_age),
        age(data.latest_age),
        data.beacons_sent,
        data.beacons_seen,
        data.beacons_missed,
    )
    .unwrap();
}

// Report the direct per-sled contact info for this sled/switch.  On a sled,
// this will return the beacon info over the bootstrap and underlay networks
// with each other sled.  On a switch, it will return the beacon traffic over
// the link-local connections between the switch and sleds it is directly
// connected to.
async fn contacts_cmd(client: &Client) -> anyhow::Result<()> {
    let sleds = client
        .get_sleds()
        .await
        .context("failed to get sled info")?
        .into_inner();
    let mut tw = TabWriter::new(stdout());
    writeln!(
        tw,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        "Sled".underline(),
        "Network".underline(),
        "Switch".underline(),
        "Oldest".underline(),
        "Latest".underline(),
        "Beacons Sent".underline(),
        "Beacons Rcvd".underline(),
        "Beacons Missed".underline(),
    )
    .unwrap();
    for sled in sleds {
        display_endpoint(&mut tw, &sled.name, "peer", &sled.link_local);
        display_endpoint(&mut tw, &sled.name, "bootstrap", &sled.bootstrap);
        display_endpoint(&mut tw, &sled.name, "underlay", &sled.underlay);
    }
    tw.flush().map_err(|e| e.into())
}

// When run on a sled, this will report on the beacon traffic to/from the two
// switches it should be connected to.  On a switch, this is a no-op.
async fn switches_cmd(client: &Client) -> anyhow::Result<()> {
    let switches = client
        .get_switches()
        .await
        .context("failed to get switches info")?
        .into_inner();
    let mut tw = TabWriter::new(stdout());
    writeln!(
        tw,
        "{}\t{}\t{}\t{}\t{}\t{}",
        "Switch".underline(),
        "Oldest".underline(),
        "Latest".underline(),
        "Beacons Sent".underline(),
        "Beacons Rcvd".underline(),
        "Beacons Missed".underline(),
    )
    .unwrap();
    for (switch, data) in switches.iter().enumerate() {
        display_switch_contacts(&mut tw, switch as u8, data);
    }
    tw.flush().map_err(|e| e.into())
}

// For the named sled, report all of the contact info that sled included in the
// last beacon received by the local pumpkind.
async fn reports_cmd(client: &Client, sled_name: &str) -> anyhow::Result<()> {
    let sleds = client
        .reports_get(sled_name)
        .await
        .context("failed to get sled info")?
        .into_inner();
    let mut tw = TabWriter::new(stdout());
    writeln!(
        tw,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        "Sled".underline(),
        "Network".underline(),
        "Switch".underline(),
        "Oldest".underline(),
        "Latest".underline(),
        "Beacons Sent".underline(),
        "Beacons Rcvd".underline(),
        "Beacons Missed".underline(),
    )
    .unwrap();
    for sled in sleds {
        display_endpoint(&mut tw, &sled.name, "peer", &sled.link_local);
        display_endpoint(&mut tw, &sled.name, "bootstrap", &sled.bootstrap);
        display_endpoint(&mut tw, &sled.name, "underlay", &sled.underlay);
    }
    tw.flush().map_err(|e| e.into())
}

async fn pumpkin_cmd(client: &Client, cmd: PumpkinCmd) -> anyhow::Result<()> {
    match cmd {
        PumpkinCmd::On { sled_name, switch } => {
            let switch: SwitchId = switch.try_into().map_err(|e| {
                anyhow::anyhow!("failed to convert to switch: {e:?}")
            })?;
            client.pumpkin_on(&sled_name, &switch).await?;
        }
        PumpkinCmd::Off { sled_name, switch } => {
            let switch =
                SwitchId::try_from(switch).map_err(|e| anyhow::anyhow!(e))?;
            client.pumpkin_off(&sled_name, &switch).await?;
        }
        PumpkinCmd::List => {}
    }
    Ok(())
}

async fn diags(client: &Client) -> anyhow::Result<()> {
    client.trigger_diagnostics().await?;
    Ok(())
}

async fn build_info(client: &Client) -> anyhow::Result<()> {
    let info = client
        .build_info()
        .await
        .context("failed to get build information")?
        .into_inner();
    println!("Version: {}", info.version);
    println!("Commit SHA: {}", info.git_sha);
    println!("Commit timestamp: {}", info.git_commit_timestamp);
    println!("Git branch: {}", info.git_branch);
    println!("Rustc version: {}", info.rustc_semver);
    println!("Rustc channel: {}", info.rustc_channel);
    println!("Rustc triple: {}", info.rustc_host_triple);
    println!("Rustc commit SHA: {}", info.rustc_commit_sha);
    println!("Cargo triple: {}", info.cargo_triple);
    println!("Debug: {}", info.debug);
    println!("Opt level: {}", info.opt_level);
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let port = cli.port.unwrap_or(common::DEFAULT_API_PORT);
    let host = cli.host.unwrap_or("localhost".to_string());
    let log = slog::Logger::root(slog::Discard, slog::o!());

    let api_log = log.new(slog::o!("unit" => "api"));
    let client = Client::new(&format!("http://{host}:{port}"), api_log);

    match cli.cmd {
        Command::Buildinfo => build_info(&client).await,
        Command::Contacts => contacts_cmd(&client).await,
        Command::Switches => switches_cmd(&client).await,
        Command::Reports { sled_name } => {
            reports_cmd(&client, &sled_name).await
        }
        Command::Pumpkin(cmd) => pumpkin_cmd(&client, cmd).await,
        Command::Diags => diags(&client).await,
    }
}
