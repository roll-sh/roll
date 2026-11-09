//! Single active consumer coordination (RabbitMQ's `rabbit_stream_sac_coordinator`).
//!
//! Consumers sharing a name on a stream form a group with one active member. On a plain stream
//! the first consumer is active and the next takes over when it leaves. On a super stream
//! partition the active member is `consumers[partition index % size]`; when that changes the
//! current one is told to step down (`active = false`) and the new one is only activated once
//! the former confirms, so two consumers never read the partition at once.

use mq_core::queue::ConnId;
use parking_lot::Mutex;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// A SAC notification for a connection: send `ConsumerUpdate` to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub sub: u8,
    pub active: bool,
    pub stepping_down: bool,
    pub stream: String,
    pub name: String,
}

/// Group key: vhost, stream, consumer name.
pub type Key = (String, String, String);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum St {
    Active,
    Waiting,
    Deactivating,
}

struct Csr {
    conn: ConnId,
    sub: u8,
    tx: UnboundedSender<Notice>,
    st: St,
}

struct Group {
    partition: i64,
    csrs: Vec<Csr>,
}

impl Group {
    fn active(&self) -> Option<usize> {
        self.csrs.iter().position(|c| c.st == St::Active)
    }

    fn evaluate(&self) -> Option<usize> {
        match self.csrs.len() {
            0 => None,
            n if self.partition >= 0 => Some((self.partition as usize) % n),
            _ => Some(0),
        }
    }
}

/// The broker-wide coordinator.
#[derive(Default)]
pub struct Coordinator {
    groups: Mutex<HashMap<Key, Group>>,
}

fn notify(key: &Key, c: &Csr, active: bool, stepping_down: bool) {
    let _ = c.tx.send(Notice { sub: c.sub, active, stepping_down, stream: key.1.clone(), name: key.2.clone() });
}

impl Coordinator {
    /// Add a consumer; returns whether it is active right away. `partition` is the super
    /// stream partition index, or -1 for a plain stream.
    pub fn register(&self, key: Key, partition: i64, conn: ConnId, sub: u8, tx: UnboundedSender<Notice>) -> bool {
        let mut groups = self.groups.lock();
        let g = groups.entry(key.clone()).or_insert_with(|| Group { partition, csrs: Vec::new() });
        if g.partition < 0 {
            let st = if g.active().is_some() { St::Waiting } else { St::Active };
            g.csrs.push(Csr { conn, sub, tx, st });
            let c = &g.csrs[g.csrs.len() - 1];
            if st == St::Active {
                notify(&key, c, true, false);
            }
            return st == St::Active;
        }
        if g.csrs.is_empty() {
            g.csrs.push(Csr { conn, sub, tx, st: St::Active });
            notify(&key, &g.csrs[0], true, false);
            return true;
        }
        g.csrs.push(Csr { conn, sub, tx, st: St::Waiting });
        Self::rebalance(&key, g);
        g.csrs.last().is_some_and(|c| c.st == St::Active)
    }

    fn rebalance(key: &Key, g: &mut Group) {
        match g.active() {
            Some(cur) => {
                if g.evaluate() != Some(cur) {
                    g.csrs[cur].st = St::Deactivating;
                    notify(key, &g.csrs[cur], false, true);
                }
            }
            None => {
                if g.csrs.iter().any(|c| c.st == St::Deactivating) {
                    return;
                }
                if let Some(i) = g.evaluate() {
                    g.csrs[i].st = St::Active;
                    notify(key, &g.csrs[i], true, false);
                }
            }
        }
    }

    /// Remove a consumer (unsubscribe or connection closed).
    pub fn unregister(&self, key: &Key, conn: ConnId, sub: u8) {
        let mut groups = self.groups.lock();
        let Some(g) = groups.get_mut(key) else { return };
        let Some(i) = g.csrs.iter().position(|c| c.conn == conn && c.sub == sub) else { return };
        let removed = g.csrs.remove(i);
        if g.csrs.is_empty() {
            groups.remove(key);
            return;
        }
        let active_removed = removed.st == St::Active;
        if g.partition < 0 {
            if active_removed {
                for c in &mut g.csrs {
                    c.st = St::Waiting;
                }
                g.csrs[0].st = St::Active;
                notify(key, &g.csrs[0], true, false);
            }
            return;
        }
        match g.active() {
            Some(_) => Self::rebalance(key, g),
            None if active_removed => {
                if let Some(i) = g.evaluate() {
                    g.csrs[i].st = St::Active;
                    notify(key, &g.csrs[i], true, false);
                }
            }
            // Waiting for a former active consumer to step down.
            None => {}
        }
    }

    /// A consumer stepped down: activate the one that should be active now.
    pub fn activate(&self, key: &Key) {
        let mut groups = self.groups.lock();
        let Some(g) = groups.get_mut(key) else { return };
        let before = g.active();
        for c in &mut g.csrs {
            c.st = St::Waiting;
        }
        if let Some(i) = g.evaluate() {
            g.csrs[i].st = St::Active;
            if before != Some(i) {
                notify(key, &g.csrs[i], true, false);
            }
        }
    }

    /// Whether a consumer is (still) the active one of its group.
    pub fn is_active(&self, key: &Key, conn: ConnId, sub: u8) -> bool {
        self.groups.lock().get(key).is_some_and(|g| g.csrs.iter().any(|c| c.conn == conn && c.sub == sub && c.st == St::Active))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn key() -> Key {
        ("/".into(), "s".into(), "app".into())
    }

    fn drain(rx: &mut UnboundedReceiver<Notice>) -> Vec<(u8, bool, bool)> {
        let mut v = Vec::new();
        while let Ok(n) = rx.try_recv() {
            v.push((n.sub, n.active, n.stepping_down));
        }
        v
    }

    #[test]
    fn plain_stream_fails_over_in_order() {
        let c = Coordinator::default();
        let (tx, mut rx) = unbounded_channel();
        assert!(c.register(key(), -1, 1, 0, tx.clone()));
        assert!(!c.register(key(), -1, 1, 1, tx.clone()));
        assert!(!c.register(key(), -1, 2, 0, tx.clone()));
        assert_eq!(drain(&mut rx), vec![(0, true, false)]);
        c.unregister(&key(), 1, 1);
        assert!(drain(&mut rx).is_empty());
        c.unregister(&key(), 1, 0);
        assert_eq!(drain(&mut rx), vec![(0, true, false)]);
        assert!(c.is_active(&key(), 2, 0));
    }

    #[test]
    fn partition_steps_down_before_handover() {
        let c = Coordinator::default();
        let (tx, mut rx) = unbounded_channel();
        // Partition 1: with two consumers the second one owns it.
        assert!(c.register(key(), 1, 1, 0, tx.clone()));
        assert_eq!(drain(&mut rx), vec![(0, true, false)]);
        assert!(!c.register(key(), 1, 2, 7, tx.clone()));
        assert_eq!(drain(&mut rx), vec![(0, false, true)]);
        assert!(!c.is_active(&key(), 2, 7));
        c.activate(&key());
        assert_eq!(drain(&mut rx), vec![(7, true, false)]);
        // The active one leaves: the remaining one takes over directly.
        c.unregister(&key(), 2, 7);
        assert_eq!(drain(&mut rx), vec![(0, true, false)]);
    }
}
