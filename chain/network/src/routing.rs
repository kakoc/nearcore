use std::collections::{hash_map::Entry, HashMap, HashSet, VecDeque};
use std::ops::Sub;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cached::{Cached, SizedCache};
use chrono;
use tracing::{trace, warn};

use crate::cache::RouteBackCache;
use near_metrics;
use near_primitives::hash::CryptoHash;
use near_primitives::network::{AnnounceAccount, PeerId};
use near_primitives::types::AccountId;
use near_primitives::utils::index_to_bytes;
use near_store::{
    ColAccountAnnouncements, ColComponentEdges, ColLastComponentNonce, ColPeerComponent, Store,
    StoreUpdate,
};

use crate::metrics;
use crate::{
    types::{PeerIdOrHash, Ping, Pong},
    utils::cache_to_hashmap,
};
use conqueue::{QueueReceiver, QueueSender};
#[cfg(feature = "delay_detector")]
use delay_detector::DelayDetector;

pub use near_network_primitives::routing::*;

const ANNOUNCE_ACCOUNT_CACHE_SIZE: usize = 10_000;
const ROUTE_BACK_CACHE_SIZE: u64 = 100_000;
const ROUTE_BACK_CACHE_EVICT_TIMEOUT: u64 = 120_000; // 120 seconds
const ROUTE_BACK_CACHE_REMOVE_BATCH: u64 = 100;
const PING_PONG_CACHE_SIZE: usize = 1_000;
const ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED: usize = 10;
const ROUND_ROBIN_NONCE_CACHE_SIZE: usize = 10_000;

pub struct EdgeVerifierHelper {
    /// Shared version of edges_info used by multiple threads
    pub edges_info_shared: Arc<Mutex<HashMap<(PeerId, PeerId), u64>>>,
    /// Queue of edges verified, but not added yes
    pub edges_to_add_receiver: QueueReceiver<Edge>,
    pub edges_to_add_sender: QueueSender<Edge>,
}

impl Default for EdgeVerifierHelper {
    fn default() -> Self {
        let (tx, rx) = conqueue::Queue::unbounded::<Edge>();
        Self {
            edges_info_shared: Default::default(),
            edges_to_add_sender: tx,
            edges_to_add_receiver: rx,
        }
    }
}

pub struct RoutingTable {
    /// PeerId associated for every known account id.
    account_peers: SizedCache<AccountId, AnnounceAccount>,
    /// Active PeerId that are part of the shortest path to each PeerId.
    pub peer_forwarding: HashMap<PeerId, Vec<PeerId>>,
    /// Store last update for known edges.
    pub edges_info: HashMap<(PeerId, PeerId), Edge>,
    /// Hash of messages that requires routing back to respective previous hop.
    pub route_back: RouteBackCache,
    /// Last time a peer with reachable through active edges.
    pub peer_last_time_reachable: HashMap<PeerId, chrono::DateTime<chrono::Utc>>,
    /// Access to store on disk
    store: Arc<Store>,
    /// Current view of the network. Nodes are Peers and edges are active connections.
    raw_graph: Graph,
    /// Number of times each active connection was used to route a message.
    /// If there are several options use route with minimum nonce.
    /// New routes are added with minimum nonce.
    route_nonce: SizedCache<PeerId, usize>,
    /// Ping received by nonce.
    ping_info: SizedCache<usize, Ping>,
    /// Ping received by nonce.
    pong_info: SizedCache<usize, Pong>,
    /// List of pings sent for which we haven't received any pong yet.
    waiting_pong: SizedCache<PeerId, SizedCache<usize, Instant>>,
    /// Last nonce sent to each peer through pings.
    last_ping_nonce: SizedCache<PeerId, usize>,
    /// Last nonce used to store edges on disk.
    pub component_nonce: u64,
}

#[derive(Debug)]
pub enum FindRouteError {
    Disconnected,
    PeerNotFound,
    AccountNotFound,
    RouteBackNotFound,
}

impl RoutingTable {
    pub fn new(peer_id: PeerId, store: Arc<Store>) -> Self {
        // Find greater nonce on disk and set `component_nonce` to this value.
        let component_nonce = store
            .get_ser::<u64>(ColLastComponentNonce, &[])
            .unwrap_or(None)
            .map_or(0, |nonce| nonce + 1);

        Self {
            account_peers: SizedCache::with_size(ANNOUNCE_ACCOUNT_CACHE_SIZE),
            peer_forwarding: Default::default(),
            edges_info: Default::default(),
            route_back: RouteBackCache::new(
                ROUTE_BACK_CACHE_SIZE,
                ROUTE_BACK_CACHE_EVICT_TIMEOUT,
                ROUTE_BACK_CACHE_REMOVE_BATCH,
            ),
            peer_last_time_reachable: Default::default(),
            store,
            raw_graph: Graph::new(peer_id),
            route_nonce: SizedCache::with_size(ROUND_ROBIN_NONCE_CACHE_SIZE),
            ping_info: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            pong_info: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            waiting_pong: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            last_ping_nonce: SizedCache::with_size(PING_PONG_CACHE_SIZE),
            component_nonce,
        }
    }

    fn peer_id(&self) -> &PeerId {
        &self.raw_graph.source
    }

    pub fn reachable_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.peer_forwarding.keys()
    }

    /// Find peer that is connected to `source` and belong to the shortest path
    /// from `source` to `peer_id`.
    pub fn find_route_from_peer_id(&mut self, peer_id: &PeerId) -> Result<PeerId, FindRouteError> {
        if let Some(routes) = self.peer_forwarding.get(&peer_id).cloned() {
            if routes.is_empty() {
                return Err(FindRouteError::Disconnected);
            }

            // Strategy similar to Round Robin. Select node with least nonce and send it. Increase its
            // nonce by one. Additionally if the difference between the highest nonce and the lowest
            // nonce is greater than some threshold increase the lowest nonce to be at least
            // max nonce - threshold.
            let nonce_peer = routes
                .iter()
                .map(|peer_id| {
                    (self.route_nonce.cache_get(&peer_id).cloned().unwrap_or(0), peer_id)
                })
                .collect::<Vec<_>>();

            // Neighbor with minimum and maximum nonce respectively.
            let min_v = nonce_peer.iter().min().cloned().unwrap();
            let max_v = nonce_peer.into_iter().max().unwrap();

            if min_v.0 + ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED < max_v.0 {
                self.route_nonce
                    .cache_set(min_v.1.clone(), max_v.0 - ROUND_ROBIN_MAX_NONCE_DIFFERENCE_ALLOWED);
            }

            let next_hop = min_v.1;
            let nonce = self.route_nonce.cache_get(&next_hop).cloned();
            self.route_nonce.cache_set(next_hop.clone(), nonce.map_or(1, |nonce| nonce + 1));
            Ok(next_hop.clone())
        } else {
            Err(FindRouteError::PeerNotFound)
        }
    }

    pub fn find_route(&mut self, target: &PeerIdOrHash) -> Result<PeerId, FindRouteError> {
        match target {
            PeerIdOrHash::PeerId(peer_id) => self.find_route_from_peer_id(&peer_id),
            PeerIdOrHash::Hash(hash) => {
                self.fetch_route_back(hash.clone()).ok_or(FindRouteError::RouteBackNotFound)
            }
        }
    }

    /// Find peer that owns this AccountId.
    pub fn account_owner(&mut self, account_id: &AccountId) -> Result<PeerId, FindRouteError> {
        self.get_announce(account_id)
            .map(|announce_account| announce_account.peer_id)
            .ok_or_else(|| FindRouteError::AccountNotFound)
    }

    /// Add (account id, peer id) to routing table.
    /// Note: There is at most on peer id per account id.
    pub fn add_account(&mut self, announce_account: AnnounceAccount) {
        let account_id = announce_account.account_id.clone();
        self.account_peers.cache_set(account_id.clone(), announce_account.clone());

        // Add account to store
        let mut update = self.store.store_update();
        if let Err(e) = update
            .set_ser(ColAccountAnnouncements, account_id.as_ref().as_bytes(), &announce_account)
            .and_then(|_| update.commit())
        {
            warn!(target: "network", "Error saving announce account to store: {:?}", e);
        }
    }

    // TODO(MarX, #1694): Allow one account id to be routed to several peer id.
    pub fn contains_account(&mut self, announce_account: &AnnounceAccount) -> bool {
        self.get_announce(&announce_account.account_id).map_or(false, |current_announce_account| {
            current_announce_account.epoch_id == announce_account.epoch_id
        })
    }

    /// Get the nonce of the component where the peer was stored
    fn component_nonce_from_peer(&mut self, peer_id: PeerId) -> Result<u64, ()> {
        match self.store.get_ser::<u64>(ColPeerComponent, Vec::from(peer_id).as_ref()) {
            Ok(Some(nonce)) => Ok(nonce),
            _ => Err(()),
        }
    }

    /// Get all edges in the component with `nonce`
    /// Remove those edges from the store.
    fn get_component_edges(
        &mut self,
        nonce: u64,
        update: &mut StoreUpdate,
    ) -> Result<Vec<Edge>, ()> {
        let enc_nonce = index_to_bytes(nonce);

        let res = match self.store.get_ser::<Vec<Edge>>(ColComponentEdges, enc_nonce.as_ref()) {
            Ok(Some(edges)) => Ok(edges),
            _ => Err(()),
        };

        update.delete(ColComponentEdges, enc_nonce.as_ref());

        res
    }

    /// If peer_id is not on memory check if it is on disk in bring it back on memory.
    fn touch(&mut self, peer_id: &PeerId) {
        if peer_id == self.peer_id() || self.peer_last_time_reachable.contains_key(peer_id) {
            return;
        }

        let me = self.peer_id().clone();

        if let Ok(nonce) = self.component_nonce_from_peer(peer_id.clone()) {
            let mut update = self.store.store_update();

            if let Ok(edges) = self.get_component_edges(nonce, &mut update) {
                for edge in edges {
                    for &peer_id in vec![&edge.peer0, &edge.peer1].iter() {
                        if peer_id == &me || self.peer_last_time_reachable.contains_key(peer_id) {
                            continue;
                        }

                        if let Ok(cur_nonce) = self.component_nonce_from_peer(peer_id.clone()) {
                            if cur_nonce == nonce {
                                self.peer_last_time_reachable.insert(
                                    peer_id.clone(),
                                    chrono::Utc::now()
                                        .sub(chrono::Duration::seconds(SAVE_PEERS_MAX_TIME as i64)),
                                );
                                update
                                    .delete(ColPeerComponent, Vec::from(peer_id.clone()).as_ref());
                            }
                        }
                    }
                    self.add_edge(edge);
                }
            }

            if let Err(e) = update.commit() {
                warn!(target: "network", "Error removing network component from store. {:?}", e);
            }
        } else {
            self.peer_last_time_reachable.insert(peer_id.clone(), chrono::Utc::now());
        }
    }

    fn add_edge(&mut self, edge: Edge) -> bool {
        let key = (edge.peer0.clone(), edge.peer1.clone());

        if self.find_nonce(&key) >= edge.nonce {
            // We already have a newer information about this edge. Discard this information.
            false
        } else {
            match edge.edge_type() {
                EdgeType::Added => {
                    self.raw_graph.add_edge(key.0.clone(), key.1.clone());
                }
                EdgeType::Removed => {
                    self.raw_graph.remove_edge(&key.0, &key.1);
                }
            }
            self.edges_info.insert(key, edge);
            true
        }
    }

    /// Add several edges to the current view of the network.
    /// These edges are assumed to be valid at this point.
    /// Return true if some of the edges contains new information to the network.
    pub fn process_edges(&mut self, edges: Vec<Edge>) -> ProcessEdgeResult {
        let mut new_edge = false;
        let total = edges.len();

        for edge in edges {
            let key = (&edge.peer0, &edge.peer1);

            self.touch(&key.0);
            self.touch(&key.1);

            if self.add_edge(edge) {
                new_edge = true;
            }
        }

        // Update metrics after edge update
        near_metrics::inc_counter_by(&metrics::EDGE_UPDATES, total as u64);
        near_metrics::set_gauge(&metrics::EDGE_ACTIVE, self.raw_graph.total_active_edges as i64);

        ProcessEdgeResult { new_edge }
    }

    pub fn find_nonce(&self, edge: &(PeerId, PeerId)) -> u64 {
        self.edges_info.get(&edge).map_or(0, |x| x.nonce)
    }

    pub fn get_edge(&self, peer0: PeerId, peer1: PeerId) -> Option<Edge> {
        let key = Edge::key(peer0, peer1);
        self.edges_info.get(&key).cloned()
    }

    pub fn get_edges(&self) -> Vec<Edge> {
        self.edges_info.iter().map(|(_, edge)| edge.clone()).collect()
    }

    pub fn add_route_back(&mut self, hash: CryptoHash, peer_id: PeerId) {
        self.route_back.insert(hash, peer_id);
    }

    // Find route back with given hash and removes it from cache.
    fn fetch_route_back(&mut self, hash: CryptoHash) -> Option<PeerId> {
        self.route_back.remove(&hash)
    }

    pub fn compare_route_back(&mut self, hash: CryptoHash, peer_id: &PeerId) -> bool {
        self.route_back.get(&hash).map_or(false, |value| value == peer_id)
    }

    pub fn add_ping(&mut self, ping: Ping) {
        self.ping_info.cache_set(ping.nonce as usize, ping);
    }

    /// Return time of the round trip of ping + pong
    pub fn add_pong(&mut self, pong: Pong) -> Option<f64> {
        let mut res = None;

        if let Some(nonces) = self.waiting_pong.cache_get_mut(&pong.source) {
            res = nonces
                .cache_remove(&(pong.nonce as usize))
                .and_then(|sent| Some(Instant::now().duration_since(sent).as_secs_f64() * 1000f64));
        }

        self.pong_info.cache_set(pong.nonce as usize, pong);

        res
    }

    pub fn sending_ping(&mut self, nonce: usize, target: PeerId) {
        let entry = if let Some(entry) = self.waiting_pong.cache_get_mut(&target) {
            entry
        } else {
            self.waiting_pong.cache_set(target.clone(), SizedCache::with_size(10));
            self.waiting_pong.cache_get_mut(&target).unwrap()
        };

        entry.cache_set(nonce, Instant::now());
    }

    pub fn get_ping(&mut self, peer_id: PeerId) -> usize {
        if let Some(entry) = self.last_ping_nonce.cache_get_mut(&peer_id) {
            *entry += 1;
            *entry - 1
        } else {
            self.last_ping_nonce.cache_set(peer_id, 1);
            0
        }
    }

    pub fn fetch_ping_pong(&self) -> (HashMap<usize, Ping>, HashMap<usize, Pong>) {
        (cache_to_hashmap(&self.ping_info), cache_to_hashmap(&self.pong_info))
    }

    pub fn info(&mut self) -> RoutingTableInfo {
        let account_peers = self
            .get_announce_accounts()
            .into_iter()
            .map(|announce_account| (announce_account.account_id, announce_account.peer_id))
            .collect();
        RoutingTableInfo { account_peers, peer_forwarding: self.peer_forwarding.clone() }
    }

    fn try_save_edges(&mut self) {
        let now = chrono::Utc::now();
        let mut oldest_time = now;
        let to_save = self
            .peer_last_time_reachable
            .iter()
            .filter_map(|(peer_id, last_time)| {
                oldest_time = std::cmp::min(oldest_time, *last_time);
                if now.signed_duration_since(*last_time).num_seconds()
                    >= SAVE_PEERS_AFTER_TIME as i64
                {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();

        // Save nodes on disk and remove from memory only if elapsed time from oldest peer
        // is greater than `SAVE_PEERS_MAX_TIME`
        if now.signed_duration_since(oldest_time).num_seconds() < SAVE_PEERS_MAX_TIME as i64 {
            return;
        }

        let component_nonce = self.component_nonce;
        self.component_nonce += 1;

        let mut update = self.store.store_update();
        let _ = update.set_ser(ColLastComponentNonce, &[], &component_nonce);

        for peer_id in to_save.iter() {
            let _ = update.set_ser(
                ColPeerComponent,
                Vec::from(peer_id.clone()).as_ref(),
                &component_nonce,
            );

            self.peer_last_time_reachable.remove(peer_id);
        }

        let component_nonce = index_to_bytes(component_nonce);
        let mut edges_in_component = vec![];

        self.edges_info.retain(|(peer0, peer1), edge| {
            if to_save.contains(peer0) || to_save.contains(peer1) {
                edges_in_component.push(edge.clone());
                false
            } else {
                true
            }
        });

        let _ = update.set_ser(ColComponentEdges, component_nonce.as_ref(), &edges_in_component);

        if let Err(e) = update.commit() {
            warn!(target: "network", "Error storing network component to store. {:?}", e);
        }
    }

    /// Recalculate routing table.
    pub fn update(&mut self, can_save_edges: bool) {
        #[cfg(feature = "delay_detector")]
        let _d = DelayDetector::new("routing table update".into());
        let _routing_table_recalculation =
            near_metrics::start_timer(&metrics::ROUTING_TABLE_RECALCULATION_HISTOGRAM);

        trace!(target: "network", "Update routing table.");

        self.peer_forwarding = self.raw_graph.calculate_distance();

        let now = chrono::Utc::now();
        for peer in self.peer_forwarding.keys() {
            self.peer_last_time_reachable.insert(peer.clone(), now);
        }

        if can_save_edges {
            self.try_save_edges();
        }

        near_metrics::inc_counter_by(&metrics::ROUTING_TABLE_RECALCULATIONS, 1);
        near_metrics::set_gauge(&metrics::PEER_REACHABLE, self.peer_forwarding.len() as i64);
    }

    /// Public interface for `account_peers`
    ///
    /// Get keys currently on cache.
    pub fn get_accounts_keys(&mut self) -> Vec<AccountId> {
        self.account_peers.key_order().cloned().collect()
    }

    /// Get announce accounts on cache.
    pub fn get_announce_accounts(&mut self) -> Vec<AnnounceAccount> {
        self.account_peers.value_order().cloned().collect()
    }

    /// Get account announce from
    pub fn get_announce(&mut self, account_id: &AccountId) -> Option<AnnounceAccount> {
        if let Some(announce_account) = self.account_peers.cache_get(&account_id) {
            Some(announce_account.clone())
        } else {
            self.store
                .get_ser(ColAccountAnnouncements, account_id.as_ref().as_bytes())
                .and_then(|res: Option<AnnounceAccount>| {
                    if let Some(announce_account) = res {
                        self.add_account(announce_account.clone());
                        Ok(Some(announce_account))
                    } else {
                        Ok(None)
                    }
                })
                .unwrap_or_else(|e| {
                    warn!(target: "network", "Error loading announce account from store: {:?}", e);
                    None
                })
        }
    }

    #[cfg(feature = "metric_recorder")]
    pub fn get_raw_graph(&self) -> HashMap<PeerId, HashSet<PeerId>> {
        let mut res = HashMap::with_capacity(self.raw_graph.adjacency.len());
        for (key, neighbors) in self.raw_graph.adjacency.iter().enumerate() {
            if self.raw_graph.used[key] {
                let key = self.raw_graph.id2p[key].clone();
                let neighbors = neighbors
                    .iter()
                    .map(|&node| self.raw_graph.id2p[node as usize].clone())
                    .collect::<HashSet<_>>();
                res.insert(key, neighbors);
            }
        }
        res
    }
}

pub struct ProcessEdgeResult {
    pub new_edge: bool,
}

#[derive(Clone)]
pub struct Graph {
    pub source: PeerId,
    source_id: u32,
    p2id: HashMap<PeerId, u32>,
    id2p: Vec<PeerId>,
    used: Vec<bool>,
    unused: Vec<u32>,
    adjacency: Vec<Vec<u32>>,

    total_active_edges: u64,
}

impl Graph {
    pub fn new(source: PeerId) -> Self {
        let mut res = Self {
            source: source.clone(),
            source_id: 0,
            p2id: HashMap::default(),
            id2p: Vec::default(),
            used: Vec::default(),
            unused: Vec::default(),
            adjacency: Vec::default(),
            total_active_edges: 0,
        };
        res.id2p.push(source.clone());
        res.adjacency.push(Vec::default());
        res.p2id.insert(source, res.source_id);
        res.used.push(true);

        res
    }

    fn contains_edge(&self, peer0: &PeerId, peer1: &PeerId) -> bool {
        if let Some(&id0) = self.p2id.get(&peer0) {
            if let Some(&id1) = self.p2id.get(&peer1) {
                return self.adjacency[id0 as usize].contains(&id1);
            }
        }
        false
    }

    fn remove_if_unused(&mut self, id: u32) {
        let entry = &self.adjacency[id as usize];

        if entry.is_empty() && id != self.source_id {
            self.used[id as usize] = false;
            self.unused.push(id);
            self.p2id.remove(&self.id2p[id as usize]);
        }
    }

    fn get_id(&mut self, peer: &PeerId) -> u32 {
        match self.p2id.entry(peer.clone()) {
            Entry::Occupied(occupied) => *occupied.get(),
            Entry::Vacant(vacant) => {
                let val = if let Some(val) = self.unused.pop() {
                    assert!(!self.used[val as usize]);
                    assert!(self.adjacency[val as usize].is_empty());
                    self.id2p[val as usize] = peer.clone();
                    self.used[val as usize] = true;
                    val
                } else {
                    let val = self.id2p.len() as u32;
                    self.id2p.push(peer.clone());
                    self.used.push(true);
                    self.adjacency.push(Vec::default());
                    val
                };

                vacant.insert(val);
                val
            }
        }
    }

    pub fn add_edge(&mut self, peer0: PeerId, peer1: PeerId) {
        assert_ne!(peer0, peer1);
        if !self.contains_edge(&peer0, &peer1) {
            let id0 = self.get_id(&peer0);
            let id1 = self.get_id(&peer1);

            self.adjacency[id0 as usize].push(id1);
            self.adjacency[id1 as usize].push(id0);

            self.total_active_edges += 1;
        }
    }

    pub fn remove_edge(&mut self, peer0: &PeerId, peer1: &PeerId) {
        assert_ne!(peer0, peer1);
        if self.contains_edge(&peer0, &peer1) {
            let id0 = self.get_id(&peer0);
            let id1 = self.get_id(&peer1);

            self.adjacency[id0 as usize].retain(|&x| x != id1);
            self.adjacency[id1 as usize].retain(|&x| x != id0);

            self.remove_if_unused(id0);
            self.remove_if_unused(id1);

            self.total_active_edges -= 1;
        }
    }

    /// Compute for every node `u` on the graph (other than `source`) which are the neighbors of
    /// `sources` which belong to the shortest path from `source` to `u`. Nodes that are
    /// not connected to `source` will not appear in the result.
    pub fn calculate_distance(&self) -> HashMap<PeerId, Vec<PeerId>> {
        // TODO add removal of unreachable nodes

        let mut queue = VecDeque::new();

        let nodes = self.id2p.len();
        let mut distance: Vec<i32> = vec![-1; nodes];
        let mut routes: Vec<u128> = vec![0; nodes];

        distance[self.source_id as usize] = 0;

        {
            let neighbors = &self.adjacency[self.source_id as usize];
            for (id, &neighbor) in neighbors.iter().enumerate().take(MAX_NUM_PEERS) {
                queue.push_back(neighbor);
                distance[neighbor as usize] = 1;
                routes[neighbor as usize] = 1u128 << id;
            }
        }

        while let Some(cur_peer) = queue.pop_front() {
            let cur_distance = distance[cur_peer as usize];

            for &neighbor in &self.adjacency[cur_peer as usize] {
                if distance[neighbor as usize] == -1 {
                    distance[neighbor as usize] = cur_distance + 1;
                    queue.push_back(neighbor);
                }
                // If this edge belong to a shortest path, all paths to
                // the closer nodes are also valid for the current node.
                if distance[neighbor as usize] == cur_distance + 1 {
                    routes[neighbor as usize] |= routes[cur_peer as usize];
                }
            }
        }

        self.compute_result(&mut routes, &distance)
    }

    fn compute_result(&self, routes: &[u128], distance: &[i32]) -> HashMap<PeerId, Vec<PeerId>> {
        let mut res = HashMap::with_capacity(routes.len());

        let neighbors = &self.adjacency[self.source_id as usize];
        let mut unreachable_nodes = 0;

        for (key, &cur_route) in routes.iter().enumerate() {
            if distance[key] == -1 && self.used[key] {
                unreachable_nodes += 1;
            }
            if key as u32 == self.source_id
                || distance[key] == -1
                || cur_route == 0u128
                || !self.used[key]
            {
                continue;
            }
            let mut peer_set: Vec<PeerId> = Vec::with_capacity(cur_route.count_ones() as usize);

            for (id, &neighbor) in neighbors.iter().enumerate().take(MAX_NUM_PEERS) {
                if (cur_route & (1u128 << id)) != 0 {
                    peer_set.push(self.id2p[neighbor as usize].clone());
                };
            }
            res.insert(self.id2p[key].clone(), peer_set);
        }
        if unreachable_nodes > 1000 {
            warn!("We store more than 1000 unreachable nodes: {}", unreachable_nodes);
        }
        res
    }
}

#[cfg(test)]
mod test {
    use crate::routing::Graph;
    use crate::test_utils::{expected_routing_tables, random_peer_id};

    #[test]
    fn graph_contains_edge() {
        let source = random_peer_id();

        let node0 = random_peer_id();
        let node1 = random_peer_id();

        let mut graph = Graph::new(source.clone());

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);

        graph.add_edge(node0.clone(), node1.clone());

        assert_eq!(graph.contains_edge(&source, &node0), false);
        assert_eq!(graph.contains_edge(&source, &node1), false);
        assert_eq!(graph.contains_edge(&node0, &node1), true);
        assert_eq!(graph.contains_edge(&node1, &node0), true);

        graph.remove_edge(&node1, &node0);

        assert_eq!(graph.contains_edge(&node0, &node1), false);
        assert_eq!(graph.contains_edge(&node1, &node0), false);
    }

    #[test]
    fn graph_distance0() {
        let source = random_peer_id();
        let node0 = random_peer_id();

        let mut graph = Graph::new(source.clone());
        graph.add_edge(source.clone(), node0.clone());
        graph.remove_edge(&source, &node0);
        graph.add_edge(source.clone(), node0.clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![(node0.clone(), vec![node0.clone()])],
        ));
    }

    #[test]
    fn graph_distance1() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[1].clone(), nodes[2].clone());

        assert!(expected_routing_tables(graph.calculate_distance(), vec![]));
    }

    #[test]
    fn graph_distance2() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[1].clone(), nodes[2].clone());
        graph.add_edge(source.clone(), nodes[0].clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[0].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone()]),
            ],
        ));
    }

    #[test]
    fn graph_distance3() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..3).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        graph.add_edge(nodes[0].clone(), nodes[1].clone());
        graph.add_edge(nodes[2].clone(), nodes[1].clone());
        graph.add_edge(nodes[0].clone(), nodes[2].clone());
        graph.add_edge(source.clone(), nodes[0].clone());
        graph.add_edge(source.clone(), nodes[1].clone());

        assert!(expected_routing_tables(
            graph.calculate_distance(),
            vec![
                (nodes[0].clone(), vec![nodes[0].clone()]),
                (nodes[1].clone(), vec![nodes[1].clone()]),
                (nodes[2].clone(), vec![nodes[0].clone(), nodes[1].clone()]),
            ],
        ));
    }

    /// Test the following graph
    ///     0 - 3 - 6
    ///   /   x   x
    /// s - 1 - 4 - 7
    ///   \   x   x
    ///     2 - 5 - 8
    ///
    ///    9 - 10 (Dummy edge disconnected)
    ///
    /// There is a shortest path to nodes [3..9) going through 0, 1, and 2.
    #[test]
    fn graph_distance4() {
        let source = random_peer_id();
        let nodes: Vec<_> = (0..11).map(|_| random_peer_id()).collect();

        let mut graph = Graph::new(source.clone());

        for i in 0..3 {
            graph.add_edge(source.clone(), nodes[i].clone());
        }

        for level in 0..2 {
            for i in 0..3 {
                for j in 0..3 {
                    graph.add_edge(nodes[level * 3 + i].clone(), nodes[level * 3 + 3 + j].clone());
                }
            }
        }

        // Dummy edge.
        graph.add_edge(nodes[9].clone(), nodes[10].clone());

        let mut next_hops: Vec<_> =
            (0..3).map(|i| (nodes[i].clone(), vec![nodes[i].clone()])).collect();
        let target: Vec<_> = (0..3).map(|i| nodes[i].clone()).collect();

        for i in 3..9 {
            next_hops.push((nodes[i].clone(), target.clone()));
        }

        assert!(expected_routing_tables(graph.calculate_distance(), next_hops));
    }
}
