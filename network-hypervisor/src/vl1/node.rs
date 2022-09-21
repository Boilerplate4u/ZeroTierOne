// (c) 2020-2022 ZeroTier, Inc. -- currently propritery pending actual release and licensing. See LICENSE.md.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};

use crate::error::InvalidParameterError;
use crate::protocol::*;
use crate::util::gate::IntervalGate;
use crate::util::marshalable::Marshalable;
use crate::vl1::address::Address;
use crate::vl1::debug_event;
use crate::vl1::endpoint::Endpoint;
use crate::vl1::event::Event;
use crate::vl1::identity::Identity;
use crate::vl1::path::{Path, PathServiceResult};
use crate::vl1::peer::Peer;
use crate::vl1::rootset::RootSet;

use zerotier_crypto::random;
use zerotier_crypto::verified::Verified;
use zerotier_utils::hex;
use zerotier_utils::ringbuffer::RingBuffer;

/// Trait implemented by external code to handle events and provide an interface to the system or application.
///
/// These methods are basically callbacks that the core calls to request or transmit things. They are called
/// during calls to things like wire_recieve() and do_background_tasks().
pub trait HostSystem: Sync + Send + 'static {
    /// Type for local system sockets.
    type LocalSocket: Sync + Send + Hash + PartialEq + Eq + Clone + ToString + 'static;

    /// Type for local system interfaces.    
    type LocalInterface: Sync + Send + Hash + PartialEq + Eq + Clone + ToString;

    /// A VL1 level event occurred.
    fn event(&self, event: Event);

    /// Check a local socket for validity.
    ///
    /// This could return false if the socket's interface no longer exists, its port has been
    /// unbound, etc.
    fn local_socket_is_valid(&self, socket: &Self::LocalSocket) -> bool;

    /// Called to send a packet over the physical network (virtual -> physical).
    ///
    /// This sends with UDP-like semantics. It should do whatever best effort it can and return.
    ///
    /// If a local socket is specified the implementation should send from that socket or not
    /// at all (returning false). If a local interface is specified the implementation should
    /// send from all sockets on that interface. If neither is specified the packet may be
    /// sent on all sockets or a random subset.
    ///
    /// For endpoint types that support a packet TTL, the implementation may set the TTL
    /// if the 'ttl' parameter is not zero. If the parameter is zero or TTL setting is not
    /// supported, the default TTL should be used. This parameter is ignored for types that
    /// don't support it.
    fn wire_send(
        &self,
        endpoint: &Endpoint,
        local_socket: Option<&Self::LocalSocket>,
        local_interface: Option<&Self::LocalInterface>,
        data: &[u8],
        packet_ttl: u8,
    );

    /// Called to get the current time in milliseconds from the system monotonically increasing clock.
    /// This needs to be accurate to about 250 milliseconds resolution or better.
    fn time_ticks(&self) -> i64;

    /// Called to get the current time in milliseconds since epoch from the real-time clock.
    /// This needs to be accurate to about one second resolution or better.
    fn time_clock(&self) -> i64;
}

/// Trait to be implemented by outside code to provide object storage to VL1
pub trait NodeStorage: Sync + Send + 'static {
    /// Load this node's identity from the data store.
    fn load_node_identity(&self) -> Option<Identity>;

    /// Save this node's identity to the data store.
    fn save_node_identity(&self, id: &Identity);
}

/// Trait to be implemented to provide path hints and a filter to approve physical paths
pub trait PathFilter: Sync + Send + 'static {
    /// Called to check and see if a physical address should be used for ZeroTier traffic to a node.
    fn check_path<HostSystemImpl: HostSystem>(
        &self,
        id: &Identity,
        endpoint: &Endpoint,
        local_socket: Option<&HostSystemImpl::LocalSocket>,
        local_interface: Option<&HostSystemImpl::LocalInterface>,
    ) -> bool;

    /// Called to look up any statically defined or memorized paths to known nodes.
    fn get_path_hints<HostSystemImpl: HostSystem>(
        &self,
        id: &Identity,
    ) -> Option<
        Vec<(
            Endpoint,
            Option<HostSystemImpl::LocalSocket>,
            Option<HostSystemImpl::LocalInterface>,
        )>,
    >;
}

/// Result of a packet handler.
pub enum PacketHandlerResult {
    /// Packet was handled successfully.
    Ok,

    /// Packet was handled and an error occurred (malformed, authentication failure, etc.)
    Error,

    /// Packet was not handled by this handler.
    NotHandled,
}

/// Interface between VL1 and higher/inner protocol layers.
///
/// This is implemented by Switch in VL2. It's usually not used outside of VL2 in the core but
/// it could also be implemented for testing or "off label" use of VL1 to carry different protocols.
pub trait InnerProtocol: Sync + Send + 'static {
    /// Handle a packet, returning true if it was handled by the next layer.
    ///
    /// Do not attempt to handle OK or ERROR. Instead implement handle_ok() and handle_error().
    fn handle_packet<HostSystemImpl: HostSystem>(
        &self,
        source: &Arc<Peer<HostSystemImpl>>,
        source_path: &Arc<Path<HostSystemImpl>>,
        verb: u8,
        payload: &PacketBuffer,
    ) -> PacketHandlerResult;

    /// Handle errors, returning true if the error was recognized.
    fn handle_error<HostSystemImpl: HostSystem>(
        &self,
        source: &Arc<Peer<HostSystemImpl>>,
        source_path: &Arc<Path<HostSystemImpl>>,
        in_re_verb: u8,
        in_re_message_id: u64,
        error_code: u8,
        payload: &PacketBuffer,
        cursor: &mut usize,
    ) -> PacketHandlerResult;

    /// Handle an OK, returing true if the OK was recognized.
    fn handle_ok<HostSystemImpl: HostSystem>(
        &self,
        source: &Arc<Peer<HostSystemImpl>>,
        source_path: &Arc<Path<HostSystemImpl>>,
        in_re_verb: u8,
        in_re_message_id: u64,
        payload: &PacketBuffer,
        cursor: &mut usize,
    ) -> PacketHandlerResult;

    /// Check if this peer should communicate with another at all.
    fn should_communicate_with(&self, id: &Identity) -> bool;
}

/// How often to check the root cluster definitions against the root list and update.
const ROOT_SYNC_INTERVAL_MS: i64 = 1000;

struct RootInfo<HostSystemImpl: HostSystem> {
    /// Root sets to which we are a member.
    sets: HashMap<String, Verified<RootSet>>,

    /// Root peers and their statically defined endpoints (from root sets).
    roots: HashMap<Arc<Peer<HostSystemImpl>>, Vec<Endpoint>>,

    /// If this node is a root, these are the root sets to which it's a member in binary serialized form.
    /// Set to None if this node is not a root, meaning it doesn't appear in any of its root sets.
    this_root_sets: Option<Vec<u8>>,

    /// True if sets have been modified and things like 'roots' need to be rebuilt.
    sets_modified: bool,

    /// True if this node is online, which means it can talk to at least one of its roots.
    online: bool,
}

#[derive(Default)]
struct BackgroundTaskIntervals {
    root_sync: IntervalGate<{ ROOT_SYNC_INTERVAL_MS }>,
    root_hello: IntervalGate<{ ROOT_HELLO_INTERVAL }>,
    root_spam_hello: IntervalGate<{ ROOT_HELLO_SPAM_INTERVAL }>,
    peer_service: IntervalGate<{ crate::vl1::peer::SERVICE_INTERVAL_MS }>,
    path_service: IntervalGate<{ crate::vl1::path::SERVICE_INTERVAL_MS }>,
    whois_queue_retry: IntervalGate<{ WHOIS_RETRY_INTERVAL }>,
}

#[derive(Default)]
struct WhoisQueueItem {
    waiting_packets: RingBuffer<PooledPacketBuffer, WHOIS_MAX_WAITING_PACKETS>,
    retry_count: u16,
}

/// A ZeroTier VL1 node that can communicate securely with the ZeroTier peer-to-peer network.
pub struct Node<HostSystemImpl: HostSystem> {
    /// A random ID generated to identify this particular running instance.
    ///
    /// This can be used to implement multi-homing by allowing remote nodes to distinguish instances
    /// that share an identity.
    pub instance_id: [u8; 16],

    /// This node's identity and permanent keys.
    pub identity: Identity,

    /// Interval latches for periodic background tasks.
    intervals: Mutex<BackgroundTaskIntervals>,

    /// Canonicalized network paths, held as Weak<> to be automatically cleaned when no longer in use.
    paths: RwLock<HashMap<PathKey<'static, 'static, HostSystemImpl>, Arc<Path<HostSystemImpl>>>>,

    /// Peers with which we are currently communicating.
    peers: RwLock<HashMap<Address, Arc<Peer<HostSystemImpl>>>>,

    /// This node's trusted roots, sorted in ascending order of quality/preference, and cluster definitions.
    roots: RwLock<RootInfo<HostSystemImpl>>,

    /// Current best root.
    best_root: RwLock<Option<Arc<Peer<HostSystemImpl>>>>,

    /// Queue of identities being looked up.
    whois_queue: Mutex<HashMap<Address, WhoisQueueItem>>,
}

impl<HostSystemImpl: HostSystem> Node<HostSystemImpl> {
    pub fn new<NodeStorageImpl: NodeStorage>(
        host_system: &HostSystemImpl,
        storage: &NodeStorageImpl,
        auto_generate_identity: bool,
        auto_upgrade_identity: bool,
    ) -> Result<Self, InvalidParameterError> {
        let mut id = {
            let id = storage.load_node_identity();
            if id.is_none() {
                if !auto_generate_identity {
                    return Err(InvalidParameterError("no identity found and auto-generate not enabled"));
                } else {
                    let id = Identity::generate();
                    host_system.event(Event::IdentityAutoGenerated(id.clone()));
                    storage.save_node_identity(&id);
                    id
                }
            } else {
                id.unwrap()
            }
        };

        if auto_upgrade_identity {
            let old = id.clone();
            if id.upgrade()? {
                storage.save_node_identity(&id);
                host_system.event(Event::IdentityAutoUpgraded(old, id.clone()));
            }
        }

        debug_event!(host_system, "[vl1] loaded identity {}", id.to_string());

        Ok(Self {
            instance_id: random::get_bytes_secure(),
            identity: id,
            intervals: Mutex::new(BackgroundTaskIntervals::default()),
            paths: RwLock::new(HashMap::new()),
            peers: RwLock::new(HashMap::new()),
            roots: RwLock::new(RootInfo {
                sets: HashMap::new(),
                roots: HashMap::new(),
                this_root_sets: None,
                sets_modified: false,
                online: false,
            }),
            best_root: RwLock::new(None),
            whois_queue: Mutex::new(HashMap::new()),
        })
    }

    pub fn peer(&self, a: Address) -> Option<Arc<Peer<HostSystemImpl>>> {
        self.peers.read().get(&a).cloned()
    }

    pub fn is_online(&self) -> bool {
        self.roots.read().online
    }

    fn update_best_root(&self, host_system: &HostSystemImpl, time_ticks: i64) {
        let roots = self.roots.read();

        // The best root is the one that has replied to a HELLO most recently. Since we send HELLOs in unison
        // this is a proxy for latency and also causes roots that fail to reply to drop out quickly.
        let mut best = None;
        let mut latest_hello_reply = 0;
        for (r, _) in roots.roots.iter() {
            let t = r.last_hello_reply_time_ticks.load(Ordering::Relaxed);
            if t > latest_hello_reply {
                latest_hello_reply = t;
                let _ = best.insert(r);
            }
        }

        if let Some(best) = best {
            let mut best_root = self.best_root.write();
            if let Some(best_root) = best_root.as_mut() {
                if !Arc::ptr_eq(best_root, best) {
                    debug_event!(
                        host_system,
                        "[vl1] new best root: {} (replaced {})",
                        best.identity.address.to_string(),
                        best_root.identity.address.to_string()
                    );
                    *best_root = best.clone();
                }
            } else {
                debug_event!(
                    host_system,
                    "[vl1] new best root: {} (was empty)",
                    best.identity.address.to_string()
                );
                let _ = best_root.insert(best.clone());
            }
        } else {
            if let Some(old_best) = self.best_root.write().take() {
                debug_event!(
                    host_system,
                    "[vl1] new best root: NONE (replaced {})",
                    old_best.identity.address.to_string()
                );
            }
        }

        // Determine if the node is online by whether there is a currently reachable root.
        if (time_ticks - latest_hello_reply) < (ROOT_HELLO_INTERVAL * 2) && best.is_some() {
            if !roots.online {
                drop(roots);
                self.roots.write().online = true;
                host_system.event(Event::Online(true));
            }
        } else if roots.online {
            drop(roots);
            self.roots.write().online = false;
            host_system.event(Event::Online(false));
        }
    }

    pub fn do_background_tasks(&self, host_system: &HostSystemImpl) -> Duration {
        const INTERVAL_MS: i64 = 1000;
        const INTERVAL: Duration = Duration::from_millis(INTERVAL_MS as u64);
        let time_ticks = host_system.time_ticks();

        let (root_sync, root_hello, mut root_spam_hello, peer_service, path_service, whois_queue_retry) = {
            let mut intervals = self.intervals.lock();
            (
                intervals.root_sync.gate(time_ticks),
                intervals.root_hello.gate(time_ticks),
                intervals.root_spam_hello.gate(time_ticks),
                intervals.peer_service.gate(time_ticks),
                intervals.path_service.gate(time_ticks),
                intervals.whois_queue_retry.gate(time_ticks),
            )
        };

        // We only "spam" (try to contact roots more often) if we are offline.
        if root_spam_hello {
            root_spam_hello = !self.is_online();
        }

        debug_event!(
            host_system,
            "[vl1] do_background_tasks:{}{}{}{}{}{} ----",
            if root_sync {
                " root_sync"
            } else {
                ""
            },
            if root_hello {
                " root_hello"
            } else {
                ""
            },
            if root_spam_hello {
                " root_spam_hello"
            } else {
                ""
            },
            if peer_service {
                " peer_service"
            } else {
                ""
            },
            if path_service {
                " path_service"
            } else {
                ""
            },
            if whois_queue_retry {
                " whois_queue_retry"
            } else {
                ""
            }
        );

        if root_sync {
            if {
                let mut roots = self.roots.write();
                if roots.sets_modified {
                    roots.sets_modified = false;
                    true
                } else {
                    false
                }
            } {
                debug_event!(host_system, "[vl1] root sets modified, synchronizing internal data structures");

                let (mut old_root_identities, address_collisions, new_roots, bad_identities, my_root_sets) = {
                    let roots = self.roots.read();

                    let old_root_identities: Vec<Identity> = roots.roots.iter().map(|(p, _)| p.identity.clone()).collect();
                    let mut new_roots = HashMap::new();
                    let mut bad_identities = Vec::new();
                    let mut my_root_sets: Option<Vec<u8>> = None;

                    // This is a sanity check to make sure we don't have root sets that contain roots with the same address
                    // but a different identity. If we do, the offending address is blacklisted. This would indicate something
                    // weird and possibly nasty happening with whomever is making your root set definitions.
                    let mut address_collisions = Vec::new();
                    {
                        let mut address_collision_check = HashMap::with_capacity(roots.sets.len() * 8);
                        for (_, rs) in roots.sets.iter() {
                            for m in rs.members.iter() {
                                if m.identity.eq(&self.identity) {
                                    let _ = my_root_sets.get_or_insert_with(|| Vec::new()).write_all(rs.to_bytes().as_slice());
                                } else if self
                                    .peers
                                    .read()
                                    .get(&m.identity.address)
                                    .map_or(false, |p| !p.identity.eq(&m.identity))
                                    || address_collision_check
                                        .insert(m.identity.address, &m.identity)
                                        .map_or(false, |old_id| !old_id.eq(&m.identity))
                                {
                                    address_collisions.push(m.identity.address);
                                }
                            }
                        }
                    }

                    for (_, rs) in roots.sets.iter() {
                        for m in rs.members.iter() {
                            if m.endpoints.is_some() && !address_collisions.contains(&m.identity.address) && !m.identity.eq(&self.identity)
                            {
                                debug_event!(
                                    host_system,
                                    "[vl1] examining root {} with {} endpoints",
                                    m.identity.address.to_string(),
                                    m.endpoints.as_ref().map_or(0, |e| e.len())
                                );
                                let peers = self.peers.upgradable_read();
                                if let Some(peer) = peers.get(&m.identity.address) {
                                    new_roots.insert(peer.clone(), m.endpoints.as_ref().unwrap().iter().cloned().collect());
                                } else {
                                    if let Some(peer) = Peer::<HostSystemImpl>::new(&self.identity, m.identity.clone(), time_ticks) {
                                        new_roots.insert(
                                            RwLockUpgradableReadGuard::upgrade(peers)
                                                .entry(m.identity.address)
                                                .or_insert_with(|| Arc::new(peer))
                                                .clone(),
                                            m.endpoints.as_ref().unwrap().iter().cloned().collect(),
                                        );
                                    } else {
                                        bad_identities.push(m.identity.clone());
                                    }
                                }
                            }
                        }
                    }

                    (old_root_identities, address_collisions, new_roots, bad_identities, my_root_sets)
                };

                for c in address_collisions.iter() {
                    host_system.event(Event::SecurityWarning(format!(
                        "address/identity collision in root sets! address {} collides across root sets or with an existing peer and is being ignored as a root!",
                        c.to_string()
                    )));
                }
                for i in bad_identities.iter() {
                    host_system.event(Event::SecurityWarning(format!(
                        "bad identity detected for address {} in at least one root set, ignoring (error creating peer object)",
                        i.address.to_string()
                    )));
                }

                let mut new_root_identities: Vec<Identity> = new_roots.iter().map(|(p, _)| p.identity.clone()).collect();
                old_root_identities.sort_unstable();
                new_root_identities.sort_unstable();

                if !old_root_identities.eq(&new_root_identities) {
                    let mut roots = self.roots.write();
                    roots.roots = new_roots;
                    roots.this_root_sets = my_root_sets;
                    host_system.event(Event::UpdatedRoots(old_root_identities, new_root_identities));
                }
            }

            self.update_best_root(host_system, time_ticks);
        }

        // Say HELLO to all roots periodically. For roots we send HELLO to every single endpoint
        // they have, which is a behavior that differs from normal peers. This allows roots to
        // e.g. see our IPv4 and our IPv6 address which can be important for us to learn our
        // external addresses from them.
        if root_hello || root_spam_hello {
            let roots = {
                let roots = self.roots.read();
                let mut roots_copy = Vec::with_capacity(roots.roots.len());
                for (root, endpoints) in roots.roots.iter() {
                    roots_copy.push((root.clone(), endpoints.clone()));
                }
                roots_copy
            };
            for (root, endpoints) in roots.iter() {
                for ep in endpoints.iter() {
                    debug_event!(
                        host_system,
                        "sending HELLO to root {} (root interval: {})",
                        root.identity.address.to_string(),
                        ROOT_HELLO_INTERVAL
                    );
                    let root = root.clone();
                    let ep = ep.clone();
                    root.send_hello(host_system, self, Some(&ep));
                }
            }
        }

        if peer_service {
            // Service all peers, removing any whose service() method returns false AND that are not
            // roots. Roots on the other hand remain in the peer list as long as they are roots.
            let mut dead_peers = Vec::new();
            {
                let roots = self.roots.read();
                for (a, peer) in self.peers.read().iter() {
                    if !peer.service(host_system, self, time_ticks) && !roots.roots.contains_key(peer) {
                        dead_peers.push(*a);
                    }
                }
            }
            for dp in dead_peers.iter() {
                self.peers.write().remove(dp);
            }
        }

        if path_service {
            let mut dead_paths = Vec::new();
            let mut need_keepalive = Vec::new();

            // First check all paths in read mode to avoid blocking the entire node.
            for (k, path) in self.paths.read().iter() {
                if host_system.local_socket_is_valid(k.local_socket()) {
                    match path.service(time_ticks) {
                        PathServiceResult::Ok => {}
                        PathServiceResult::Dead => dead_paths.push(k.to_copied()),
                        PathServiceResult::NeedsKeepalive => need_keepalive.push(path.clone()),
                    }
                } else {
                    dead_paths.push(k.to_copied());
                }
            }

            // Lock in write mode and remove dead paths, doing so piecemeal to again avoid blocking.
            for dp in dead_paths.iter() {
                self.paths.write().remove(dp);
            }

            // Finally run keepalive sends as a batch.
            let keepalive_buf = [time_ticks as u8]; // just an arbitrary byte, no significance
            for p in need_keepalive.iter() {
                host_system.wire_send(&p.endpoint, Some(&p.local_socket), Some(&p.local_interface), &keepalive_buf, 0);
            }
        }

        if whois_queue_retry {
            let need_whois = {
                let mut need_whois = Vec::new();
                let mut whois_queue = self.whois_queue.lock();
                whois_queue.retain(|_, qi| qi.retry_count <= WHOIS_RETRY_COUNT_MAX);
                for (address, qi) in whois_queue.iter_mut() {
                    qi.retry_count += 1;
                    need_whois.push(*address);
                }
                need_whois
            };
            if !need_whois.is_empty() {
                self.send_whois(host_system, need_whois.as_slice());
            }
        }

        debug_event!(host_system, "[vl1] do_background_tasks DONE ----");
        INTERVAL
    }

    pub fn handle_incoming_physical_packet<InnerProtocolImpl: InnerProtocol>(
        &self,
        host_system: &HostSystemImpl,
        inner: &InnerProtocolImpl,
        source_endpoint: &Endpoint,
        source_local_socket: &HostSystemImpl::LocalSocket,
        source_local_interface: &HostSystemImpl::LocalInterface,
        mut data: PooledPacketBuffer,
    ) {
        debug_event!(
            host_system,
            "[vl1] {} -> #{} {}->{} length {} (on socket {}@{})",
            source_endpoint.to_string(),
            data.bytes_fixed_at::<8>(0)
                .map_or("????????????????".into(), |pid| hex::to_string(pid)),
            data.bytes_fixed_at::<5>(13).map_or("??????????".into(), |src| hex::to_string(src)),
            data.bytes_fixed_at::<5>(8).map_or("??????????".into(), |dest| hex::to_string(dest)),
            data.len(),
            source_local_socket.to_string(),
            source_local_interface.to_string()
        );

        // An 0xff value at byte [8] means this is a ZSSP packet. This is accomplished via the
        // backward compatibilty hack of always having 0xff at byte [4] of 6-byte session IDs
        // and by having 0xffffffffffff be the "nil" session ID for session init packets. ZSSP
        // is the new V2 Noise-based forward-secure transport protocol. What follows below this
        // is legacy handling of the old v1 protocol.
        if data.u8_at(8).map_or(false, |x| x == 0xff) {
            todo!();
        }

        // Legacy ZeroTier V1 packet handling
        if let Ok(fragment_header) = data.struct_mut_at::<v1::FragmentHeader>(0) {
            if let Some(dest) = Address::from_bytes_fixed(&fragment_header.dest) {
                let time_ticks = host_system.time_ticks();
                if dest == self.identity.address {
                    let path = self.canonical_path(source_endpoint, source_local_socket, source_local_interface, time_ticks);
                    path.log_receive_anything(time_ticks);

                    if fragment_header.is_fragment() {
                        #[cfg(debug_assertions)]
                        let fragment_header_id = u64::from_be_bytes(fragment_header.id);
                        debug_event!(
                            host_system,
                            "[vl1] [v1] #{:0>16x} fragment {} of {} received",
                            u64::from_be_bytes(fragment_header.id),
                            fragment_header.fragment_no(),
                            fragment_header.total_fragments()
                        );

                        if let Some(assembled_packet) = path.receive_fragment(
                            fragment_header.packet_id(),
                            fragment_header.fragment_no(),
                            fragment_header.total_fragments(),
                            data,
                            time_ticks,
                        ) {
                            if let Some(frag0) = assembled_packet.frags[0].as_ref() {
                                #[cfg(debug_assertions)]
                                debug_event!(host_system, "[vl1] [v1] #{:0>16x} packet fully assembled!", fragment_header_id);

                                if let Ok(packet_header) = frag0.struct_at::<v1::PacketHeader>(0) {
                                    if let Some(source) = Address::from_bytes(&packet_header.src) {
                                        if let Some(peer) = self.peer(source) {
                                            peer.receive(
                                                self,
                                                host_system,
                                                inner,
                                                time_ticks,
                                                &path,
                                                packet_header,
                                                frag0,
                                                &assembled_packet.frags[1..(assembled_packet.have as usize)],
                                            );
                                        } else {
                                            /*
                                            self.whois_lookup_queue.query(
                                                self,
                                                host_system,
                                                source,
                                                Some(QueuedPacket::Fragmented(assembled_packet)),
                                            );
                                            */
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        #[cfg(debug_assertions)]
                        if let Ok(packet_header) = data.struct_at::<v1::PacketHeader>(0) {
                            debug_event!(
                                host_system,
                                "[vl1] [v1] #{:0>16x} is unfragmented",
                                u64::from_be_bytes(packet_header.id)
                            );

                            if let Some(source) = Address::from_bytes(&packet_header.src) {
                                if let Some(peer) = self.peer(source) {
                                    peer.receive(self, host_system, inner, time_ticks, &path, packet_header, data.as_ref(), &[]);
                                } else {
                                    self.whois(host_system, source, Some(data));
                                }
                            }
                        }
                    }
                } else {
                    #[cfg(debug_assertions)]
                    let debug_packet_id;

                    if fragment_header.is_fragment() {
                        #[cfg(debug_assertions)]
                        {
                            debug_packet_id = u64::from_be_bytes(fragment_header.id);
                            debug_event!(
                                host_system,
                                "[vl1] [v1] #{:0>16x} forwarding packet fragment to {}",
                                debug_packet_id,
                                dest.to_string()
                            );
                        }
                        if fragment_header.increment_hops() > v1::FORWARD_MAX_HOPS {
                            #[cfg(debug_assertions)]
                            debug_event!(host_system, "[vl1] [v1] #{:0>16x} discarded: max hops exceeded!", debug_packet_id);
                            return;
                        }
                    } else {
                        if let Ok(packet_header) = data.struct_mut_at::<v1::PacketHeader>(0) {
                            #[cfg(debug_assertions)]
                            {
                                debug_packet_id = u64::from_be_bytes(packet_header.id);
                                debug_event!(
                                    host_system,
                                    "[vl1] [v1] #{:0>16x} forwarding packet to {}",
                                    debug_packet_id,
                                    dest.to_string()
                                );
                            }
                            if packet_header.increment_hops() > v1::FORWARD_MAX_HOPS {
                                #[cfg(debug_assertions)]
                                debug_event!(
                                    host_system,
                                    "[vl1] [v1] #{:0>16x} discarded: max hops exceeded!",
                                    u64::from_be_bytes(packet_header.id)
                                );
                                return;
                            }
                        } else {
                            return;
                        }
                    }

                    if let Some(peer) = self.peer(dest) {
                        // TODO: SHOULD we forward? Need a way to check.
                        peer.forward(host_system, time_ticks, data.as_ref());
                        #[cfg(debug_assertions)]
                        debug_event!(host_system, "[vl1] [v1] #{:0>16x} forwarded successfully", debug_packet_id);
                    }
                }
            }
        }
    }

    fn whois(&self, host_system: &HostSystemImpl, address: Address, waiting_packet: Option<PooledPacketBuffer>) {
        {
            let mut whois_queue = self.whois_queue.lock();
            let qi = whois_queue.entry(address).or_default();
            if let Some(p) = waiting_packet {
                qi.waiting_packets.add(p);
            }
            if qi.retry_count > 0 {
                return;
            } else {
                qi.retry_count += 1;
            }
        }
        self.send_whois(host_system, &[address]);
    }

    fn send_whois(&self, host_system: &HostSystemImpl, addresses: &[Address]) {
        if let Some(root) = self.best_root() {}
    }

    /// Get the current "best" root from among this node's trusted roots.
    pub fn best_root(&self) -> Option<Arc<Peer<HostSystemImpl>>> {
        self.best_root.read().clone()
    }

    /// Check whether a peer is a root according to any root set trusted by this node.
    pub fn is_peer_root(&self, peer: &Peer<HostSystemImpl>) -> bool {
        self.roots.read().roots.keys().any(|p| p.identity.eq(&peer.identity))
    }

    /// Returns true if this node is a member of a root set (that it knows about).
    pub fn this_node_is_root(&self) -> bool {
        self.roots.read().this_root_sets.is_some()
    }

    /// Called when a remote node sends us a root set update, applying the update if it is valid and applicable.
    ///
    /// This will only replace an existing root set with a newer one. It won't add a new root set, which must be
    /// done by an authorized user or administrator not just by a root.
    pub(crate) fn remote_update_root_set(&self, received_from: &Identity, rs: Verified<RootSet>) {
        let mut roots = self.roots.write();
        if let Some(entry) = roots.sets.get_mut(&rs.name) {
            if entry.members.iter().any(|m| m.identity.eq(received_from)) && rs.should_replace(entry) {
                *entry = rs;
                roots.sets_modified = true;
            }
        }
    }

    /// Add a new root set or update the existing root set if the new root set is newer and otherwise matches.
    pub fn add_update_root_set(&self, rs: Verified<RootSet>) -> bool {
        let mut roots = self.roots.write();
        if let Some(entry) = roots.sets.get_mut(&rs.name) {
            if rs.should_replace(entry) {
                *entry = rs;
                roots.sets_modified = true;
                true
            } else {
                false
            }
        } else {
            let _ = roots.sets.insert(rs.name.clone(), rs);
            roots.sets_modified = true;
            true
        }
    }

    /// Returns whether or not this node has any root sets defined.
    pub fn has_roots_defined(&self) -> bool {
        self.roots.read().sets.iter().any(|rs| !rs.1.members.is_empty())
    }

    /// Initialize with default roots if there are no roots defined, otherwise do nothing.
    pub fn init_default_roots(&self) -> bool {
        if !self.has_roots_defined() {
            self.add_update_root_set(RootSet::zerotier_default())
        } else {
            false
        }
    }

    /// Get the root sets that this node trusts.
    pub fn root_sets(&self) -> Vec<RootSet> {
        self.roots.read().sets.values().cloned().map(|s| s.unwrap()).collect()
    }

    /// Get the canonical Path object corresponding to an endpoint.
    pub(crate) fn canonical_path(
        &self,
        ep: &Endpoint,
        local_socket: &HostSystemImpl::LocalSocket,
        local_interface: &HostSystemImpl::LocalInterface,
        time_ticks: i64,
    ) -> Arc<Path<HostSystemImpl>> {
        if let Some(path) = self.paths.read().get(&PathKey::Ref(ep, local_socket)) {
            path.clone()
        } else {
            self.paths
                .write()
                .entry(PathKey::Copied(ep.clone(), local_socket.clone()))
                .or_insert_with(|| Arc::new(Path::new(ep.clone(), local_socket.clone(), local_interface.clone(), time_ticks)))
                .clone()
        }
    }
}

/// Key used to look up paths in a hash map
/// This supports copied keys for storing and refs for fast lookup without having to copy anything.
enum PathKey<'a, 'b, HostSystemImpl: HostSystem> {
    Copied(Endpoint, HostSystemImpl::LocalSocket),
    Ref(&'a Endpoint, &'b HostSystemImpl::LocalSocket),
}

impl<'a, 'b, HostSystemImpl: HostSystem> Hash for PathKey<'a, 'b, HostSystemImpl> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Copied(ep, ls) => {
                ep.hash(state);
                ls.hash(state);
            }
            Self::Ref(ep, ls) => {
                (*ep).hash(state);
                (*ls).hash(state);
            }
        }
    }
}

impl<HostSystemImpl: HostSystem> PartialEq for PathKey<'_, '_, HostSystemImpl> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Copied(ep1, ls1), Self::Copied(ep2, ls2)) => ep1.eq(ep2) && ls1.eq(ls2),
            (Self::Copied(ep1, ls1), Self::Ref(ep2, ls2)) => ep1.eq(*ep2) && ls1.eq(*ls2),
            (Self::Ref(ep1, ls1), Self::Copied(ep2, ls2)) => (*ep1).eq(ep2) && (*ls1).eq(ls2),
            (Self::Ref(ep1, ls1), Self::Ref(ep2, ls2)) => (*ep1).eq(*ep2) && (*ls1).eq(*ls2),
        }
    }
}

impl<HostSystemImpl: HostSystem> Eq for PathKey<'_, '_, HostSystemImpl> {}

impl<'a, 'b, HostSystemImpl: HostSystem> PathKey<'a, 'b, HostSystemImpl> {
    #[inline(always)]
    fn local_socket(&self) -> &HostSystemImpl::LocalSocket {
        match self {
            Self::Copied(_, ls) => ls,
            Self::Ref(_, ls) => *ls,
        }
    }

    #[inline(always)]
    fn to_copied(&self) -> PathKey<'static, 'static, HostSystemImpl> {
        match self {
            Self::Copied(ep, ls) => PathKey::<'static, 'static, HostSystemImpl>::Copied(ep.clone(), ls.clone()),
            Self::Ref(ep, ls) => PathKey::<'static, 'static, HostSystemImpl>::Copied((*ep).clone(), (*ls).clone()),
        }
    }
}

/// Dummy no-op inner protocol for debugging and testing.
#[derive(Default)]
pub struct DummyInnerProtocol;

impl InnerProtocol for DummyInnerProtocol {
    #[inline(always)]
    fn handle_packet<HostSystemImpl: HostSystem>(
        &self,
        _source: &Arc<Peer<HostSystemImpl>>,
        _source_path: &Arc<Path<HostSystemImpl>>,
        _verb: u8,
        _payload: &PacketBuffer,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    #[inline(always)]
    fn handle_error<HostSystemImpl: HostSystem>(
        &self,
        _source: &Arc<Peer<HostSystemImpl>>,
        _source_path: &Arc<Path<HostSystemImpl>>,
        _in_re_verb: u8,
        _in_re_message_id: u64,
        _error_code: u8,
        _payload: &PacketBuffer,
        _cursor: &mut usize,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    #[inline(always)]
    fn handle_ok<HostSystemImpl: HostSystem>(
        &self,
        _source: &Arc<Peer<HostSystemImpl>>,
        _source_path: &Arc<Path<HostSystemImpl>>,
        _in_re_verb: u8,
        _in_re_message_id: u64,
        _payload: &PacketBuffer,
        _cursor: &mut usize,
    ) -> PacketHandlerResult {
        PacketHandlerResult::NotHandled
    }

    #[inline(always)]
    fn should_communicate_with(&self, _id: &Identity) -> bool {
        true
    }
}

/// Dummy no-op path filter for debugging and testing.
#[derive(Default)]
pub struct DummyPathFilter;

impl PathFilter for DummyPathFilter {
    #[inline(always)]
    fn check_path<HostSystemImpl: HostSystem>(
        &self,
        _id: &Identity,
        _endpoint: &Endpoint,
        _local_socket: Option<&<HostSystemImpl as HostSystem>::LocalSocket>,
        _local_interface: Option<&<HostSystemImpl as HostSystem>::LocalInterface>,
    ) -> bool {
        true
    }

    #[inline(always)]
    fn get_path_hints<HostSystemImpl: HostSystem>(
        &self,
        _id: &Identity,
    ) -> Option<
        Vec<(
            Endpoint,
            Option<<HostSystemImpl as HostSystem>::LocalSocket>,
            Option<<HostSystemImpl as HostSystem>::LocalInterface>,
        )>,
    > {
        None
    }
}