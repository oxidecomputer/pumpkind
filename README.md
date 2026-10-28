# pumpkind

This daemon is used to track the viability of inter-sled links in an Oxide rack.

One instance of the daemon runs in the global zone on each sled, sending beacon
packets to each other sled and status updates to collectors running in the
switch zones.

One instance of the daemon runs in each switch zone.  It collects reachability
data from the instances running on each sled.  It provides each sled with an
inventory of all other sleds, letting them know where beacon packets should be
sent.

# Beacons

## Beacon contents

Each beacon packet contains:
 - the name of the sender
 - the ID of the switch over which the packet is expected to travel
 - a sequence number
 - the addresses used by the sender
 - a summary of the senders connectivity to other nodes

The sequence number increases by 1 each time the sender issues a round of
beacons.  This predictable numbering allows the receiver (or another observer)
to determine whether beacon packets are being lost.  The full set of addresses
being used by the sender allow each daemon to gradually learn the full
population of daemons in the rack. The per-sled connectivity information is used
by the switch zone daemons to identify connectivity failures.

## Beacon transmission

Every "n" seconds, each sender issues a set of beacon packets to all of the
addresses it knows about over UDP.  Each beacon in a set will share a packet ID.

Initially, each daemon will only know its own set of local addresses.  When it
constructs a beacon packet, its payload will include its addresses (if any) on
the underlay and bootstrap networks.  The beacon will be multicast across each
of the backplane interfaces (`cxgbe0` and `cxgbe1` on the sleds, and
`tfportrear0` etc., in the switch zone). When a beacon arrives, the recipient
will examine the payload to learn the underlay and bootstrap addresses of other
sleds in the rack. Note: In theory, we could query `ddmd` for information about
its peers, but we want this daemon to be functional even if no other component
of the system is.

Once a sled has learned the addresses of other sleds, it will send unicast UDP
beacon packets to those addresses as well as multicasting to the switches.  Each
unicast beacon will be sent twice: once over each switch.  On receipt of a
unicast beacon, the sled will note which interface it arrived on.  By comparing
the set of packets sent and received by each pair of switches, we can identify
those pairs that appear to be hitting a broken communication path.

# Diagnosis

The specific symptoms we use to diagnose an issue are:

  - two sleds can communicate with a daemon running on switch X
  - the sleds cannot communicate with each other across switch X
  - the sleds can communicate with one another across switch Y

# Mitigation

When the daemon on a switch node believes that it has detected a pair of sleds
in a pumpkin state, it will use `swadm` to dump in-daemon state, the raw
on-ASIC table data, as well as all of the counters in the `p4` program.  Once it
has collected this data for post-mortem analysis, it will ask `smf` to restart
the `dpd` daemon.

Once `dpd` has been reset, the daemon will not attempt to reset the daemon again
for several minutes.  This gives all the sleds time to recover from the
transient network outage, reducing the likelihood that the short-term
instability is mistakenly identified as a reoccurrence of the fault.

**In `log_only` mode, only data collection is performed. That is the only mode
the daemon currently runs in.**

We have a hard limit of 5 resets allowed. If we exceed that limit, we assume we
are seeing some failure mode that isn't being helped by our resets.
