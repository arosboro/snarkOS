// Copyright (C) 2019-2020 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    external::{
        message_types::{GetMemoryPool, GetPeers, Version},
        protocol::sync::SyncState,
    },
    Server,
};

use chrono::{Duration as ChronoDuration, Utc};
use std::{net::SocketAddr, time::Duration};
use tokio::{task, time::delay_for};

impl Server {
    /// Manages the number of active connections according to the connection frequency.
    /// 1. Get more connected peers if we are under the minimum number specified by the network context.
    ///     1.1 Ask our connected peers for their peers.
    ///     1.2 Ask our gossiped peers to handshake and become connected.
    /// 2. Maintain connected peers by sending ping messages.
    /// 3. Purge peers that have not responded in connection_frequency x 5 seconds.
    /// 4. Reselect a sync node if we purged it.
    /// 5. Update our memory pool every connection_frequency x memory_pool_interval seconds.
    /// All errors encountered by the connection handler will be logged to the console but will not stop the thread.
    pub async fn connection_handler(&self) {
        let context = self.context.clone();
        let memory_pool_lock = self.memory_pool_lock.clone();
        let sync_handler_lock = self.sync_handler_lock.clone();
        let storage = self.storage.clone();
        let connection_frequency = self.connection_frequency;

        // Start a separate thread for the handler.
        task::spawn(async move {
            let mut interval_ticker: u8 = 0;

            loop {
                {
                    // There are a lot of potentially expensive and blocking operations here.
                    // Let's wait for connection_frequency seconds before starting each loop.
                    delay_for(Duration::from_millis(connection_frequency)).await;

                    let peer_book = &mut context.peer_book.write().await;
                    let connections = context.connections.read().await;
                    let pings = &mut context.pings.write().await;

                    // Remove the local_address from the peer book
                    // if the node somehow discovered itself as a peer.
                    let local_address = *context.local_address.read().await;
                    peer_book.forget_peer(local_address);

                    // We have less peers than our minimum peer requirement. Look for more peers.
                    if peer_book.connected_total() < context.min_peers {
                        // Ask our connected peers.
                        for (address, _last_seen) in peer_book.get_connected() {
                            match connections.get(&address) {
                                Some(channel) => {
                                    // Disconnect from the peer if the get peers message was not sent properly
                                    if let Err(_) = channel.write(&GetPeers).await {
                                        peer_book.disconnect_peer(address);
                                    }
                                }
                                // Disconnect from the peer if there is no active connection channel
                                None => {
                                    peer_book.disconnect_peer(address);
                                }
                            }
                        }

                        // Create a list of gossiped peers and bootnodes to connect to.
                        let mut gossiped_peers = vec![];

                        // Add gossiped peers.
                        for (remote_address, _last_seen) in peer_book.get_gossiped() {
                            if remote_address != local_address && !peer_book.connected_contains(&remote_address) {
                                gossiped_peers.push(remote_address);
                            }
                        }

                        // Add unconnected bootnodes to the list of gossiped peers.
                        for bootnode in context.bootnodes.iter() {
                            if let Ok(bootnode_address) = bootnode.parse::<SocketAddr>() {
                                if bootnode_address != local_address && !peer_book.connected_contains(&bootnode_address)
                                {
                                    gossiped_peers.push(bootnode_address);
                                }
                            }
                        }

                        // Try and connect to gossiped peers and unconnected bootnodes.
                        for remote_address in gossiped_peers {
                            // Create a non-blocking handshake request
                            {
                                let new_context = context.clone();
                                let latest_block_height = storage.get_latest_block_height();

                                task::spawn(async move {
                                    // TODO (raychu86) Establish a formal node version
                                    let version =
                                        Version::new(1u64, latest_block_height, remote_address, local_address);

                                    let mut handshakes = new_context.handshakes.write().await; // Acquire the handshake lock
                                    if let Err(_) = handshakes.send_request(&version).await {
                                        debug!("Could not connect to gossiped peer {}", remote_address);
                                    }
                                });

                                peer_book.remove_gossiped(remote_address);
                            }
                        }
                    }

                    // Send a ping protocol request to each of our connected peers to maintain the connection.
                    for (address, last_seen) in peer_book.get_connected() {
                        let time_since_last_seen = (Utc::now() - last_seen).num_milliseconds();
                        if address != local_address
                            && time_since_last_seen.is_positive()
                            && time_since_last_seen as u64 > (connection_frequency * 3)
                        {
                            match connections.get(&address) {
                                Some(channel) => {
                                    // Disconnect from the peer if the ping message was not sent properly
                                    if let Err(_) = pings.send_ping(channel).await {
                                        warn!("Ping message failed to send to {}", address);
                                        peer_book.disconnect_peer(address);
                                    }
                                }
                                // Disconnect from the peer if there is no active connection channel
                                None => {
                                    peer_book.disconnect_peer(address);
                                }
                            }
                        }
                    }

                    // Purge peers that haven't responded in five frequency loops.
                    let response_timeout = ChronoDuration::milliseconds((connection_frequency * 5) as i64);

                    for (address, last_seen) in peer_book.get_connected() {
                        if Utc::now() - last_seen.clone() > response_timeout {
                            peer_book.disconnect_peer(address);
                        }
                    }

                    // If we have disconnected from our sync node,
                    // then set our sync state to idle and find a new sync node.
                    {
                        if let Ok(mut sync_handler) = sync_handler_lock.try_lock() {
                            if peer_book.disconnected_contains(&sync_handler.sync_node) {
                                if let Some(peer) = peer_book.get_connected().iter().max_by(|a, b| a.1.cmp(&b.1)) {
                                    sync_handler.sync_state = SyncState::Idle;
                                    sync_handler.sync_node = peer.0.clone();
                                };
                            }
                        }
                    }

                    // Store connected peers in database.
                    peer_book
                        .store(&storage)
                        .unwrap_or_else(|error| debug!("Failed to store connected peers in database {}", error));

                    // Every two frequency loops, send a version message to all peers for periodic syncs.
                    if interval_ticker % 2 == 1 {
                        if peer_book.connected_total() > 0 {
                            debug!("Sending out periodic version message to peers");
                        }

                        for (remote_address, _last_seen) in peer_book.get_connected() {
                            match connections.get(&remote_address) {
                                Some(channel) => {
                                    // Send a version message to peers.
                                    // If they are behind, they will attempt to sync.
                                    if let Some(handshake) = context.handshakes.read().await.get(&remote_address) {
                                        let version = Version::from(
                                            1u64, // TODO (raychu86) Establish a formal node version
                                            storage.get_latest_block_height(),
                                            remote_address,
                                            local_address,
                                            handshake.nonce,
                                        );
                                        if let Err(_) = channel.write(&version).await {
                                            peer_book.disconnect_peer(remote_address);
                                        }
                                    }
                                }
                                // Disconnect from the peer if there is no active connection channel
                                None => {
                                    peer_book.disconnect_peer(remote_address);
                                }
                            }
                        }
                    }

                    // Update our memory pool after memory_pool_interval frequency loops.
                    if interval_ticker >= context.memory_pool_interval {
                        // Ask our connected peers for their memory pool transactions
                        {
                            for (address, _last_seen) in peer_book.get_connected() {
                                if let Some(channel) = connections.get(&address) {
                                    // Disconnect from the peer if the GetMemoryPool message was not sent properly
                                    if let Err(_) = channel.write(&GetMemoryPool).await {
                                        peer_book.disconnect_peer(address);
                                    }
                                }
                            }
                        }

                        // Update our memory pool

                        let mut memory_pool = match memory_pool_lock.try_lock() {
                            Ok(memory_pool) => memory_pool,
                            _ => continue,
                        };

                        memory_pool.cleanse(&storage).unwrap_or_else(|error| {
                            debug!("Failed to cleanse memory pool transactions in database {}", error)
                        });

                        memory_pool.store(&storage).unwrap_or_else(|error| {
                            debug!("Failed to store memory pool transaction in database {}", error)
                        });

                        interval_ticker = 0;
                    } else {
                        interval_ticker += 1;
                    }
                }
            }
        });
    }
}
