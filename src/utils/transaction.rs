// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use atomic::Ordering;
use calloop::ping::{make_ping, Ping};
use calloop::LoopHandle;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::compositor::{Blocker, BlockerState};

use crate::utils::timers::Timers;

/// Default time limit, after which the transaction completes.
///
/// Serves to avoid hanging when a client fails to respond to a configure promptly.
const TIME_LIMIT: Duration = Duration::from_millis(300);

/// Transaction between Wayland clients.
///
/// How to use it:
/// 1. Create a transaction with [`Transaction::new()`].
/// 2. Clone it as many times as you need.
/// 3. Before adding the transaction as a commit blocker, remember to call
///    [`Transaction::add_notification()`] to receive a notification when the transaction completes.
/// 4. Before adding the transaction as a commit blocker, remember to call
///    [`Transaction::register_deadline_timer()`] to make sure the transaction completes when
///    reaching the deadline.
/// 5. In your surface pre-commit handler, if the transaction corresponding to that commit isn't
///    ready, get a blocker with [`Transaction::blocker()`] and add it to the surface.
#[derive(Debug, Clone)]
pub struct Transaction {
    inner: Arc<Inner>,
    deadline: Rc<RefCell<Deadline>>,
}

/// Blocker for a [`Transaction`].
#[derive(Debug)]
pub struct TransactionBlocker(Weak<Inner>);

#[derive(Debug)]
enum Deadline {
    NotRegistered,
    Registered { remove: Ping },
}

#[derive(Debug)]
struct Inner {
    /// Whether the transaction is completed.
    completed: AtomicBool,
    /// Notifications to send out upon completing the transaction.
    notifications: Mutex<Option<(Sender<Client>, Vec<Client>)>>,
}

impl Transaction {
    /// Creates a new transaction.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
            deadline: Rc::new(RefCell::new(Deadline::NotRegistered)),
        }
    }

    /// Gets a blocker for this transaction.
    pub fn blocker(&self) -> TransactionBlocker {
        trace!(transaction = ?Arc::as_ptr(&self.inner), "generating blocker");
        TransactionBlocker(Arc::downgrade(&self.inner))
    }

    /// Adds a notification for when this transaction completes.
    pub fn add_notification(&self, sender: Sender<Client>, client: Client) {
        if self.is_completed() {
            error!("tried to add notification to a completed transaction");
            return;
        }

        let mut guard = self.inner.notifications.lock().unwrap();
        guard.get_or_insert((sender, Vec::new())).1.push(client);
    }

    /// Registers this transaction's deadline timer, so a client that never answers its configure
    /// cannot hang the commit for longer than [`TIME_LIMIT`].
    ///
    /// On the compositor's clock like every other deadline (see [`crate::utils::timers`]), which
    /// is what lets a test reach this path at all: it used to be `#[cfg(not(test))]`, disabled
    /// wholesale under test because the only way to reach a real-time deadline was to sleep out
    /// 300 ms per transaction.
    /// Takes the wheel and the loop handle rather than `&mut Synoik`, because the pre-commit hook
    /// that calls it is holding a borrow of the layout.
    pub fn register_deadline_timer(
        &self,
        now: Duration,
        timers: &mut Timers<crate::synoik::State>,
        event_loop: &LoopHandle<'static, crate::synoik::State>,
    ) {
        let mut cell = self.deadline.borrow_mut();
        if !matches!(*cell, Deadline::NotRegistered) {
            return;
        }

        let inner = Arc::downgrade(&self.inner);
        let token = timers.insert_at(now + TIME_LIMIT, move |_state| {
            let _span =
                trace_span!("deadline timer", transaction = ?Weak::as_ptr(&inner)).entered();

            if let Some(inner) = inner.upgrade() {
                trace!("deadline reached, completing transaction");
                inner.complete();
            } else {
                // We should cancel the timer automatically. But this callback can still just
                // happen to run while the ping callback is scheduled, leading to this branch
                // being legitimately taken.
                trace!("transaction completed without cancelling the timer");
            }

            None
        });

        // Add a ping source that will be used to cancel the timer automatically.
        let (ping, source) = make_ping().unwrap();
        event_loop
            .insert_source(source, move |_, _, state: &mut crate::synoik::State| {
                state.synoik.cancel_timer(token);
            })
            .unwrap();

        *cell = Deadline::Registered { remove: ping };
    }

    /// Returns whether this transaction has already completed.
    pub fn is_completed(&self) -> bool {
        self.inner.is_completed()
    }

    /// Returns whether this is the last instance of this transaction.
    pub fn is_last(&self) -> bool {
        Arc::strong_count(&self.inner) == 1
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        let _span = trace_span!("drop", transaction = ?Arc::as_ptr(&self.inner)).entered();

        if self.is_last() {
            // If this was the last transaction, complete it.
            trace!("last transaction dropped, completing");
            self.inner.complete();

            // Also remove the timer.
            if let Deadline::Registered { remove } = &*self.deadline.borrow() {
                remove.ping();
            };
        }
    }
}

impl TransactionBlocker {
    pub fn completed() -> Self {
        Self(Weak::new())
    }
}

impl Blocker for TransactionBlocker {
    fn state(&self) -> BlockerState {
        if self.0.upgrade().is_none_or(|x| x.is_completed()) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

impl Inner {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            notifications: Mutex::new(None),
        }
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Relaxed)
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Relaxed);

        let mut guard = self.notifications.lock().unwrap();
        if let Some((sender, clients)) = guard.take() {
            for client in clients {
                if let Err(err) = sender.send(client) {
                    warn!("error sending blocker notification: {err:?}");
                };
            }
        }
    }
}
