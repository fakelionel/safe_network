// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::error::{Error, Result};
use bytes::Bytes;
use err_derive::Error;
use futures::{
    lock::Mutex,
    stream::{FuturesUnordered, StreamExt},
};
use lru_time_cache::LruCache;
use qp2p::{Connection, Endpoint, QuicP2p};
use std::{
    fmt::{self, Debug, Formatter},
    future::Future,
    net::SocketAddr,
    sync::Arc,
};
use tokio::{
    sync::{mpsc, watch},
    task,
};

// Number of Connections to maintain in the cache
const CONNECTIONS_CACHE_SIZE: usize = 1024;

// Communication component of the node to interact with other nodes.
pub(crate) struct Comm {
    _quic_p2p: QuicP2p,
    endpoint: Endpoint,
    node_conns: Mutex<LruCache<SocketAddr, Arc<Connection>>>,
}

impl Comm {
    pub fn new(transport_config: qp2p::Config) -> Result<Self> {
        let quic_p2p = QuicP2p::with_config(Some(transport_config), Default::default(), true)?;

        // Don't bootstrap, just create an endpoint where to listen to
        // the incoming messages from other nodes.
        let endpoint = quic_p2p.new_endpoint()?;
        let node_conns = Mutex::new(LruCache::with_capacity(CONNECTIONS_CACHE_SIZE));

        Ok(Self {
            _quic_p2p: quic_p2p,
            endpoint,
            node_conns,
        })
    }

    pub async fn from_bootstrapping(transport_config: qp2p::Config) -> Result<(Self, SocketAddr)> {
        let quic_p2p = QuicP2p::with_config(Some(transport_config), Default::default(), true)?;

        // Bootstrap to the network returning the connection to a node.
        let (endpoint, conn) = quic_p2p.bootstrap().await?;
        let addr = conn.remote_address();

        let mut node_conns = LruCache::with_capacity(CONNECTIONS_CACHE_SIZE);
        let _ = node_conns.insert(addr, Arc::new(conn));
        let node_conns = Mutex::new(node_conns);

        Ok((
            Self {
                _quic_p2p: quic_p2p,
                endpoint,
                node_conns,
            },
            addr,
        ))
    }

    /// Starts listening for incoming messages. Returns a stream to read the messages from.
    ///
    /// NOTE: this method can be called multiple times, producing multiple independent streams.
    /// Every message is received on only one stream, but it's unspecified which. For this reason
    /// it's recommended to always use only one stream to avoid potentially surprising behaviour.
    ///
    /// Also, if a stream is dropped and then another one created, it will cause a disconnection
    /// event for the peers whom we received any messages from on the first stream. This means the
    /// next message they send to us will fail the first send attempt (but will likely succeed on
    /// the subsequent one).
    pub fn listen(&self) -> Result<IncomingConnections> {
        Ok(IncomingConnections::new(self.endpoint.listen()?))
    }

    pub async fn our_connection_info(&self) -> Result<SocketAddr> {
        self.endpoint.socket_addr().await.map_err(|err| {
            error!("Failed to retrieve our connection info: {:?}", err);
            err.into()
        })
    }

    /// Sends a message to multiple recipients. Attempts to send to `delivery_group_size`
    /// recipients out of the `recipients` list. If a send fails, attempts to send to the next peer
    /// until `delivery_goup_size` successful sends complete or there are no more recipients to
    /// try.
    ///
    /// Returns `Ok` if all of `delivery_group_size` sends succeeded and `Err` if less that
    /// `delivery_group_size` succeeded. Also returns all the failed recipients which can be used
    /// by the caller to identify lost peers.
    pub async fn send_message_to_targets(
        &self,
        recipients: &[SocketAddr],
        delivery_group_size: usize,
        msg: Bytes,
    ) -> (Result<(), SendError>, Vec<SocketAddr>) {
        trace!(
            "Sending message ({} bytes) to {} of {:?}",
            msg.len(),
            delivery_group_size,
            recipients
        );

        if recipients.len() < delivery_group_size {
            warn!(
                "Less than delivery_group_size valid recipients - delivery_group_size: {}, recipients: {:?}",
                delivery_group_size,
                recipients,
            );
        }

        let delivery_group_size = delivery_group_size.min(recipients.len());

        // Run all the sends concurrently (using `FuturesUnordered`). If any of them fails, pick
        // the next recipient and try to send to them. Proceed until the needed number of sends
        // succeeds or if there are no more recipients to pick.
        let send = |recipient, msg| async move { (self.send_to(recipient, msg).await, recipient) };

        let mut tasks: FuturesUnordered<_> = recipients[0..delivery_group_size]
            .iter()
            .map(|recipient| send(recipient, msg.clone()))
            .collect();

        let mut next = delivery_group_size;
        let mut successes = 0;
        let mut failed_recipients = vec![];

        while let Some((result, addr)) = tasks.next().await {
            if result.is_ok() {
                successes += 1;
            } else {
                failed_recipients.push(*addr);

                if next < recipients.len() {
                    tasks.push(send(&recipients[next], msg.clone()));
                    next += 1;
                }
            }
        }

        trace!(
            "Sending message ({} bytes) finished to {}/{} recipients (failed: {:?})",
            msg.len(),
            successes,
            delivery_group_size,
            failed_recipients
        );

        let result = if successes == delivery_group_size {
            Ok(())
        } else {
            Err(SendError)
        };

        (result, failed_recipients)
    }

    // Low-level send
    async fn send_to(&self, recipient: &SocketAddr, msg: Bytes) -> Result<(), qp2p::Error> {
        // Cache the Connection to the node or obtain the already cached one
        // NOTE: not using the entry API to avoid holding the mutex longer than necessary.
        // NOTE: make sure the `let conn = self.node_conns.lock()...` expression is on its own line
        //       to avoid deadlocks.
        let conn = self.node_conns.lock().await.get(recipient).cloned();
        if let Some(conn) = conn {
            if conn.send_uni(msg.clone()).await.is_ok() {
                return Ok(());
            } else {
                let _ = self.node_conns.lock().await.remove(recipient);
            }
        }

        let conn = self.endpoint.connect_to(recipient).await?;
        conn.send_uni(msg).await?;

        let _ = self
            .node_conns
            .lock()
            .await
            .insert(*recipient, Arc::new(conn));

        Ok(())
    }
}

#[derive(Debug, Error)]
#[error(display = "Send failed")]
pub struct SendError;

impl From<SendError> for Error {
    fn from(_: SendError) -> Self {
        Error::FailedSend
    }
}

pub(crate) enum ConnectionEvent {
    Received(qp2p::Message),
    Disconnected(SocketAddr),
}

impl Debug for ConnectionEvent {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::Received(qp2p::Message::UniStream { src, .. }) => {
                write!(f, "Received(UniStream {{ src: {}, .. }})", src)
            }
            Self::Received(qp2p::Message::BiStream { src, .. }) => {
                write!(f, "Received(BiStream {{ src: {}, .. }})", src)
            }
            Self::Disconnected(addr) => write!(f, "Disconnected({})", addr),
        }
    }
}

/// Stream of incoming connection events.
pub(crate) struct IncomingConnections {
    event_rx: mpsc::Receiver<ConnectionEvent>,

    // TODO: use `mpsc::Sender::closed` instead of this when we switch to the version of tokio that
    // supports it (>= 0.3.0).
    cancel_tx: watch::Sender<bool>,
}

impl Drop for IncomingConnections {
    fn drop(&mut self) {
        let _ = self.cancel_tx.broadcast(true);
    }
}

impl IncomingConnections {
    pub fn new(incoming_conns: qp2p::IncomingConnections) -> Self {
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let (event_tx, event_rx) = mpsc::channel(1);

        // Need to `recv` once, otherwise we would cancel all the tasks immediatelly
        // NOTE: using block_on to avoid making this function `async`. It won't actually block,
        // because the receiver is immediatelly ready.
        // (for more details, see: https://docs.rs/tokio/0.2.22/tokio/sync/watch/struct.Receiver.html?search=#method.recv).
        let _ = futures::executor::block_on(cancel_rx.recv());

        let _ = task::spawn(cancellable(
            cancel_rx.clone(),
            handle_incoming_connections(incoming_conns, event_tx, cancel_rx),
        ));

        Self {
            event_rx,
            cancel_tx,
        }
    }

    pub async fn next(&mut self) -> Option<ConnectionEvent> {
        self.event_rx.recv().await
    }
}

async fn handle_incoming_connections(
    mut incoming_conns: qp2p::IncomingConnections,
    event_tx: mpsc::Sender<ConnectionEvent>,
    cancel_rx: watch::Receiver<bool>,
) {
    while let Some(incoming_msgs) = incoming_conns.next().await {
        trace!(
            "New connection established by peer {}",
            incoming_msgs.remote_addr()
        );

        let _ = task::spawn(cancellable(
            cancel_rx.clone(),
            handle_incoming_messages(incoming_msgs, event_tx.clone()),
        ));
    }
}

async fn handle_incoming_messages(
    mut incoming_msgs: qp2p::IncomingMessages,
    mut event_tx: mpsc::Sender<ConnectionEvent>,
) {
    while let Some(msg) = incoming_msgs.next().await {
        let _ = event_tx.send(ConnectionEvent::Received(msg)).await;
    }

    let _ = event_tx
        .send(ConnectionEvent::Disconnected(incoming_msgs.remote_addr()))
        .await;
}

async fn cancellable<F: Future>(
    mut cancel_rx: watch::Receiver<bool>,
    future: F,
) -> Result<F::Output, Cancelled> {
    tokio::select! {
        value = future => Ok(value),
        _ = cancel_rx.recv() => Err(Cancelled),
    }
}

struct Cancelled;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use assert_matches::assert_matches;
    use futures::future;
    use qp2p::Config;
    use std::{net::Ipv4Addr, slice, time::Duration};
    use tokio::{net::UdpSocket, sync::mpsc, time};

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn successful_send() -> Result<()> {
        let comm = Comm::new(transport_config())?;

        let mut peer0 = Peer::new().await?;
        let mut peer1 = Peer::new().await?;

        let message = Bytes::from_static(b"hello world");
        comm.send_message_to_targets(&[peer0.addr, peer1.addr], 2, message.clone())
            .await
            .0?;

        assert_eq!(peer0.rx.recv().await, Some(message.clone()));
        assert_eq!(peer1.rx.recv().await, Some(message));

        Ok(())
    }

    #[tokio::test]
    async fn successful_send_to_subset() -> Result<()> {
        let comm = Comm::new(transport_config())?;

        let mut peer0 = Peer::new().await?;
        let mut peer1 = Peer::new().await?;

        let message = Bytes::from_static(b"hello world");
        comm.send_message_to_targets(&[peer0.addr, peer1.addr], 1, message.clone())
            .await
            .0?;

        assert_eq!(peer0.rx.recv().await, Some(message));

        assert!(time::timeout(TIMEOUT, peer1.rx.recv())
            .await
            .unwrap_or_default()
            .is_none());

        Ok(())
    }

    #[tokio::test]
    async fn failed_send() -> Result<()> {
        let comm = Comm::new(Config {
            // This makes this test faster.
            idle_timeout_msec: Some(1),
            ..transport_config()
        })?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        let (result, failed_recipients) = comm
            .send_message_to_targets(&[invalid_addr], 1, message.clone())
            .await;
        assert!(result.is_err());
        assert_eq!(failed_recipients, [invalid_addr]);

        Ok(())
    }

    #[tokio::test]
    async fn successful_send_after_failed_attempts() -> Result<()> {
        let comm = Comm::new(Config {
            idle_timeout_msec: Some(1),
            ..transport_config()
        })?;
        let mut peer = Peer::new().await?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        comm.send_message_to_targets(&[invalid_addr, peer.addr], 1, message.clone())
            .await
            .0?;

        assert_eq!(peer.rx.recv().await, Some(message));

        Ok(())
    }

    #[tokio::test]
    async fn partially_successful_send() -> Result<()> {
        let comm = Comm::new(Config {
            idle_timeout_msec: Some(1),
            ..transport_config()
        })?;
        let mut peer = Peer::new().await?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        let (result, failed_recipients) = comm
            .send_message_to_targets(&[invalid_addr, peer.addr], 2, message.clone())
            .await;

        assert!(result.is_err());
        assert_eq!(failed_recipients, [invalid_addr]);
        assert_eq!(peer.rx.recv().await, Some(message));

        Ok(())
    }

    #[tokio::test]
    async fn send_after_reconnect() -> Result<()> {
        let send_comm = Comm::new(transport_config())?;

        let recv_transport = QuicP2p::with_config(Some(transport_config()), &[], false)?;
        let recv_endpoint = recv_transport.new_endpoint()?;
        let recv_addr = recv_endpoint.socket_addr().await?;
        let mut recv_incoming_connections = recv_endpoint.listen()?;

        // Send the first message.
        let msg0 = Bytes::from_static(b"zero");
        send_comm
            .send_message_to_targets(slice::from_ref(&recv_addr), 1, msg0.clone())
            .await
            .0?;

        let mut msg0_received = false;

        // Receive one message and drop the incoming stream.
        {
            if let Some(mut incoming_msgs) =
                time::timeout(TIMEOUT, recv_incoming_connections.next()).await?
            {
                if let Some(msg) = time::timeout(TIMEOUT, incoming_msgs.next()).await? {
                    assert_eq!(msg.get_message_data(), msg0);
                    msg0_received = true;
                }
            }

            assert!(msg0_received);
        }

        // Send the second message.
        let msg1 = Bytes::from_static(b"one");
        send_comm
            .send_message_to_targets(slice::from_ref(&recv_addr), 1, msg1.clone())
            .await
            .0?;

        let mut msg1_received = false;

        // Expect to receive the second message on a re-established connection.
        if let Some(mut incoming_msgs) =
            time::timeout(TIMEOUT, recv_incoming_connections.next()).await?
        {
            if let Some(msg) = time::timeout(TIMEOUT, incoming_msgs.next()).await? {
                assert_eq!(msg.get_message_data(), msg1);
                msg1_received = true;
            }
        }

        assert!(msg1_received);

        Ok(())
    }

    #[tokio::test]
    async fn incoming_connection_lost() -> Result<()> {
        let comm0 = Comm::new(transport_config())?;
        let addr0 = comm0.our_connection_info().await?;
        let mut incoming_conns0 = comm0.listen()?;

        let comm1 = Comm::new(transport_config())?;
        let addr1 = comm1.our_connection_info().await?;

        // Send a message to establish the connection
        comm1
            .send_message_to_targets(slice::from_ref(&addr0), 1, Bytes::from_static(b"hello"))
            .await
            .0?;
        assert_matches!(
            incoming_conns0.next().await,
            Some(ConnectionEvent::Received(_))
        );

        // Drop `comm1` to cause connection lost.
        drop(comm1);

        assert_matches!(
            time::timeout(TIMEOUT, incoming_conns0.next()).await?,
            Some(ConnectionEvent::Disconnected(addr)) => assert_eq!(addr, addr1)
        );

        Ok(())
    }

    fn transport_config() -> Config {
        Config {
            ip: Some(Ipv4Addr::LOCALHOST.into()),
            ..Default::default()
        }
    }

    struct Peer {
        addr: SocketAddr,
        rx: mpsc::Receiver<Bytes>,
    }

    impl Peer {
        async fn new() -> Result<Self> {
            let transport = QuicP2p::with_config(Some(transport_config()), &[], false)?;

            let endpoint = transport.new_endpoint()?;
            let addr = endpoint.socket_addr().await?;
            let mut incoming_connections = endpoint.listen()?;

            let (tx, rx) = mpsc::channel(1);

            let _ = tokio::spawn(async move {
                while let Some(mut connection) = incoming_connections.next().await {
                    let mut tx = tx.clone();
                    let _ = tokio::spawn(async move {
                        while let Some(message) = connection.next().await {
                            let _ = tx.send(message.get_message_data()).await;
                        }
                    });
                }
            });

            Ok(Self { addr, rx })
        }
    }

    async fn get_invalid_addr() -> Result<SocketAddr> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = socket.local_addr()?;

        // Keep the socket alive to keep the address bound, but don't read/write to it so any
        // attempt to connect to it will fail.
        let _ = tokio::spawn(async move {
            future::pending::<()>().await;
            let _ = socket;
        });

        Ok(addr)
    }
}
