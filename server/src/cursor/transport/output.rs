//! Buffers, replays, broadcasts, and atomically closes downstream output.

use bytes::Bytes;
use tokio::sync::mpsc;

#[derive(Default)]
pub struct OutputHub {
    state: parking_lot::Mutex<OutputState>,
}

#[derive(Default)]
struct OutputState {
    history: Vec<Bytes>,
    subscribers: Vec<mpsc::UnboundedSender<Bytes>>,
    closed: bool,
}

impl OutputHub {
    pub fn emit(&self, frame: Bytes) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.history.push(frame.clone());
        state
            .subscribers
            .retain(|subscriber| subscriber.send(frame.clone()).is_ok());
        true
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Bytes> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = self.state.lock();
        for frame in &state.history {
            let _ = sender.send(frame.clone());
        }
        if !state.closed {
            state.subscribers.push(sender);
        }
        receiver
    }

    /// Prunes dropped receivers before reporting. Used by the disconnect grace
    /// path so a stale stream's late drop cannot cancel a Run that already has
    /// a reconnected subscriber.
    pub fn has_live_subscribers(&self) -> bool {
        let mut state = self.state.lock();
        state
            .subscribers
            .retain(|subscriber| !subscriber.is_closed());
        !state.subscribers.is_empty()
    }

    pub fn close(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.subscribers.clear();
        drop(state);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn late_subscriber_replays_history_then_receives_live_frames() {
        let hub = OutputHub::default();
        assert!(hub.emit(Bytes::from_static(b"one")));
        assert!(hub.emit(Bytes::from_static(b"two")));

        let mut receiver = hub.subscribe();
        assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"one")));
        assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"two")));

        assert!(hub.emit(Bytes::from_static(b"three")));
        assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"three")));
    }

    #[tokio::test]
    async fn closed_hub_replays_history_but_rejects_new_output() {
        let hub = OutputHub::default();
        assert!(hub.emit(Bytes::from_static(b"done")));
        assert!(hub.close());

        let mut receiver = hub.subscribe();
        assert_eq!(receiver.recv().await, Some(Bytes::from_static(b"done")));
        assert!(!hub.emit(Bytes::from_static(b"late")));
    }
}
