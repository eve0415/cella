//! Reverse-tunnel broker: matches pending tunnel requests with incoming agent
//! tunnel connections.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};

/// Manages pending reverse-tunnel requests and matches them with incoming
/// tunnel connections from agents.
pub struct TunnelBroker {
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<TcpStream>>>>,
}

impl TunnelBroker {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Allocate a connection ID and register a waiter.
    pub async fn request_tunnel(&self) -> (u64, oneshot::Receiver<TcpStream>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        (id, rx)
    }

    /// Deliver a reverse tunnel connection from an agent.
    ///
    /// Returns `false` if no pending request matches (timed out or spurious).
    pub async fn deliver(&self, connection_id: u64, stream: TcpStream) -> bool {
        let tx = self.pending.lock().await.remove(&connection_id);
        if let Some(tx) = tx {
            tx.send(stream).is_ok()
        } else {
            false
        }
    }

    /// Remove a timed-out pending request.
    pub async fn cancel(&self, connection_id: u64) {
        self.pending.lock().await.remove(&connection_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_returns_incrementing_ids() {
        let broker = TunnelBroker::new();
        let (id1, _rx1) = broker.request_tunnel().await;
        let (id2, _rx2) = broker.request_tunnel().await;
        assert_eq!(id1 + 1, id2);
    }

    #[tokio::test]
    async fn deliver_to_pending_request() {
        let broker = TunnelBroker::new();
        let (id, rx) = broker.request_tunnel().await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        let _ = connect_handle.await;

        assert!(broker.deliver(id, accepted).await);
        assert!(rx.await.is_ok());
    }

    #[tokio::test]
    async fn deliver_to_unknown_id_returns_false() {
        let broker = TunnelBroker::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        let _ = connect_handle.await;

        assert!(!broker.deliver(999, accepted).await);
    }

    #[tokio::test]
    async fn cancel_removes_pending_request() {
        let broker = TunnelBroker::new();
        let (id, rx) = broker.request_tunnel().await;
        broker.cancel(id).await;

        // Receiver should error since sender was dropped
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn cancel_unknown_id_is_harmless() {
        let broker = TunnelBroker::new();
        broker.cancel(12345).await;
    }

    #[tokio::test]
    async fn deliver_after_receiver_dropped_returns_false() {
        let broker = TunnelBroker::new();
        let (id, rx) = broker.request_tunnel().await;
        drop(rx);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        let _ = connect_handle.await;

        assert!(!broker.deliver(id, accepted).await);
    }
}
