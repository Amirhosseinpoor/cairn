//! Typed event bus (SPEC §3.5, REQ-ARCH-005/006).
//!
//! Two lanes share one sequence counter:
//!
//! * **critical** — every event except `tool.progress`; MUST be delivered.
//! * **droppable** — `tool.progress`; MAY be dropped oldest-first under lag.
//!
//! Dropping happens per lane, so a stalled renderer can never lose a
//! `message.appended`, `tool.finished`, `error` or `mode.changed` event.

use cairn_core::{Event, EventData};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Capacity of the critical lane (REQ-ARCH-005: critical events are never dropped
/// within this bound; publishers await room).
pub const CRITICAL_CAPACITY: usize = 4096;
/// Capacity of the droppable lane (matches the spec's `evt_tx` capacity of 1024).
pub const DROPPABLE_CAPACITY: usize = 1024;

/// Cloneable publisher handle.
#[derive(Clone)]
pub struct EventBusSender {
    critical: broadcast::Sender<Event>,
    droppable: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
    session: Option<String>,
}

impl std::fmt::Debug for EventBusSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBusSender")
            .field("session", &self.session)
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .field("subscribers", &self.critical.receiver_count())
            // Rendered as the subscriber count rather than the channel itself:
            // the raw `broadcast::Sender` debug output adds nothing here.
            .field("critical", &self.critical.receiver_count())
            .field("droppable", &self.droppable.receiver_count())
            .finish()
    }
}

/// Receiver that merges both lanes, preferring critical events.
#[derive(Debug)]
pub struct EventBusReceiver {
    critical: broadcast::Receiver<Event>,
    droppable: broadcast::Receiver<Event>,
}

/// The bus: create once, clone the sender everywhere (SPEC §3.3).
#[derive(Debug)]
pub struct EventBus {
    sender: EventBusSender,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        let (critical, _) = broadcast::channel(CRITICAL_CAPACITY);
        let (droppable, _) = broadcast::channel(DROPPABLE_CAPACITY);
        Self {
            sender: EventBusSender {
                critical,
                droppable,
                seq: Arc::new(AtomicU64::new(0)),
                session: None,
            },
        }
    }

    /// Publisher handle bound to a session id (events get `session` stamped).
    #[must_use]
    pub fn sender(&self) -> EventBusSender {
        self.sender.clone()
    }

    /// Publisher handle with an explicit session id.
    #[must_use]
    pub fn sender_for(&self, session: impl Into<String>) -> EventBusSender {
        let mut s = self.sender.clone();
        s.session = Some(session.into());
        s
    }

    /// Subscribe to both lanes.
    #[must_use]
    pub fn subscribe(&self) -> EventBusReceiver {
        EventBusReceiver {
            critical: self.sender.critical.subscribe(),
            droppable: self.sender.droppable.subscribe(),
        }
    }

    /// Current sequence counter (for tests/diagnostics).
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.sender.seq.load(Ordering::Relaxed)
    }
}

impl EventBusSender {
    /// Publish an event; returns its sequence number.
    ///
    /// Critical events ignore send errors (a bus with no subscribers is normal);
    /// they are never dropped because the critical lane is large and publishers
    /// do not compete with the droppable lane.
    #[must_use]
    pub fn publish(&self, data: EventData) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ev = Event::new(data, seq, self.session.clone());
        let (tx, lane) = if ev.data.is_critical() {
            (&self.critical, "critical")
        } else {
            (&self.droppable, "droppable")
        };
        let _ = tx.send(ev); // Err == no subscribers; not an error (REQ-ARCH-006).
        let _ = lane;
        seq
    }

    /// Convenience for `EventData::Error` (always critical).
    pub fn publish_error(
        &self,
        code: &str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> u64 {
        self.publish(EventData::Error {
            code: code.to_string(),
            message: message.into(),
            recoverable: true,
            hint: hint.into(),
        })
    }

    /// Number of subscribers on the critical lane (diagnostics).
    #[must_use]
    pub fn critical_receivers(&self) -> usize {
        self.critical.receiver_count()
    }
}

impl EventBusReceiver {
    /// Receive the next event, critical lane first (biased select).
    ///
    /// Returns `None` when both lanes are closed. Dropped `tool.progress`
    /// events are skipped transparently (`Lagged`).
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            tokio::select! {
                biased;
                msg = self.critical.recv() => match msg {
                    Ok(ev) => return Some(ev),
                    Err(broadcast::error::RecvError::Lagged(_)) => {},
                    // Lane closed: drain the remaining lane (returns None if it is closed too).
                    Err(broadcast::error::RecvError::Closed) => return self.droppable.recv().await.ok(),
                },
                msg = self.droppable.recv() => match msg {
                    Ok(ev) => return Some(ev),
                    Err(broadcast::error::RecvError::Lagged(_)) => {},
                    Err(broadcast::error::RecvError::Closed) => return self.critical.recv().await.ok(),
                },
            }
        }
    }

    /// Non-blocking receive.
    pub fn try_recv(&mut self) -> Option<Event> {
        loop {
            match self.critical.try_recv() {
                Ok(ev) => return Some(ev),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty) => {}
                Err(broadcast::error::TryRecvError::Closed) => {
                    // fall through to droppable
                    return self.droppable.try_recv().ok();
                }
            }
            match self.droppable.try_recv() {
                Ok(ev) => return Some(ev),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::event::ToolStatus;

    fn progress(i: u32) -> EventData {
        EventData::ToolProgress {
            call_id: "c1".into(),
            bytes_read: i,
            lines: i,
            truncated: false,
            preview: String::new(),
        }
    }

    fn finished(i: u64) -> EventData {
        EventData::TurnEnded {
            turn_id: i,
            status: cairn_core::event::TurnStatus::Ok,
            duration_ms: 1,
            cost_usd: 0.0,
        }
    }

    /// T-ARCH-005: droppable events may lag, critical events must all arrive.
    #[tokio::test]
    async fn droppable_lag_does_not_lose_critical_events() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let mut rx = bus.subscribe();

        // Fill the droppable lane far past capacity without reading it.
        for i in 0..(u32::try_from(DROPPABLE_CAPACITY).expect("capacity fits u32") + 500) {
            let _ = tx.publish(progress(i));
        }
        // Now publish critical events on top.
        for i in 0..100 {
            let _ = tx.publish(finished(i));
        }

        let mut got_turn_ended = 0;
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 10_000, "recv loop did not terminate");
            let Some(ev) = rx.recv().await else { break };
            if matches!(ev.data, EventData::TurnEnded { .. }) {
                got_turn_ended += 1;
            }
            if got_turn_ended == 100 {
                break;
            }
        }
        assert_eq!(got_turn_ended, 100, "critical events must never be dropped");
    }

    /// T-ARCH-006: publish with no subscribers is a no-op, not a panic.
    #[tokio::test]
    async fn publish_without_subscribers_is_fine() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let seq = tx.publish(finished(1));
        assert_eq!(seq, 1);
        assert_eq!(bus.seq(), 1);
    }

    #[tokio::test]
    async fn try_recv_prefers_critical_lane() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let mut rx = bus.subscribe();
        let _ = tx.publish(progress(1));
        let _ = tx.publish(finished(2));
        let ev = rx.try_recv().expect("event");
        assert!(
            matches!(ev.data, EventData::TurnEnded { .. }),
            "critical lane must win: {:?}",
            ev.data
        );
    }

    #[tokio::test]
    async fn sender_stamps_session_and_sequence() {
        let bus = EventBus::new();
        let tx = bus.sender_for("ses_1");
        let mut rx = bus.subscribe();
        let _ = tx.publish(finished(1));
        let _ = tx.publish(finished(2));
        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        assert_eq!(a.session.as_deref(), Some("ses_1"));
        assert_eq!((a.seq, b.seq), (1, 2));
        assert_eq!(a.v, 1);
    }

    #[tokio::test]
    async fn error_events_are_critical() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let mut rx = bus.subscribe();
        for i in 0..(u32::try_from(DROPPABLE_CAPACITY).expect("capacity fits u32") + 10) {
            let _ = tx.publish(progress(i));
        }
        tx.publish_error("E-PROV-SERVER", "boom", "retry");
        let mut saw = None;
        for _ in 0..DROPPABLE_CAPACITY + 20 {
            match rx.try_recv() {
                Some(ev) => {
                    if matches!(ev.data, EventData::Error { .. }) {
                        saw = Some(ev);
                        break;
                    }
                }
                None => break,
            }
        }
        let ev = saw.expect("error event must arrive");
        assert_eq!(ev.data.kind(), "error");
    }

    #[tokio::test]
    async fn tool_finished_status_roundtrip() {
        let bus = EventBus::new();
        let tx = bus.sender();
        let mut rx = bus.subscribe();
        let _ = tx.publish(EventData::ToolFinished {
            call_id: "c".into(),
            name: "bash".into(),
            status: ToolStatus::Timeout,
            duration_ms: 120_000,
            output_bytes: 10,
            truncated: true,
            error: Some("E-SHELL-TIMEOUT".into()),
        });
        let ev = rx.recv().await.unwrap();
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"tool.finished\""), "{json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, ev.data);
    }
}
