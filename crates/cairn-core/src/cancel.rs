//! The cancellation tree of SPEC §3.3 (REQ-ARCH-007).
//!
//! ```text
//! root (process) ──┬── turn ──┬── tool call
//!                  │          ├── model stream
//!                  │          └── subagent turn ──┬── tool call
//!                  └── background job
//! ```
//!
//! Propagation is **eager**: cancelling a node walks its live descendants and
//! marks them, which is O(descendants) and finishes in microseconds — well
//! inside REQ-ARCH-007's 10 ms budget — and keeps `is_cancelled()` lock-free
//! for the polling loop (a tool executor polls at least every 100 ms).
//!
//! The type is std-only on purpose: `cairn-core` may import nothing but
//! `std`, `serde` and `thiserror` (§3.2), and this is the one concurrency
//! primitive every crate has to name.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// A node in the cancellation tree (SPEC §3.3, REQ-ARCH-007).
#[derive(Debug)]
pub struct CancellationToken {
    node: Arc<Node>,
}

#[derive(Debug)]
struct Node {
    cancelled: AtomicBool,
    /// Live children — and the monitor the [`Condvar`] waits on. The flag is
    /// only ever flipped while this lock is held, so a waiter can neither miss
    /// a notification nor deadlock against a concurrent `child()`.
    children: Mutex<Vec<Weak<Node>>>,
    changed: Condvar,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for CancellationToken {
    fn clone(&self) -> Self {
        Self {
            node: Arc::clone(&self.node),
        }
    }
}

impl CancellationToken {
    /// A root token, not yet cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node: Arc::new(Node::new(false)),
        }
    }

    /// Spawn a descendant. A child of an already-cancelled parent starts
    /// cancelled, so no work can slip in after the fact.
    #[must_use]
    pub fn child(&self) -> Self {
        let child = CancellationToken {
            node: Arc::new(Node::new(self.is_cancelled())),
        };
        let mut siblings = self.node.children.lock().expect("children lock");
        if self.is_cancelled() {
            // Raced with a concurrent `cancel()` — start cancelled.
            child.node.cancelled.store(true, Ordering::SeqCst);
        } else {
            // Reap entries whose child has been dropped.
            siblings.retain(|w| w.strong_count() > 0);
            siblings.push(Arc::downgrade(&child.node));
        }
        drop(siblings);
        child
    }

    /// Cancel this node and every descendant (REQ-ARCH-007). Idempotent.
    pub fn cancel(&self) {
        let children = {
            let mut guard = self.node.children.lock().expect("children lock");
            if self.node.cancelled.swap(true, Ordering::SeqCst) {
                return; // already cancelled: the walk happened then
            }
            let taken = std::mem::take(&mut *guard);
            self.node.changed.notify_all();
            taken
        };
        for child in children.into_iter().filter_map(|w| w.upgrade()) {
            cancel_node(&child);
        }
    }

    /// Lock-free check — safe to poll every 100 ms (REQ-ARCH-007) or tighter.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.node.cancelled.load(Ordering::SeqCst)
    }

    /// Block until cancelled or `timeout` elapses; returns whether the node is
    /// cancelled when it returns. Never waits past the deadline.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut guard = self.node.children.lock().expect("children lock");
        while !self.is_cancelled() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (g, _) = self
                .node
                .changed
                .wait_timeout(guard, deadline - now)
                .expect("condvar poisoned");
            guard = g;
        }
        self.is_cancelled()
    }
}

impl Node {
    fn new(cancelled: bool) -> Self {
        Self {
            cancelled: AtomicBool::new(cancelled),
            children: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        }
    }
}

/// The recursive half of [`CancellationToken::cancel`].
fn cancel_node(node: &Arc<Node>) {
    let children = {
        let mut guard = node.children.lock().expect("children lock");
        if node.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let taken = std::mem::take(&mut *guard);
        node.changed.notify_all();
        taken
    };
    for child in children.into_iter().filter_map(|w| w.upgrade()) {
        cancel_node(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-ARCH-007 — cancelling the root cancels every descendant (2 subagents,
    /// one grandchild, 1 background job) within 10 ms (REQ-ARCH-007).
    #[test]
    fn t_arch_007_parent_cancels_descendants_within_10ms() {
        let root = CancellationToken::new();
        let turn = root.child();
        let subagent_1 = turn.child();
        let grandchild = subagent_1.child();
        let subagent_2 = turn.child();
        let job = root.child();

        let started = Instant::now();
        root.cancel();
        let elapsed = started.elapsed();

        for (name, token) in [
            ("turn", &turn),
            ("subagent_1", &subagent_1),
            ("grandchild", &grandchild),
            ("subagent_2", &subagent_2),
            ("background job", &job),
        ] {
            assert!(token.is_cancelled(), "`{name}` was not cancelled");
        }
        assert!(
            elapsed <= Duration::from_millis(10),
            "propagation took {elapsed:?}, budget is 10 ms"
        );
        assert!(root.is_cancelled());
    }

    /// T-ARCH-007 — a tool executor polling at the cadence the spec mandates
    /// observes the cancellation within 100 ms (REQ-ARCH-007).
    #[test]
    fn t_arch_007_tool_executor_observes_within_100ms() {
        let root = CancellationToken::new();
        let tool = root.child();

        let observed = std::thread::spawn(move || {
            let started = Instant::now();
            while !tool.is_cancelled() {
                assert!(
                    started.elapsed() < Duration::from_millis(500),
                    "executor never saw the cancellation"
                );
                // REQ-ARCH-007: poll at least every 100 ms.
                std::thread::sleep(Duration::from_millis(25));
            }
            started.elapsed()
        });

        std::thread::sleep(Duration::from_millis(30));
        root.cancel();
        let elapsed = observed.join().expect("executor thread");
        assert!(
            elapsed <= Duration::from_millis(100),
            "executor observed cancellation after {elapsed:?}, budget is 100 ms"
        );
    }

    #[test]
    fn child_of_a_cancelled_parent_starts_cancelled() {
        let root = CancellationToken::new();
        root.cancel();
        let late = root.child();
        assert!(late.is_cancelled());
        assert!(late.child().is_cancelled());
    }

    #[test]
    fn cancelling_a_leaf_does_not_touch_its_parent() {
        let root = CancellationToken::new();
        let leaf = root.child();
        leaf.cancel();
        assert!(leaf.is_cancelled());
        assert!(!root.is_cancelled(), "propagation is one-directional");
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
        // A child created after the second cancel still starts cancelled.
        assert!(token.child().is_cancelled());
    }

    #[test]
    fn dropped_children_are_not_cancelled_twice() {
        let root = CancellationToken::new();
        let a = root.child();
        let b = root.child();
        drop(a);
        root.cancel();
        assert!(b.is_cancelled());
        assert!(root.is_cancelled());
    }

    #[test]
    fn wait_timeout_returns_early_on_cancel_and_false_on_timeout() {
        let token = CancellationToken::new();
        assert!(
            !token.wait_timeout(Duration::from_millis(5)),
            "nothing cancelled it"
        );

        let waiter = token.clone();
        let handle = std::thread::spawn(move || waiter.wait_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(5));
        token.cancel();
        assert!(handle.join().expect("waiter"), "woken by the cancellation");
    }

    #[test]
    fn a_waiter_that_starts_waiting_after_cancel_sees_it_immediately() {
        let token = CancellationToken::new();
        token.cancel();
        let started = Instant::now();
        assert!(token.wait_timeout(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_millis(10));
    }

    #[test]
    fn tokens_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CancellationToken>();
    }
}
