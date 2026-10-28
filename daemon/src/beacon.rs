// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::cmp;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::convert::Into;
use std::convert::TryFrom;
use std::fmt;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use network_interface::NetworkInterfaceConfig;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use slog::error;
use slog::info;
use slog::warn;
use tokio::net::UdpSocket;

use crate::network;
use crate::switch;
use crate::Global;
use crate::RunMode;
use crate::SledPair;
use crate::SledPairNew;
use crate::SwitchId;

/// Size of the buffer to allocate for incoming beacons
const MAX_BUF_SIZE: usize = 4096;

// How frequently do we send bacons
#[cfg(debug_assertions)]
const BEACON_PERIOD: Duration = Duration::from_secs(2);
#[cfg(not(debug_assertions))]
const BEACON_PERIOD: Duration = Duration::from_secs(10);

const MCAST_ADDR: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x7718);
const UNICAST_PORT: u16 = 12217;
const MULTICAST_PORT: u16 = 12218;

// Time management is a bit tricky, since these reports may span ntp
// initialization or adjustment as the system comes online.  It gets even dicier
// in the presence of sleds being added on the fly.  Our approach is to rely on
// Rust's time::Instant to maintain accuracy across internal ntp adjustments.
// To deal with inter-sled issues, we communicate using Ages rather than
// timestamps, and make the simplifying assumption that the time delta between a
// packet's construction on one sled and receipt on another sled is 0.  Given
// the scale of the measurements we care about (many seconds), the inaccuracy
// introduced by this assumption shouldn't impact our decisions.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Age {
    time: Option<Instant>,
}

impl fmt::Debug for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.age())
    }
}

impl cmp::PartialOrd for Age {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl cmp::Ord for Age {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        other.time.cmp(&self.time)
    }
}

#[test]
fn test_age() {
    let older = Age::from_secs(5u32);
    let younger = Age::from_secs(2u32);
    assert!(older > younger);
    assert!(younger < older);
}

#[test]
fn test_0_age() {
    let older = Age::from_secs(0u32);
    std::thread::sleep(Duration::from_millis(10));
    let younger = Age::from_secs(0u32);
    assert!(older > younger);
    assert!(younger < older);
}

#[test]
fn test_older() {
    let never = Age::default();
    let age = Age::from_secs(10u32);
    let younger = Duration::from_secs(5);
    let older = Duration::from_secs(15);

    // An Age of 10 seconds is older than 5 seconds
    assert!(age.is_older(younger));
    // An Age of 10 seconds is not older than 15 seconds
    assert!(!age.is_older(older));

    // Never is older than everything
    assert!(never.is_older(younger));
}

impl Age {
    /// Construct an `Age` representing an event that happened `secs`
    /// seconds ago.
    pub fn from_secs(secs: impl Into<u64>) -> Age {
        let age = Duration::from_secs(secs.into());
        let time = Instant::now() - age;
        Age { time: Some(time) }
    }

    /// Test if the Age is older than the Duration.  If the Age is "never", it
    /// is older than everything.
    pub fn is_older(&self, d: Duration) -> bool {
        match self.age() {
            Some(t) => t > d,
            None => true,
        }
    }

    fn age(&self) -> Option<Duration> {
        self.time.map(|t| Instant::now() - t)
    }
}

impl From<Age> for u32 {
    fn from(a: Age) -> u32 {
        match a.age() {
            Some(a) => a.as_secs() as u32,
            None => u32::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct NetInfo {
    addr: Ipv6Addr,
    ifindex: u32,
    network: TargetNetwork,
}

type LocalMap = BTreeMap<String, Option<NetInfo>>;
type PeerMap = BTreeMap<String, Peer>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetNetwork {
    Bootstrap,
    Underlay,
    LinkLocal,
}

impl fmt::Display for TargetNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetNetwork::Bootstrap => write!(f, "bootstrap"),
            TargetNetwork::Underlay => write!(f, "underlay"),
            TargetNetwork::LinkLocal => write!(f, "link local"),
        }
    }
}

/// There are a set of well known names and constants for each network or
/// addrobj of interest. Building these into the utility is potentially brittle,
/// but seems like a reasonable approach given the limited scope of the project.
///
/// In particular, the underlay network of the future may expand to include any
/// address in the `0xfdxx::` space other than that reserved for the bootstrap
/// network.
const UNDERLAY_NAME: &str = "underlay0";
const BOOTSTRAP_NAME: &str = "bootstrap0";

const BOOTSTRAP_SEG: u16 = 0xfdb0;
const UNDERLAY_SEG: u16 = 0xfd00;
const LINK_LOCAL_SEG: u16 = 0xfe80;

impl TryFrom<Ipv6Addr> for TargetNetwork {
    type Error = &'static str;

    fn try_from(addr: Ipv6Addr) -> Result<Self, Self::Error> {
        match addr.segments()[0] {
            BOOTSTRAP_SEG => Ok(TargetNetwork::Bootstrap),
            UNDERLAY_SEG => Ok(TargetNetwork::Underlay),
            LINK_LOCAL_SEG => Ok(TargetNetwork::LinkLocal),
            _ => Err("unrecognized segment"),
        }
    }
}

/// The ContactData struct contains our internal tracking of incoming beacons
/// from a single address on a remote node to a specific local interface.  On a
/// fully established rack, each sled can have 4 endpoints for each other sled,
/// representing the links on the underlay and bootstrap networks, each
/// connected through both sidecars.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContactData {
    pub first_contact: Age,
    pub last_contact: Age,
    pub last_sent: u32,
    pub last_seen: u32,
    pub beacons_sent: u32,
    pub beacons_seen: u32,
    pub beacons_missed: u32,
}

impl ContactData {
    fn update_from_beacon(&mut self, log: &slog::Logger, beacon: &BeaconData) {
        if self.beacons_seen == 0 {
            self.first_contact = Age::from_secs(0u64);
            self.last_contact = self.first_contact;
        } else {
            self.last_contact = Age::from_secs(0u64);
            if self.last_seen == beacon.pkt_id {
                warn!(log, "Beacon {} seen multiple times", beacon.pkt_id);
            } else if self.last_seen > beacon.pkt_id {
                warn!(
                    log,
                    "Beacon {} seen after beacon {}",
                    beacon.pkt_id,
                    self.last_seen
                );
            } else if self.last_seen + 1 != beacon.pkt_id {
                let missed = beacon.pkt_id - self.last_seen - 1;
                warn!(log, "Missed {} beacons", missed);
                self.beacons_missed += missed
            }
        }
        self.last_seen = beacon.pkt_id;
        self.beacons_seen += 1;
    }
}

/// The Peer structure represents the two sides of a directly connected pair of
/// interfaces.  That is, it represents a connection between a sled and a
/// sidecar.
#[derive(Clone, Debug)]
struct Peer {
    switch: SwitchId,
    info: Option<NetInfo>,
}

impl Peer {
    pub fn new(switch: SwitchId) -> Self {
        Peer { switch, info: None }
    }
}

/// This represents an IPv6 address on either the bootstrap or underlay
/// networks.  Each endpoint has a single address that is available via two
/// different switches.
#[derive(Clone, Debug, Default)]
pub struct NetworkEndpoint {
    pub address: Option<Ipv6Addr>,
    pub contacts: [ContactData; 2],
}

/// This represents a single sled in the rack.  Each sled has a name and a
/// presence on two different networks.
#[derive(Clone, Debug)]
pub struct SledContacts {
    // The name of the sled sending the beacons.  This is redundant, as the
    // name is used in the BTree to find this record, but it allows us to
    // implement From for SledRecord easily.
    pub name: String,
    // Contact data over the link-local connection to the switch
    pub link_local: NetworkEndpoint,
    // Contact data over the bootstrap network
    pub bootstrap: NetworkEndpoint,
    // Contact data over the underlay network
    pub underlay: NetworkEndpoint,
}

impl SledContacts {
    fn new(name: impl ToString) -> Self {
        SledContacts {
            name: name.to_string(),
            link_local: Default::default(),
            bootstrap: Default::default(),
            underlay: Default::default(),
        }
    }

    // Create a new SledContact based on data in a beacon packet
    fn new_from_beacon(
        log: &slog::Logger,
        address: Ipv6Addr,
        beacon: &BeaconData,
    ) -> Self {
        let mut c = SledContacts::new(&beacon.name);
        c.update_from_beacon(log, address, beacon);
        c
    }

    // Create a new SledContact based on data inferred from another sled's
    // observations.  In practice this means that we know that a sled exists
    // with a specific IPv6 address, but nothing beyond that.
    fn new_inferred(name: &str, address: Ipv6Addr) -> Self {
        let mut c = SledContacts::new(name);
        c.update_inferred(address);
        c
    }

    // Update our local notion of a sled's status based on information
    // received via beacon from another sled or switch.
    fn update_from_beacon(
        &mut self,
        log: &slog::Logger,
        address: Ipv6Addr,
        beacon: &BeaconData,
    ) {
        let endpoint = match address.try_into() {
            Ok(TargetNetwork::LinkLocal) => &mut self.link_local,
            Ok(TargetNetwork::Bootstrap) => &mut self.bootstrap,
            Ok(TargetNetwork::Underlay) => &mut self.underlay,
            Err(_) => {
                warn!(
                    log,
                    "beacon received on unrecognized network: {address}"
                );
                return;
            }
        };

        if endpoint.address.is_none() {
            endpoint.address = Some(address);
        } else if endpoint.address != Some(address) {
            warn!(log, "new address replaces {}", endpoint.address.unwrap());
            endpoint.address = Some(address)
        }

        // Update our counts of beacon packets seen, missed, etc.  Report any
        // packets arriving twice or out of order.
        let cd = &mut endpoint.contacts[usize::from(beacon.switch)];
        cd.update_from_beacon(log, beacon);
    }

    // We haven't received a beacon from this sled on this address, but another
    // sled has.  We add a placeholder record for it, which will cause us to
    // start sending beacons to it.  This optimistic adding of endpoints we
    // haven't personally seen is how each node gradually learns the identity of
    // all the others.
    fn update_inferred(&mut self, address: Ipv6Addr) {
        let endpoint = match address.try_into() {
            Ok(TargetNetwork::Bootstrap) => &mut self.bootstrap,
            Ok(TargetNetwork::Underlay) => &mut self.underlay,
            Ok(TargetNetwork::LinkLocal) => return,
            Err(_) => return,
        };

        if endpoint.address.is_none() {
            endpoint.address = Some(address);
        }
    }
}

fn to_socketaddr(addr: Ipv6Addr) -> SocketAddrV6 {
    SocketAddrV6::new(addr, UNICAST_PORT, 0, 0)
}

// Build a list of all the addresses on this (network, switch) combination to
// which we did _not_ just send a beacon, because they are in our "pumpkin"
// list.  We use this address list to update our beacons_sent count for those
// sleds to which a beacon was artificially withheld.  This mismatch between our
// beacons_sent count and the other sled's beacons_received count should be
// interpreted by the switches as a connectivity issue.
fn get_pumpkins(
    g: &Global,
    net: TargetNetwork,
    switch: SwitchId,
) -> HashSet<Ipv6Addr> {
    let pumpkins = g.pumpkins.lock().unwrap();
    g.sleds
        .lock()
        .unwrap()
        .values()
        .filter(|s| pumpkins.contains(&(s.name.clone(), switch)))
        .filter_map(|s| match net {
            TargetNetwork::Bootstrap => s.bootstrap.address,
            TargetNetwork::Underlay => s.underlay.address,
            TargetNetwork::LinkLocal => panic!("unsupported"),
        })
        .collect()
}

// Get a list of all the addresses on this network that we should be sending
// beacons to.
fn get_targets(
    g: &Global,
    net: TargetNetwork,
    switch: SwitchId,
) -> Vec<SocketAddrV6> {
    let pumpkins = g.pumpkins.lock().unwrap();
    g.sleds
        .lock()
        .unwrap()
        .values()
        .filter(|s| !pumpkins.contains(&(s.name.clone(), switch)))
        .filter_map(|s| match net {
            TargetNetwork::Bootstrap => s.bootstrap.address,
            TargetNetwork::Underlay => s.underlay.address,
            TargetNetwork::LinkLocal => panic!("unsupported"),
        })
        .map(to_socketaddr)
        .collect()
}

// After sending a beacon to one or more sleds, we update the beacons_sent count
// for each endpoint.  This is done after the send is completed so we can drop
// the 'sleds' lock while we're pushing packets onto the network.
fn update_counts(
    g: &Global,
    net: TargetNetwork,
    switch: SwitchId,
    pkt_id: u32,
    addrs: HashSet<Ipv6Addr>,
) {
    for sled in g.sleds.lock().unwrap().values_mut() {
        let endpoint = match net {
            TargetNetwork::Bootstrap => &mut sled.bootstrap,
            TargetNetwork::Underlay => &mut sled.underlay,
            _ => continue,
        };
        if let Some(addr) = endpoint.address {
            if addrs.contains(&addr) {
                endpoint.contacts[usize::from(switch)].beacons_sent += 1;
                endpoint.contacts[usize::from(switch)].last_sent = pkt_id;
            }
        }
    }
}

// Add, remove, or update an address.  Log any changes made.
fn update_netinfo(
    log: &slog::Logger,
    old_info: &mut Option<NetInfo>,
    new_info: Option<&NetInfo>,
    name: impl std::fmt::Display,
) {
    match (&old_info.clone(), new_info) {
        (None, None) => {}
        (None, Some(x)) => {
            info!(log, "address {name} discovered: {x:?}");
        }
        (Some(x), None) => {
            info!(log, "address {name} disappeared.  Was {x:?}");
        }
        (Some(x), Some(y)) => {
            if x != y {
                info!(log, "address {name} changed from {x:?} to {y:?}");
            }
        }
    }
    *old_info = new_info.copied();
}

// Ask Illumos for configured local addresses, from which we will identify any
// addresses on the underlay and bootstrap networks, as well as the link-local
// addresses of the connected T6 and sidecar ports.
async fn refresh_addresses(
    log: &slog::Logger,
    locals: &mut LocalMap,
    peers: &mut PeerMap,
) {
    let nis = match network_interface::NetworkInterface::show() {
        Ok(s) => s,
        Err(e) => {
            error!(log, "getting network info: {e:?}");
            return;
        }
    };

    let mut info = BTreeMap::<String, NetInfo>::new();

    for ni in nis.into_iter() {
        let addr = match ni.addr {
            Some(network_interface::Addr::V6(addr)) => addr.ip,
            _ => continue,
        };
        let ifindex = ni.index;

        if let Ok(network) = addr.try_into() {
            let name = match network {
                TargetNetwork::Bootstrap => BOOTSTRAP_NAME.to_string(),
                TargetNetwork::Underlay => UNDERLAY_NAME.to_string(),
                TargetNetwork::LinkLocal => ni.name.clone(),
            };

            info.insert(
                name,
                NetInfo {
                    addr,
                    ifindex,
                    network,
                },
            );
        }
    }

    if let Some(local) = locals.get_mut(BOOTSTRAP_NAME) {
        let ni = info.get(BOOTSTRAP_NAME);
        update_netinfo(log, local, ni, BOOTSTRAP_NAME);
    }
    if let Some(local) = locals.get_mut(UNDERLAY_NAME) {
        let ni = info.get(UNDERLAY_NAME);
        update_netinfo(log, local, ni, UNDERLAY_NAME);
    }
    for (name, peer) in peers.iter_mut() {
        let ni = info.get(name);
        update_netinfo(log, &mut peer.info, ni, name);
    }
}

/// Contact data for a single sled on a single network over a specific switch.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct ContactReport {
    /// Number of seconds since a beacon from this endpoint was first receieved
    pub first_age: u32,
    /// Number of seconds since a beacon from this endpoint was last receieved
    pub latest_age: u32,
    /// ID of the last beacon packet sent to this endpoint
    pub last_sent: u32,
    /// ID of the last beacon packet received from this endpoint
    pub last_seen: u32,
    /// Number of beacon packets sent to this endpoint
    pub beacons_sent: u32,
    /// Number of beacon packets received from this endpoint
    pub beacons_seen: u32,
    /// Number of beacon packets missed by this endpoint, as determined by
    /// tracking the packet IDs of received packets.
    pub beacons_missed: u32,
}

impl From<&ContactData> for ContactReport {
    fn from(c: &ContactData) -> ContactReport {
        ContactReport {
            first_age: c.first_contact.into(),
            latest_age: c.last_contact.into(),
            last_sent: c.last_sent,
            last_seen: c.last_seen,
            beacons_sent: c.beacons_sent,
            beacons_seen: c.beacons_seen,
            beacons_missed: c.beacons_missed,
        }
    }
}

/// Contact data for a single sled on a single network over both switches.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct NetworkReport {
    pub addr: Ipv6Addr,
    pub contacts: [ContactReport; 2],
}

impl From<&NetworkEndpoint> for Option<NetworkReport> {
    fn from(n: &NetworkEndpoint) -> Option<NetworkReport> {
        n.address.map(|addr| NetworkReport {
            addr,
            contacts: [(&n.contacts[0]).into(), (&n.contacts[1]).into()],
        })
    }
}

/// Contact data for a single sled on both networks over both switches.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SledReport {
    /// The name of the sled
    pub name: String,
    /// The contact data for the switch<->sled connection, if any
    pub link_local: Option<NetworkReport>,
    /// The contact data for the bootstrap network, if any
    pub bootstrap: Option<NetworkReport>,
    /// The contact data for the underlay network, if any
    pub underlay: Option<NetworkReport>,
}

impl SledReport {
    pub fn inferred_addresses(&self) -> Vec<(String, Ipv6Addr)> {
        let mut inferred: Vec<(String, Ipv6Addr)> = Vec::new();
        if let Some(r) = &self.bootstrap {
            inferred.push((self.name.clone(), r.addr))
        }
        if let Some(r) = &self.underlay {
            inferred.push((self.name.clone(), r.addr))
        }
        inferred
    }
}

impl From<&SledContacts> for SledReport {
    fn from(c: &SledContacts) -> SledReport {
        SledReport {
            name: c.name.to_string(),
            link_local: (&c.link_local).into(),
            bootstrap: (&c.bootstrap).into(),
            underlay: (&c.underlay).into(),
        }
    }
}

/// All of the data packed into a single Beacon packet.  The same format is used
/// for packets sent between sleds and between a sled and the sidecar, although
/// some fields are only relevant for sled-originated beacons.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct BeaconData {
    /// The name of the sled/switch sending the beacon
    pub name: String,
    /// The switch over which the beacon was sent from a sled.  For beacons
    /// originating from a switch, this will contain its switch ID.
    pub switch: u8,
    /// The ID of the packet.  This value will increase by 1 each time a packet
    /// is sent.
    pub pkt_id: u32,
    /// This sled's bootstrap and underlay addresses (if any)
    pub addresses: Vec<Ipv6Addr>,
    /// This sled's contact data for all of the other sleds it is aware of
    pub sleds: Vec<SledReport>,
}

impl BeaconData {
    pub fn inferred_addresses(&self) -> Vec<(String, Ipv6Addr)> {
        let mut inferred: Vec<(String, Ipv6Addr)> = self
            .addresses
            .iter()
            .map(|a| (self.name.clone(), *a))
            .collect();
        inferred.append(
            &mut self
                .sleds
                .iter()
                .flat_map(|s| s.inferred_addresses())
                .collect(),
        );
        inferred
    }
}

// Receive and process an incoming beacon.
//
// If the packet is successfully received and processed, the call will return
// the source address and the parsed BeaconData from the packet.
//
// This function returns an error only if we were unable to successfully receive
// the packet from the network.  If the incoming packet is flawed in some way,
// that will be logged, and the function will return None.
async fn beacon_receive(
    log: &slog::Logger,
    sock: &UdpSocket,
    buf: &mut [u8],
) -> anyhow::Result<Option<(TargetNetwork, Ipv6Addr, BeaconData)>> {
    let (len, from) = match sock.try_recv_from(buf) {
        Ok((l, f)) => Ok((l, f)),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(None);
        }
        Err(e) => Err(anyhow::anyhow!("failed to read UDP socket: {e:?}")),
    }?;

    let addr = match from {
        SocketAddr::V6(from) => *from.ip(),
        SocketAddr::V4(from) => {
            warn!(log, "received IPv4 beacon"; "addr" => from.to_string());
            return Ok(None);
        }
    };
    let Ok(network) = TargetNetwork::try_from(addr) else {
        warn!(log, "beacon received from unrecognized network: {addr}");
        return Ok(None);
    };

    let b: BeaconData = match rmp_serde::from_slice(&buf[..len]) {
        Ok(b) => b,
        Err(e) => {
            warn!(log, "failed to decode beacon from {from}: {e:?}");
            return Ok(None);
        }
    };
    if SwitchId::try_from(b.switch).is_err() {
        warn!(log, "beacon from unrecognized switch: {}", b.switch);
        Ok(None)
    } else {
        Ok(Some((network, addr, b)))
    }
}

// As a sled, receive and process a beacon.
//
// When a beacon arrives from another sled, we update our ContactData for that
// sled.  When a beacon arrives from a sled or a switch, we use the set of sled
// addresses it includes to update the set of sleds we send beacons to.
async fn sled_receive(
    g: &Global,
    log: &slog::Logger,
    sock: &UdpSocket,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let Some((network, addr, b)) = beacon_receive(log, sock, buf).await? else {
        return Ok(());
    };

    let log = log.new(slog::o!("addr" => addr.to_string(),
		    "network" => network.to_string(),
		    "switch" => b.switch,
		    "name" => b.name.clone()));

    // When receiving a packet on the LinkLocal network, we update the counters
    // in our switch structure.
    if network == TargetNetwork::LinkLocal {
        let mut switches = g.switches.lock().unwrap();
        let cd = &mut switches[b.switch as usize];
        cd.update_from_beacon(&log, &b);
    }

    // When receiving a packet on a non-LinkLocal network, we update the
    // counters associated with the originating sled.
    let mut all = g.sleds.lock().unwrap();
    if network != TargetNetwork::LinkLocal {
        all.entry(b.name.clone())
            .and_modify(|sled| sled.update_from_beacon(&log, addr, &b))
            .or_insert(SledContacts::new_from_beacon(&log, addr, &b));
    }

    for (name, addr) in b
        .inferred_addresses()
        .into_iter()
        .filter(|(name, _addr)| name != &g.name)
    {
        all.entry(name.clone())
            .and_modify(|sled| sled.update_inferred(addr))
            .or_insert(SledContacts::new_inferred(&name, addr));
    }

    Ok(())
}

// As a switch, receive and process a beacon.
//
// When a beacon arrives from another sled, we update our ContactData for that
// sled.  We use the set of sled addresses it includes to update the set of
// sleds we include in our outgoing beacons, which is how sleds become aware of
// one another.
//
// We also preserve the contact data each sled reports for each of the other
// sleds.  Comparing the contact data from different sleds allows us to detect
// anomalies or asymmetries that may suggest a problem.
async fn switch_receive(
    g: &Global,
    log: &slog::Logger,
    sock: &UdpSocket,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let Some((network, addr, b)) = beacon_receive(log, sock, buf).await? else {
        return Ok(());
    };

    if network != TargetNetwork::LinkLocal {
        warn!(log, "beacon received from non-link_local network: {addr}");
        return Ok(());
    }

    let log = log.new(slog::o!("addr" => addr.to_string(),
		    "name" => b.name.clone()));

    {
        let mut all = g.sleds.lock().unwrap();
        all.entry(b.name.clone())
            .and_modify(|sled| sled.update_from_beacon(&log, addr, &b))
            .or_insert(SledContacts::new_from_beacon(&log, addr, &b));

        for (name, addr) in b.inferred_addresses() {
            all.entry(name.clone())
                .and_modify(|sled| sled.update_inferred(addr))
                .or_insert(SledContacts::new_inferred(&name, addr));
        }
    }

    let mut matrix = g.matrix.lock().unwrap();
    for sled in b.sleds {
        // Each sled<->sled contact report in the matrix is indexed with an
        // (observer, observed) tuple.
        let key = SledPair::new(&b.name, &sled.name);
        matrix
            .entry(key)
            .and_modify(|e| *e = sled.clone())
            .or_insert(sled.clone());
    }

    Ok(())
}

// Send a packet as a sled daemon.  This involves muticasting across the two
// T6->sidecar links, and then unicasting to all of the bootstrap and underlay
// addresses we know about.
async fn sled_send(
    g: &Global,
    log: &slog::Logger,
    beacon: &mut BeaconData,
    locals: &LocalMap,
    peers: &PeerMap,
) {
    let pkt_id = beacon.pkt_id;

    for peer in peers.values() {
        let Some(peer_info) = peer.info else { continue };

        // Start by sending the packet to the switch on this interface.
        beacon.switch = peer.switch.into();
        if let Err(e) = send_multicast_beacon(log, peer_info, beacon).await {
            error!(log, "failed to multicast to {peer_info:?}: {e:?}")
        } else {
            g.switches.lock().unwrap()[usize::from(peer.switch)]
                .beacons_sent += 1;
        }

        // For each network we are connected to, send a beacon to each
        // of the matching endpoints on each sled.  That is, if we have
        // a local bootstrap address, send a beacon to each of the other
        // bootstrap addresses we know about.
        for local in locals.values().flatten() {
            if local.network == TargetNetwork::LinkLocal {
                warn!(log, "LocalMap contains link-local target: {local:?}");
                continue;
            }
            // Unicast a beacon to each sled and update the beacons_sent count
            update_counts(
                g,
                local.network,
                peer.switch,
                pkt_id,
                send_unicast_beacons(
                    log,
                    peer_info.ifindex,
                    local.addr,
                    get_targets(g, local.network, peer.switch),
                    beacon,
                )
                .await,
            );

            // For each of the pumpkin-mode targets, bump the beacons_sent count
            // without actually sending a beacon.
            update_counts(
                g,
                local.network,
                peer.switch,
                pkt_id,
                get_pumpkins(g, local.network, peer.switch),
            );
        }
    }
}

// Send a packet as a switch daemon.  This just involves muticasting across each
// of our active links.
async fn switch_send(
    g: &Global,
    log: &slog::Logger,
    beacon: &mut BeaconData,
    peers: &PeerMap,
) {
    beacon.switch = g.switch_id.unwrap().into();

    // Send the packet over each of our backplane ports
    for peer in peers.values() {
        if let Some(peer_info) = peer.info {
            if let Err(e) = send_multicast_beacon(log, peer_info, beacon).await
            {
                error!(log, "failed to multicast to {peer_info:?}: {e:?}")
            }
        }
    }
}

fn beacon_build(
    g: &Global,
    pkt_id: u32,
    switch: u8,
    addresses: Vec<Ipv6Addr>,
) -> BeaconData {
    let sled_data = g.sleds.lock().unwrap();
    let sleds: Vec<SledReport> = sled_data.values().map(|s| s.into()).collect();

    BeaconData {
        name: g.name.clone(),
        switch,
        pkt_id,
        addresses,
        sleds,
    }
}

async fn build_socket(addr: Ipv6Addr, port: u16) -> anyhow::Result<UdpSocket> {
    let sock = UdpSocket::bind(SocketAddrV6::new(addr, port, 0, 0))
        .await
        .context("binding UDP socket")?;
    Ok(sock)
}

async fn build_send_socket(
    local: Ipv6Addr,
    iface: u32,
) -> anyhow::Result<UdpSocket> {
    let sock = build_socket(local, 0).await?;
    network::bind_to_interface(&sock, iface)
        .context(format!("binding UDP socket to {iface}"))?;
    Ok(sock)
}

async fn send_multicast_beacon(
    log: &slog::Logger,
    info: NetInfo,
    beacon: &mut BeaconData,
) -> anyhow::Result<()> {
    let sock = build_send_socket(info.addr, info.ifindex).await?;
    sock.join_multicast_v6(&MCAST_ADDR, info.ifindex)
        .context("joining multicast group")?;
    if let Err(e) = sock.set_multicast_loop_v6(false) {
        error!(log, "failed to set multicast loop to false: {e:?}");
    }

    let serialized = rmp_serde::to_vec(&beacon).unwrap();
    let target = SocketAddrV6::new(MCAST_ADDR, MULTICAST_PORT, 0, 0);

    sock.send_to(&serialized, target)
        .await
        .context(format!("failed to multicast from {info:?}"))?;
    Ok(())
}

async fn send_unicast_beacons(
    log: &slog::Logger,
    ifindex: u32,
    source_addr: Ipv6Addr,
    tgt_list: Vec<SocketAddrV6>,
    beacon: &mut BeaconData,
) -> HashSet<Ipv6Addr> {
    if tgt_list.is_empty() {
        return HashSet::new();
    }

    let sock = match build_send_socket(source_addr, ifindex).await {
        Ok(s) => s,
        Err(e) => {
            error!(log, "failed to build UDP socket: {e:?}");
            return HashSet::new();
        }
    };
    let serialized = rmp_serde::to_vec(&beacon).unwrap();
    let mut success = HashSet::new();
    for remote in tgt_list {
        match sock.send_to(&serialized, remote).await {
            Ok(_) => {
                success.insert(*remote.ip());
            }
            Err(e) => {
                error!(
                    log,
                    "failed to send from {source_addr:?} to {remote:?}: {e:?}"
                )
            }
        }
    }
    success
}

fn peer_ifindexes(peers: &PeerMap) -> HashSet<u32> {
    peers
        .values()
        .filter_map(|p| p.info.map(|i| i.ifindex))
        .collect()
}

fn update_multicast_membership(
    log: &slog::Logger,
    sock: &UdpSocket,
    old: HashSet<u32>,
    new: HashSet<u32>,
) {
    for ifindex in old.difference(&new) {
        if let Err(e) = sock.leave_multicast_v6(&MCAST_ADDR, *ifindex) {
            error!(log, "{ifindex} failed to leave multicast group: {e:?}");
        }
    }
    for ifindex in new.difference(&old) {
        if let Err(e) = sock.join_multicast_v6(&MCAST_ADDR, *ifindex) {
            error!(log, "{ifindex} failed to join multicast group: {e:?}");
        }
    }
}

enum WakeupReason {
    TimeoutExpired,
    PacketReceived(Arc<UdpSocket>),
    Shutdown,
}

pub async fn beacon_loop(
    g: Arc<Global>,
    mode: RunMode,
    mut shutdown_rx: tokio::sync::watch::Receiver<()>,
) -> anyhow::Result<()> {
    let log = g.log.new(slog::o!("unit" => "beacon"));
    info!(log, "beacon loop started");

    assert!(MCAST_ADDR.is_multicast(), "Must be multcast address");

    let multicast_recv = Arc::new(
        build_socket(MCAST_ADDR, MULTICAST_PORT)
            .await
            .context("binding multicast socket")?,
    );

    let unicast_recv = Arc::new(
        build_socket(Ipv6Addr::UNSPECIFIED, UNICAST_PORT)
            .await
            .context("binding unicast socket")?,
    );

    let mut locals = LocalMap::new();
    let mut peers = PeerMap::new();
    match mode {
        RunMode::Sled => {
            locals.insert(UNDERLAY_NAME.to_string(), None);
            locals.insert(BOOTSTRAP_NAME.to_string(), None);
            peers.insert(
                "cxgbe0".to_string(),
                Peer::new(0u8.try_into().unwrap()),
            );
            peers.insert(
                "cxgbe1".to_string(),
                Peer::new(1u8.try_into().unwrap()),
            );
        }
        RunMode::Switch => {
            for sled in 0..switch::MAX_SLEDS {
                peers.insert(
                    format!("tfportrear{sled}_0"),
                    Peer::new(g.switch_id.unwrap()),
                );
            }
        }
    }

    let mut buf = [0; MAX_BUF_SIZE];
    let mut pkt_id = 0u32;
    let mut now = Instant::now();
    let mut next_beacon = now;

    loop {
        let delay = next_beacon - now;
        #[rustfmt::skip]
	let reason = tokio::select! {
	    _ = shutdown_rx.changed() => { WakeupReason::Shutdown },
	    _ = tokio::time::sleep(delay) => { WakeupReason::TimeoutExpired },
	    _ = unicast_recv.readable() => { WakeupReason::PacketReceived(unicast_recv.clone()) },
	    _ = multicast_recv.readable() => { WakeupReason::PacketReceived(multicast_recv.clone()) },
	};

        match reason {
            WakeupReason::Shutdown => {
                info!(log, "shutdown message received");
                break;
            }
            WakeupReason::PacketReceived(sock) => {
                if let Err(e) = match mode {
                    RunMode::Sled => {
                        sled_receive(&g, &log, &sock, &mut buf).await
                    }
                    RunMode::Switch => {
                        switch_receive(&g, &log, &sock, &mut buf).await
                    }
                } {
                    error!(g.log, "while receiving beacon: {e:?}");
                }
            }
            WakeupReason::TimeoutExpired => {
                // We don't act on the timeout here.  Instead, we
                // explicitly do a timecheck below.  This ensures that
                // we don't skip a beacon if a packet receive takes long
                // enough to span a timeout.
            }
        }

        now = Instant::now();
        if now >= next_beacon {
            // Update our lists of addresses on the various networks.  If
            // the set of addresses on per links has changed, update our
            // multicast membership accordingly.
            let old_set = peer_ifindexes(&peers);
            refresh_addresses(&log, &mut locals, &mut peers).await;
            let new_set = peer_ifindexes(&peers);
            update_multicast_membership(
                &log,
                &multicast_recv,
                old_set,
                new_set,
            );

            let addrs: Vec<Ipv6Addr> =
                locals.values().flatten().map(|info| info.addr).collect();
            pkt_id += 1;
            let mut beacon = beacon_build(&g, pkt_id, 0, addrs);

            match mode {
                RunMode::Sled => {
                    sled_send(&g, &log, &mut beacon, &locals, &peers).await
                }
                RunMode::Switch => {
                    switch_send(&g, &log, &mut beacon, &peers).await
                }
            };

            now = Instant::now();
            next_beacon = now + BEACON_PERIOD;
        }
    }

    info!(log, "beacon loop done");
    Ok(())
}

/// Construct a maximally sized beacon, serialize it, and verify that it fits
/// into the buffer we allocate for incoming packets.
#[test]
fn test_max_beacon_size() {
    // Our sled names have a common structure, which fits into this space.
    // If this structure grows while the pumpkin issue is still extant,
    // we'll have to adjust this test.
    const SLED_NAME: &str = "BRM42220036";

    let network_report = NetworkReport {
        addr: Ipv6Addr::LOCALHOST,
        contacts: [ContactReport::default(), ContactReport::default()],
    };

    let sled_report = SledReport {
        name: SLED_NAME.to_string(),
        link_local: Some(network_report.clone()),
        bootstrap: Some(network_report.clone()),
        underlay: Some(network_report.clone()),
    };
    let sleds = (0..switch::MAX_SLEDS)
        .map(|_| sled_report.clone())
        .collect();
    let addresses = vec![Ipv6Addr::LOCALHOST, Ipv6Addr::LOCALHOST];
    let beacon_data = BeaconData {
        name: SLED_NAME.to_string(),
        switch: 0,
        pkt_id: 0,
        addresses,
        sleds,
    };
    let serialized = rmp_serde::to_vec(&beacon_data).unwrap();
    // Leave a hint in case of failure
    println!("buffer size needed: {}", serialized.len());
    assert!(serialized.len() < MAX_BUF_SIZE);
}
