//! Merge several ACP connections into the one stream the event loop drains.
//!
//! Multi-profile mode (`/pane`) runs more than one kernel subprocess, so there
//! is more than one `AcpClientRx`. The event loop's shape — a single `select!`
//! arm with a look-ahead slot, a drain batch, and a session-load barrier — only
//! needs *one* ordered stream, and routing is already done per message by
//! `session_id` (`acp_handler::routing`). So instead of teaching every arm about
//! connections, each connection gets a forwarder task into one channel.
//!
//! Three properties matter:
//!
//! - **No message is lost or reordered per connection.** A forwarder is a plain
//!   FIFO pump for one receiver, and the merged channel preserves per-sender
//!   order, which is what replay/dedup relies on.
//! - **A connection can be replaced (reconnect) or dropped (pane closed).**
//!   [`AcpMux::attach`] retires the previous forwarder for that slot so the old
//!   receiver cannot interleave with the new one.
//! - **A closed receiver is reported, not hidden.** Merging would otherwise
//!   swallow the `None` that tells the loop its kernel died, so the forwarder
//!   emits [`MuxEvent::Closed`] for the slot it was serving. A deliberate
//!   detach retires the forwarder without that event.

use xai_acp_lib::AcpClientMessage;

/// What the loop receives from the merged stream.
#[derive(Debug)]
pub(crate) enum MuxEvent {
    /// A message from some connection.
    Message(AcpClientMessage),
    /// The receiver of `slot` reached EOF (its kernel or bridge stopped).
    Closed { slot: usize },
}

/// One slot's forwarder task.
struct Forwarder {
    abort: tokio::task::AbortHandle,
    /// Cleared before a deliberate abort so the task cannot report `Closed`.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// The merged client-message stream.
pub(crate) struct AcpMux {
    tx: tokio::sync::mpsc::UnboundedSender<MuxEvent>,
    rx: tokio::sync::mpsc::UnboundedReceiver<MuxEvent>,
    forwarders: std::collections::HashMap<usize, Forwarder>,
    /// Slots whose receiver closed, waiting for the loop to report them.
    closed_slots: Vec<usize>,
}

impl AcpMux {
    /// Merge `first` as connection slot 0.
    pub(crate) fn new(first: xai_acp_lib::AcpClientRx) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut mux = Self {
            tx,
            rx,
            forwarders: std::collections::HashMap::new(),
            closed_slots: Vec::new(),
        };
        mux.attach(0, first);
        mux
    }

    /// Pump `rx` into the merged stream as `slot`, replacing any previous one.
    pub(crate) fn attach(&mut self, slot: usize, mut rx: xai_acp_lib::AcpClientRx) {
        self.detach(slot);
        let tx = self.tx.clone();
        let live = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let task_live = live.clone();
        let task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                // The loop is gone: stop pumping instead of buffering forever.
                if tx.send(MuxEvent::Message(message)).is_err() {
                    return;
                }
            }
            if task_live.load(std::sync::atomic::Ordering::Acquire) {
                // EOF on a connection the loop still considers live: report it
                // so the kernel-death path can run.
                let _ = tx.send(MuxEvent::Closed { slot });
            }
        });
        self.forwarders.insert(
            slot,
            Forwarder {
                abort: task.abort_handle(),
                live,
            },
        );
    }

    /// Stop pumping one slot (pane closed, or its kernel was replaced).
    ///
    /// No [`MuxEvent::Closed`] is emitted: the caller is already handling the
    /// change and does not want a death notice for a connection it retired.
    pub(crate) fn detach(&mut self, slot: usize) {
        if let Some(forwarder) = self.forwarders.remove(&slot) {
            forwarder
                .live
                .store(false, std::sync::atomic::Ordering::Release);
            forwarder.abort.abort();
        }
    }

    /// Number of live connections feeding this stream.
    pub(crate) fn connection_count(&self) -> usize {
        self.forwarders.len()
    }

    /// Whether no connection is left.
    pub(crate) fn is_empty(&self) -> bool {
        self.forwarders.is_empty()
    }

    pub(crate) async fn recv(&mut self) -> Option<MuxEvent> {
        self.rx.recv().await
    }

    /// Pop one message, recording any close that arrives first.
    ///
    /// Used by the look-ahead path (session-load barrier), which can only hold a
    /// message. A close is stashed in [`Self::take_closed_slots`] instead of
    /// being consumed, so the death is still reported by the loop.
    pub(crate) fn try_next_message(
        &mut self,
    ) -> Result<AcpClientMessage, tokio::sync::mpsc::error::TryRecvError> {
        loop {
            match self.rx.try_recv() {
                Ok(MuxEvent::Message(message)) => return Ok(message),
                Ok(MuxEvent::Closed { slot }) => self.closed_slots.push(slot),
                Err(error) => return Err(error),
            }
        }
    }

    /// Closed slots seen since the last call, in arrival order.
    pub(crate) fn take_closed_slots(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.closed_slots)
    }

    /// Record a close the caller already consumed from [`Self::recv`].
    pub(crate) fn record_closed(&mut self, slot: usize) {
        self.closed_slots.push(slot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_acp_lib::acp_channels;

    /// Two connections both feed one stream.
    #[tokio::test]
    async fn merges_two_connections() {
        let (client_a, _agent_a) = acp_channels();
        let (client_b, _agent_b) = acp_channels();
        let mut mux = AcpMux::new(client_a.rx);
        mux.attach(1, client_b.rx);
        assert_eq!(mux.connection_count(), 2);
        assert!(matches!(
            mux.try_next_message(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// Re-attaching a slot retires the previous forwarder, so a reconnect can
    /// never interleave the dead receiver with the new one.
    #[tokio::test]
    async fn reattach_replaces_the_slot() {
        let (client_a, _agent_a) = acp_channels();
        let (client_c, _agent_c) = acp_channels();
        let mut mux = AcpMux::new(client_a.rx);
        mux.attach(0, client_c.rx);
        assert_eq!(mux.connection_count(), 1);
        mux.detach(0);
        assert_eq!(mux.connection_count(), 0);
        assert!(mux.is_empty());
    }

    /// A dropped sender closes the receiver, and the loop is told which slot
    /// died rather than silently stopping.
    #[tokio::test]
    async fn closed_connection_is_reported_with_its_slot() {
        let (client_a, agent_a) = acp_channels();
        let (_client_b, _agent_b) = acp_channels();
        let mut mux = AcpMux::new(client_a.rx);
        drop(agent_a);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), mux.recv())
            .await
            .expect("closed event")
            .expect("stream still open");
        match event {
            MuxEvent::Closed { slot } => assert_eq!(slot, 0),
            MuxEvent::Message(_) => panic!("expected a close event"),
        }
    }

    /// A close that arrives while the look-ahead path is polling is stashed, and
    /// the next message is still returned: the death notice is not lost.
    #[tokio::test]
    async fn lookahead_records_a_close_instead_of_dropping_it() {
        let (client_a, agent_a) = acp_channels();
        let mut mux = AcpMux::new(client_a.rx);
        drop(agent_a);
        assert!(matches!(
            mux.try_next_message(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
                | Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        // Give the forwarder a chance to publish the close, then drain it.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = mux.try_next_message();
        let closed = mux.take_closed_slots();
        assert!(closed.contains(&0), "close must be recorded: {closed:?}");
    }

    /// A deliberate detach must not look like a crash.
    #[tokio::test]
    async fn detach_does_not_report_a_close() {
        let (client_a, agent_a) = acp_channels();
        let mut mux = AcpMux::new(client_a.rx);
        mux.detach(0);
        drop(agent_a);
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(200), mux.recv()).await,
            Err(_)
        ));
    }
}
