// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use slog::error;
use slog::info;
use slog::warn;
use tokio::process::Command;

use crate::beacon::ContactReport;
use crate::beacon::NetworkReport;
use crate::beacon::SledReport;
use crate::Global;
use crate::SledPair;
use crate::SledPairNew;
use crate::SwitchId;

pub const MAX_SLEDS: usize = 32;

// How frequently do we reevaluate inter-sled connectivity?
#[cfg(debug_assertions)]
const SWITCH_PERIOD: Duration = Duration::from_secs(2);
#[cfg(not(debug_assertions))]
const SWITCH_PERIOD: Duration = Duration::from_secs(10);

// Since the inter-sled beacons are all sent asyncronously, as are the beacons
// to the switches, it is totally normal for there to be a discrepancy of 1 or 2
// between reports of packets sent from once sled and received by the other.  We
// also want to allow for the possibility that some UDP traffic is dropped as
// part of normal traffic management.  The SLED_BEACON_TOLERANCE indicates how
// much of a delta we will allow before deciding that something has gone wrong.
// Because we would rather be right slowly than be wrong quickly, we have chosen
// to be _very_ conservative here.
const SLED_BEACON_TOLERANCE: u32 = 15; // apparently lost packets

// If we don't hear from a sled for a period of time, we treat it as if it were
// offline.  It may well be the case that it is communicating with other sleds,
// and we can infer from their reports that it is still alive.  However, if we
// aren't seeing the beacons we know that something is wrong.  Failure to
// communicate with the switch is not a symptom of the pumpkin problem.  Indeed,
// switch communication happens much earlier in the p4 pipeline than sled<->sled
// communication.
#[cfg(debug_assertions)]
const SWITCH_BEACON_TOLERANCE: Duration = Duration::from_secs(10);
#[cfg(not(debug_assertions))]
const SWITCH_BEACON_TOLERANCE: Duration = Duration::from_secs(60);

// We are using crude heuristics to diagnose a poorly understood problem, and
// thus need to be wary of false positives causing more trouble than we are
// attempting to fix.  Accordingly, we will wait at least this long between dpd
// restarts:
#[cfg(debug_assertions)]
const INTER_RESET_DELTA: Duration = Duration::from_secs(2 * 60);
#[cfg(not(debug_assertions))]
const INTER_RESET_DELTA: Duration = Duration::from_secs(10 * 60);

// If we keep diagnosing the problem and our attempts to fix it don't stick,
// then we assume we are misdiagnosing the problem and just give up.
const MAX_RESETS_ALLOWED: u32 = 5;

/// Internal state maintained by a switch monitor
pub struct SwitchState {
    /// Names of all sleds currently sending us beacons
    live_sleds: HashSet<String>,
    /// Summary of the sled<->sled connectivity
    connections: BTreeMap<SledPair, Summary>,
    /// Sled pairs exhibiting connectivity failures only through this switch
    pumpkin_pairs: HashSet<SledPair>,
    /// Last time we reset dpd
    last_reset: Instant,
    /// Number of times we have reset dpd
    reset_count: u32,
}

impl Default for SwitchState {
    fn default() -> SwitchState {
        SwitchState {
            live_sleds: HashSet::new(),
            connections: BTreeMap::new(),
            pumpkin_pairs: HashSet::new(),
            last_reset: Instant::now(),
            reset_count: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SpIdentifier {
    #[serde(rename = "type")]
    pub _type: String,
    pub slot: u32,
}

// Fetch the switch ID from MGS.  This call will spin until it succeeds, or if
// we get an illegal switch ID from the SP.
pub async fn fetch_id(log: &slog::Logger) -> anyhow::Result<SwitchId> {
    info!(log, "Fetching switch ID from MGS");

    let mut delay = 1;
    loop {
        match reqwest::get("http://localhost:12225/local/switch-id").await {
            Ok(r) => match r.text().await {
                Ok(text) => match serde_json::from_str::<SpIdentifier>(&text) {
                    Ok(spid) => {
                        return SwitchId::try_from(spid.slot).map_err(|e| {
                            anyhow::anyhow!(
                                "converting spid {spid:?} to SwitchId: {e:?}"
                            )
                        });
                    }
                    Err(e) => warn!(log, "failed to parse SpIdentifier: {e:?}"),
                },
                Err(_) => {
                    warn!(log, "failed to extract text from MGS response")
                }
            },
            Err(_) => warn!(log, "failed to fetch ID from MGS"),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        if delay < 8 {
            delay *= 2;
        }
    }
}

// When evaluating the connectivity over a specific link, we can find no
// traffic, traffic in both directions, or traffic in either direction.  We use
// "beacons received by a sled" as our indication of connectivity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Connectivity {
    // One or both sleds are missing an address on this network
    Offline,
    // Beacons are not being received by either sled
    NoTraffic,
    // Beacons are being received by one of the two sleds
    OneWay,
    // Beacons are being received by both sleds
    TwoWay,
}

// There are up to 4 links between each sled:
//     (switch0, switch1) * (underlay, bootstrap)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Summary {
    // per-switch connectivity on the bootstrap network
    bootstrap: [Connectivity; 2],
    // per-switch connectivity on the underlay network
    underlay: [Connectivity; 2],
}

// Given the last beacon ID sent from sled a and the last beacon received by
// sled b, should we treat this as a live connection or not?
fn considered_alive(last_sent: u32, last_received: u32) -> bool {
    // If the last packet sent is less than the last packet received, that
    // suggests that we have an active connection, and the reports from the
    // sending sled are arriving after the reports from the receiving sled.
    //
    // Alternatively, it could suggest that one of the pumpkin daemons (and
    // possibly the whole sled) restarted. TODO: If a sled restarts and then
    // immediately turns into a pumpkin, this may prevent us from detecting it.
    if last_sent < last_received {
        true
    } else {
        (last_sent - last_received) < SLED_BEACON_TOLERANCE
    }
}

// Determine connectivity across one switch/network pair.
//
// By comparing the last_seen and last_sent packet IDs, we can tell whether one
// sled's beacons are reaching the other sled or if they are disappearing
// somewhere en route.  By looking at whether packets are successfully being
// sent from a to b and/or from b to a, we can determine whether we have traffic
// moving in both directions, only in one direction, or if no traffic is moving
// between this pair of sleds.
fn compare_contacts(a: &ContactReport, b: &ContactReport) -> Connectivity {
    let a_to_b = considered_alive(a.last_sent, b.last_seen);
    let b_to_a = considered_alive(b.last_sent, a.last_seen);

    if a.last_seen == 0 && b.last_seen == 0 {
        Connectivity::NoTraffic
    } else if a_to_b && b_to_a {
        Connectivity::TwoWay
    } else if a_to_b || b_to_a {
        Connectivity::OneWay
    } else {
        Connectivity::NoTraffic
    }
}

// Compare the per-network reports from two sleds and determine the connectivity
// across both switches.
fn compare_networks(
    log: &slog::Logger,
    network: &str,
    namea: &str,
    reporta: &Option<NetworkReport>,
    nameb: &str,
    reportb: &Option<NetworkReport>,
) -> [Connectivity; 2] {
    // We would expect the two sleds to have endpoints on the same network. If
    // there is a mismatch, it could indicate that one of the sleds rebooted and
    // hasn't yet established anything other than its link-local address.
    // Regardless of the reason, it's worth noting here, but it isn't likely to
    // indicate a switch-level issue.
    if let (Some(a), Some(b)) = (reporta, reportb) {
        [
            compare_contacts(&a.contacts[0], &b.contacts[0]),
            compare_contacts(&a.contacts[1], &b.contacts[1]),
        ]
    } else {
        if reporta.is_none() && reportb.is_none() {
            // No log message as this is normal
        } else if reporta.is_none() {
            info!(log, "missing address on {network} network"; "sled" => namea);
        } else if reportb.is_none() {
            info!(log, "missing address on {network} network"; "sled" => nameb);
        }
        [Connectivity::Offline, Connectivity::Offline]
    }
}

fn compare_reports(
    log: &slog::Logger,
    namea: &str,
    reporta: &SledReport,
    nameb: &str,
    reportb: &SledReport,
) -> Summary {
    Summary {
        bootstrap: compare_networks(
            log,
            "bootstrap",
            namea,
            &reporta.bootstrap,
            nameb,
            &reportb.bootstrap,
        ),
        underlay: compare_networks(
            log,
            "underlay",
            namea,
            &reporta.underlay,
            nameb,
            &reportb.underlay,
        ),
    }
}

fn live_sleds(g: &Global) -> HashSet<String> {
    let switch_id = g.switch_id.unwrap() as usize;
    g.sleds
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, contacts)| {
            !contacts.link_local.contacts[switch_id]
                .last_contact
                .is_older(SWITCH_BEACON_TOLERANCE)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn summarize(
    g: &Global,
    log: &slog::Logger,
    sleds: &HashSet<String>,
) -> BTreeMap<SledPair, Summary> {
    let mut cs = BTreeMap::new();

    let m = g.matrix.lock().unwrap();
    for a in sleds {
        for b in sleds {
            // The bidirectional summary of inter-sled connectivity will be
            // symmetric, so we only look at the variant with the lower-named
            // sled first.
            if b <= a {
                continue;
            }
            let a_to_b = SledPair::new(a, b);
            let b_to_a = SledPair::new(b, a);
            if let (Some(repa), Some(repb)) = (m.get(&a_to_b), m.get(&b_to_a)) {
                cs.insert(a_to_b, compare_reports(log, a, repa, b, repb));
            }
        }
    }
    cs
}

fn update(g: &Global, log: &slog::Logger) {
    // Find all of the sleds that are sending us regular beacons.
    let sleds = live_sleds(g);

    // Build an updated map of current inter-sled connectivity
    let mut cs = summarize(g, log, &sleds);

    // Log any changes in the sled population
    let mut ss = g.switch_state.lock().unwrap();
    for sled in sleds.difference(&ss.live_sleds) {
        info!(log, "new sled discovered"; "sled" => sled);
    }
    for sled in ss.live_sleds.difference(&sleds) {
        info!(log, "sled disappeared"; "sled" => sled);
    }
    ss.live_sleds = sleds;

    // Iterate over all of the previous connection records, looking for those
    // that have changed or disappeared.  Log any changes.
    let mut to_delete = HashSet::<SledPair>::new();
    for (pair, old) in ss.connections.iter_mut() {
        if let Some(new) = cs.remove(pair) {
            if *old != new {
                info!(log, "updated connection state: {pair:?} => {new:?}");
                *old = new;
            }
        } else {
            info!(log, "connection disappeared: {pair:?}");
            to_delete.insert(pair.clone());
        };
    }

    for (a, b) in to_delete {
        ss.connections.remove(&(a, b));
    }

    // All of the entries remaining in 'cs' are connections that didn't appear
    // in the old records
    for (pair, new) in cs {
        info!(log, "new connection found: {pair:?} => {new:?}");
        ss.connections.insert(pair, new);
    }
}

// Execute a shell command
async fn exec(log: &slog::Logger, cmd: &str) {
    info!(log, "executing: {cmd}");
    let mut f: Vec<&str> = cmd.split(' ').collect();
    let cmd = f.remove(0);
    match Command::new(cmd).args(&f).output().await {
        Ok(out) => match out.status.success() {
            true => info!(log, "\n{}", String::from_utf8_lossy(&out.stdout)),
            false => error!(
                log,
                "{} failed: {}",
                cmd,
                String::from_utf8_lossy(&out.stderr)
            ),
        },
        Err(e) => {
            error!(log, "failed to execute {cmd}: {e:?}");
        }
    }
}

pub async fn record_diags(log: &slog::Logger) {
    let mut cmds = vec![
        "/opt/oxide/dendrite/bin/swadm link ls".to_string(),
        "/opt/oxide/dendrite/bin/swadm arp list".to_string(),
        "/opt/oxide/dendrite/bin/swadm route ls".to_string(),
        "/opt/oxide/dendrite/bin/swadm addr ls".to_string(),
        "/opt/oxide/dendrite/bin/swadm nat ls".to_string(),
        "/opt/oxide/dendrite/bin/swadm sp xcvr ls".to_string(),
    ];

    let counters = vec![
        "Service",
        "Ingress",
        "Egress",
        "Packet",
        "Drop_Port",
        "Drop_Reason",
    ];

    let tables = vec![
        "pipe.Ingress.l3_router.Router4.lookup_idx.lookup",
        "pipe.Ingress.l3_router.Router4.lookup_idx.route",
        "pipe.Ingress.l3_router.Router6.lookup.tbl",
        "pipe.Ingress.l3_router.Router4.Arp.tbl",
        "pipe.Ingress.l3_router.Router6.Ndp.tbl",
        "pipe.Ingress.mac_rewrite.mac_rewrite",
        "pipe.Ingress.filter.switch_ipv4_addr",
        "pipe.Ingress.filter.switch_ipv6_addr",
        "pipe.Ingress.nat_ingress.ingress_ipv4",
        "pipe.Ingress.nat_ingress.ingress_ipv6",
        "pipe.Ingress.nat_ingress.nat_only",
    ];

    for ctr in counters {
        cmds.push(format!("/opt/oxide/dendrite/bin/swadm counters get {ctr}"))
    }
    for tbl in tables {
        cmds.push(format!("/opt/oxide/dendrite/bin/swadm table dump {tbl}"))
    }

    for c in cmds {
        exec(log, &c).await;
    }
}

async fn reset_dpd(log: &slog::Logger) {
    exec(log, "/usr/sbin/svcadm restart dendrite").await;
}

async fn evaluate(g: &Global, log: &slog::Logger) -> bool {
    let mut ss = g.switch_state.lock().unwrap();

    // Which of the two per-switch connectivity slots contains status for
    // traffic transiting this switch?
    let us = usize::from(g.switch_id.unwrap());
    let them = 1 - us;

    // Are we currently in window after resetting dpd in which we don't want to
    // do it again?
    let in_reset_window = Instant::now() - ss.last_reset < INTER_RESET_DELTA;

    let mut new_failures = HashSet::new();
    let mut cleared = HashSet::new();

    for (pair, s) in &ss.connections {
        // Once a pair has been marked as failing, it stays in that state
        // until we see it moving traffic through this switch or it goes
        // offline.
        if ss.pumpkin_pairs.contains(pair) {
            if (s.bootstrap[us] == Connectivity::TwoWay
                || s.bootstrap[us] == Connectivity::Offline)
                && (s.underlay[us] == Connectivity::TwoWay
                    || s.underlay[us] == Connectivity::Offline)
            {
                info!(log, "clearing failure indication for {pair:?}");
                cleared.insert(pair.clone());
            }
            continue;
        }

        // If we are in the reset window, we don't add new pairs to the
        // pumpkin list.  If the pair is still broken when the reset window
        // expires, we will take action then.
        if in_reset_window {
            continue;
        }

        let mut failed = false;
        if s.bootstrap[them] == Connectivity::TwoWay
            && s.bootstrap[us] == Connectivity::NoTraffic
        {
            warn!(log, "traffic stopped on bootstrap network: {pair:?}");
            failed = true;
        }
        if s.underlay[them] == Connectivity::TwoWay
            && s.underlay[us] == Connectivity::NoTraffic
        {
            warn!(log, "traffic stopped on underlay network: {pair:?}");
            failed = true;
        }
        // An aggressive approach would have us declare a pair of sleds failed
        // if either of the two networks were blocked.  We're choosing to be
        // conservative, so we will declare a pair of sleds to be functional if
        // at least one network is functional.
        if s.bootstrap[us] == Connectivity::TwoWay
            || s.underlay[us] == Connectivity::TwoWay
        {
            failed = false;
        }

        if failed {
            new_failures.insert(pair.clone());
        }
    }

    let reset_needed =
        !new_failures.is_empty() && ss.reset_count < MAX_RESETS_ALLOWED;

    for pair in cleared {
        ss.pumpkin_pairs.remove(&pair);
    }
    for pair in new_failures {
        ss.pumpkin_pairs.insert(pair);
    }

    // XXX: How should we handle a non-empty pumpkin list when the reset window
    // expires?  Anything on there is pair that was broken before the dpd
    // reset, and which hasn't recovered as a result of the reset.  We could
    // try resetting again or we could leave them marked as broken and not reset
    // unless another pair breaks. For now, we will not reset unless we find a
    // new failure.
    if reset_needed {
        ss.reset_count += 1;
        ss.last_reset = Instant::now();
        true
    } else {
        false
    }
}

pub async fn switch_loop(
    g: Arc<Global>,
    log_only: bool,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) -> anyhow::Result<()> {
    let log = g.log.new(slog::o!("unit" => "switch"));
    info!(log, "switch loop started");

    loop {
        #[rustfmt::skip]
	tokio::select! {
	    _ = shutdown_rx.changed() => break,
	    _ = tokio::time::sleep(SWITCH_PERIOD) => { },
	};

        update(&g, &log);
        if evaluate(&g, &log).await {
            warn!(log, "pumpkin suspected - capturing diagnostics");
            record_diags(&log).await;

            if !log_only {
                warn!(log, "resetting");
                reset_dpd(&log).await;
            }
        }
    }

    info!(log, "switch loop done");
    Ok(())
}
