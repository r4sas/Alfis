use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, IpAddr, Shutdown, ToSocketAddrs};
use mio::{Token, Interest, Registry};
use mio::net::TcpStream;
use crate::p2p::{Peer, State, Message};
use crate::p2p::network::LISTEN_PORT;
use crate::p2p::network::next;
use rand::random;
use rand::seq::IteratorRandom;
#[allow(unused_imports)]
use log::{trace, debug, info, warn, error};
use crate::{Bytes, is_yggdrasil, commons};
use crate::commons::MAX_RECONNECTS;

pub struct Peers {
    peers: HashMap<Token, Peer>,
    new_peers: Vec<SocketAddr>,
    ignored: HashSet<IpAddr>,
    my_id: String
}

const PING_PERIOD: u64 = 60;

impl Peers {
    pub fn new() -> Self {
        Peers { peers: HashMap::new(), new_peers: Vec::new(), ignored: HashSet::new(), my_id: commons::random_string(6) }
    }

    pub fn add_peer(&mut self, token: Token, peer: Peer) {
        self.peers.insert(token, peer);
    }

    pub fn get_peer(&self, token: &Token) -> Option<&Peer> {
        self.peers.get(token)
    }

    pub fn get_mut_peer(&mut self, token: &Token) -> Option<&mut Peer> {
        self.peers.get_mut(token)
    }

    pub fn close_peer(&mut self, registry: &Registry, token: &Token) {
        let peer = self.peers.get_mut(token);
        match peer {
            Some(peer) => {
                let stream = peer.get_stream();
                let _ = stream.shutdown(Shutdown::Both);
                let _ = registry.deregister(stream);
                match peer.get_state() {
                    State::Connecting => {
                        debug!("Peer connection {} to {:?} has timed out", &token.0, &peer.get_addr());
                    }
                    State::Connected => {
                        debug!("Peer connection {} to {:?} disconnected", &token.0, &peer.get_addr());
                    }
                    State::Idle { .. } | State::Message { .. } => {
                        debug!("Peer connection {} to {:?} disconnected", &token.0, &peer.get_addr());
                    }
                    State::Error => {
                        debug!("Peer connection {} to {:?} has shut down on error", &token.0, &peer.get_addr());
                    }
                    State::Banned => {
                        debug!("Peer connection {} to {:?} has shut down, banned", &token.0, &peer.get_addr());
                    }
                    State::Offline { .. } => {
                        debug!("Peer connection {} to {:?} is offline", &token.0, &peer.get_addr());
                    }
                }

                if !peer.disabled() && !peer.is_inbound() {
                    peer.set_state(State::offline());
                    peer.set_active(false);
                } else {
                    self.peers.remove(token);
                }
            }
            None => {}
        }
    }

    pub fn add_peers_from_exchange(&mut self, peers: Vec<String>) {
        let peers: HashSet<String> = peers
            .iter()
            .fold(HashSet::new(), |mut peers, peer| {
                peers.insert(peer.to_owned());
                peers
            });
        debug!("Got {} peers: {:?}", peers.len(), &peers);
        // TODO make it return error if these peers are wrong and seem like an attack
        for peer in peers.iter() {
            let addr: SocketAddr = match peer.parse() {
                Err(_) => {
                    warn!("Error parsing peer {}", peer);
                    continue;
                }
                Ok(addr) => addr
            };

            if self.peers
                .iter()
                .find(|(_token, peer)| peer.get_addr().ip() == addr.ip())
                .is_some() {
                //debug!("Skipping address from exchange: {}", &addr);
                continue;
            }

            if self.new_peers
                .iter()
                .find(|a| a.ip().eq(&addr.ip()))
                .is_some() {
                //debug!("Skipping address from exchange: {}", &addr);
                continue;
            }

            if self.ignored.contains(&addr.ip()) {
                trace!("Skipping address from exchange: {}", &addr);
                continue;
            }

            if skip_private_addr(&addr) {
                //debug!("Skipping address from exchange: {}", &addr);
                continue; // Return error in future
            }
            let mut found = false;
            for (_token, p) in self.peers.iter() {
                if p.equals(&addr) {
                    found = true;
                    break;
                }
            }
            if found {
                continue;
            }
            self.new_peers.push(addr);
        }
    }

    pub fn get_my_id(&self) -> &str {
        &self.my_id
    }

    pub fn is_our_own_connect(&self, rand: &str) -> bool {
        self.my_id.eq(rand)
    }

    pub fn get_peers_for_exchange(&self, peer_address: &SocketAddr) -> Vec<String> {
        let mut result: Vec<String> = Vec::new();
        for (_, peer) in self.peers.iter() {
            if peer.disabled() {
                continue;
            }
            if peer.equals(peer_address) {
                continue;
            }
            if peer.is_public() {
                result.push(SocketAddr::new(peer.get_addr().ip(), LISTEN_PORT).to_string());
            }
        }
        result
    }

    pub fn get_peers_active_count(&self) -> usize {
        let mut count = 0;
        for (_, peer) in self.peers.iter() {
            if peer.active() {
                count += 1;
            }
        }
        count
    }

    pub fn ignore_peer(&mut self, registry: &Registry, token: &Token) {
        let peer = self.peers.get_mut(token).unwrap();
        peer.set_state(State::Banned);
        let ip = peer.get_addr().ip().clone();
        self.close_peer(registry, token);
        self.ignored.insert(ip);
        match self.peers
            .iter()
            .find(|(_, p)| p.get_addr().ip() == ip)
            .map(|(t, _)| t.clone()) {
            None => {}
            Some(t) => {
                self.close_peer(registry, &t);
                self.peers.remove(&t);
            }
        }
    }

    pub fn ignore_ip(&mut self, ip: &IpAddr) {
        self.ignored.insert(ip.clone());
    }

    pub fn skip_peer_connection(&self, addr: &SocketAddr) -> bool {
        for (_, peer) in self.peers.iter() {
            if peer.equals(addr) && (!peer.is_public() || peer.active() || peer.disabled()) {
                return true;
            }
        }
        false
    }

    pub fn send_pings(&mut self, registry: &Registry, height: u64, hash: Bytes) {
        let mut ping_sent = false;
        for (token, peer) in self.peers.iter_mut() {
            match peer.get_state() {
                State::Idle { from } => {
                    let random_time = random::<u64>() % PING_PERIOD;
                    if from.elapsed().as_secs() >= PING_PERIOD + random_time {
                        // Sometimes we check for new peers instead of pinging
                        let random: u8 = random();
                        let message = if random < 16 {
                            Message::GetPeers
                        } else {
                            Message::ping(height, hash.clone())
                        };

                        peer.set_state(State::message(message));
                        let stream = peer.get_stream();
                        registry.reregister(stream, token.clone(), Interest::WRITABLE).unwrap();
                        ping_sent = true;
                    }
                }
                _ => {}
            }
        }

        // If someone has more blocks we sync
        if !ping_sent {
            let mut rng = rand::thread_rng();
            match self.peers
                .iter_mut()
                .filter_map(|(token, peer)| if peer.has_more_blocks(height) { Some((token, peer)) } else { None })
                .choose(&mut rng) {
                None => {}
                Some((token, peer)) => {
                    debug!("Found some peer higher than we are, sending block request");
                    registry.reregister(peer.get_stream(), token.clone(), Interest::WRITABLE).unwrap();
                    peer.set_state(State::message(Message::GetBlock { index: height + 1 }));
                    ping_sent = true;
                }
            }
        }

        // If someone has less blocks (we mined a new block) we send a ping with our height
        if !ping_sent {
            let mut rng = rand::thread_rng();
            match self.peers
                .iter_mut()
                .filter_map(|(token, peer)| if peer.is_lower(height) && peer.get_state().is_idle() { Some((token, peer)) } else { None })
                .choose(&mut rng) {
                None => {}
                Some((token, peer)) => {
                    debug!("Found some peer lower than we are, sending ping");
                    registry.reregister(peer.get_stream(), token.clone(), Interest::WRITABLE).unwrap();
                    peer.set_state(State::message(Message::Ping { height, hash }));
                }
            }
        }

        let mut offline_ips = Vec::new();
        // Remove all peers that are offline for a long time
        self.peers.retain(|_, p| {
            let offline = p.get_state().need_reconnect() && p.reconnects() >= MAX_RECONNECTS;
            if offline {
                offline_ips.push(p.get_addr().ip());
            }
            !offline
        });
        for ip in offline_ips {
            self.ignore_ip(&ip);
        }

        for (token, peer) in self.peers.iter_mut() {
            if peer.get_state().need_reconnect() {
                let addr = peer.get_addr();
                if let Ok(mut stream) = TcpStream::connect(addr.clone()) {
                    debug!("Trying to connect to peer {}", &addr);
                    registry.register(&mut stream, token.clone(), Interest::WRITABLE).unwrap();
                    peer.set_state(State::Connecting);
                    peer.inc_reconnects();
                    peer.set_stream(stream);
                }
                // We make reconnects only to one at a time
                break;
            }
        }
    }

    pub fn connect_new_peers(&mut self, registry: &Registry, unique_token: &mut Token, yggdrasil_only: bool) {
        if self.new_peers.is_empty() {
            return;
        }
        for addr in &self.new_peers.clone() {
            self.connect_peer(&addr, registry, unique_token, yggdrasil_only);
        }
        self.new_peers.clear();
    }

    /// Connecting to configured (bootstrap) peers
    pub fn connect_peers(&mut self, peers_addrs: Vec<String>, registry: &Registry, unique_token: &mut Token, yggdrasil_only: bool) {
        for peer in peers_addrs.iter() {
            let addresses: Vec<SocketAddr> = match peer.to_socket_addrs() {
                Ok(peers) => { peers.collect() }
                Err(_) => { error!("Can't resolve address {}", &peer); continue; }
            };

            for addr in addresses {
                self.connect_peer(&addr, registry, unique_token, yggdrasil_only);
            }
        }
    }

    fn connect_peer(&mut self, addr: &SocketAddr, registry: &Registry, unique_token: &mut Token, yggdrasil_only: bool) {
        if self.ignored.contains(&addr.ip()) {
            return;
        }
        if yggdrasil_only && !is_yggdrasil(&addr.ip()) {
            debug!("Ignoring not Yggdrasil address '{}'", &addr.ip());
            return;
        }
        if let Ok(mut stream) = TcpStream::connect(addr.clone()) {
            let token = next(unique_token);
            debug!("Created connection {}, to peer {}", &token.0, &addr);
            registry.register(&mut stream, token, Interest::WRITABLE).unwrap();
            let mut peer = Peer::new(addr.clone(), stream, State::Connecting, false);
            peer.set_public(true);
            self.peers.insert(token, peer);
        }
    }
}

fn skip_private_addr(addr: &SocketAddr) -> bool {
    if addr.ip().is_loopback() {
        return true;
    }
    match addr {
        SocketAddr::V4(addr) => {
            if addr.ip().is_private() {
                return true;
            }
        }
        SocketAddr::V6(_addr) => {
            // TODO uncomment when stabilized
            // if addr.ip().is_unique_local() {
            //     return true;
            // }
        }
    }

    false
}