//! D3Log Accumulator
//! Defines the Accumulator trait, which is an Observer and Observable simultaneously. Implements
//! the trait with a DistributingAccumulator that observes a single observable and can be
//! subscribed by multiple observers.
//!
//! Conceptually, the DistributingAccumulator consists of two different components:
//! the AccumulatingObserver and a TxnDistributor. It encapsulates the functionality
//! of those components in a single structure.
//! The AccumulatingObserver is a simple proxy between an observable and an observer that
//! inspects the data that passes through. It accumulates the data to keep track of the
//! current state.
//! The TxnDistributor is the inverse of the `TxnMux` class, it listens to a single observable and
//! is able to send data to multiple observers.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::Mutex;

use log::trace;
use uid::Id;

use differential_datalog::program::RelId;
use differential_datalog::program::Update;

use crate::Observer;
use crate::ObserverBox;
use crate::{Observable, UpdatesObservable};

use crate::accumulate::AccumulatingObserver;
use crate::accumulate::TxnDistributor;

/// A trait object that acts as a proxy between an observable and observer.
/// It accumulates the updates to maintain the current state of the data.
pub trait Accumulator<V, E>: Observer<Update<V>, E> + Observable<Update<V>, E>
where
    V: Send + Debug + Eq + Hash,
    E: Send,
{
    /// Creates a new Accumulator without any subscriptions or subscribers.
    fn new() -> Self;

    /// Returns a new Observable that can be used to listen to the outputs of the Accumulator.
    fn create_observable(&mut self) -> UpdatesObservable<Update<V>, E>;

    /// Return the current state of the data.
    fn get_current_state(&self) -> HashMap<RelId, HashSet<V>>;
}

/// An Accumulator implementation that can have multiple observers (can be subscribed to more
/// than once). Spawns an `AccumulatingObserver` to which a `TxnDistributor` is subscribed to.
#[derive(Debug)]
pub struct DistributingAccumulator<T, V, E>
where
    T: Debug + Send,
    V: Debug + Eq + Hash + Send,
    E: Debug + Send,
{
    /// The accumulator's unique ID.
    id: usize,
    /// Component responsible for accumulating the data.
    observer: AccumulatingObserver<T, V, E>,
    /// Component responsible for distributing the output to multiple observers.
    distributor: Arc<Mutex<TxnDistributor<T, E>>>,
}

impl<V, E> Accumulator<V, E> for DistributingAccumulator<Update<V>, V, E>
where
    V: Debug + Send + Clone + Eq + Hash + 'static,
    E: Debug + Send + 'static,
{
    fn new() -> Self {
        let id = Id::<()>::new().get();
        trace!("DistributingAccumulator({})::new", id);

        // Instantiate AccumulatingObserver and TxnDistributor and subscribe the latter to the former
        let mut observer = AccumulatingObserver::new();
        let distributor = Arc::new(Mutex::new(TxnDistributor::new()));
        let _subscription = observer.subscribe(Box::new(distributor.clone()));

        Self {
            id,
            observer,
            distributor,
        }
    }

    /// Creates a new `Observable` for this accumulator without the currently accumulated state.
    /// I.e. if subscribed to, the subscriber only receives transactions occurring after the subscription.
    fn create_observable(&mut self) -> UpdatesObservable<Update<V>, E> {
        trace!("DistributingAccumulator({})::create_observable()", self.id);
        self.distributor.lock().unwrap().create_observable()
    }

    fn get_current_state(&self) -> HashMap<RelId, HashSet<V>> {
        trace!("DistributingAccumulator({})::get_current_state()", self.id);
        self.observer.get_current_state()
    }
}

/// The methods for the Observable trait are delegated to the TxnDistributor
impl<V, E> Observable<Update<V>, E> for DistributingAccumulator<Update<V>, V, E>
where
    V: Debug + Send + Clone + Eq + Hash + 'static,
    E: Debug + Send + 'static,
{
    type Subscription = usize;

    fn subscribe(
        &mut self,
        mut observer: ObserverBox<Update<V>, E>,
    ) -> Result<Self::Subscription, ObserverBox<Update<V>, E>> {
        trace!("DistributingAccumulator({})::subscribe()", self.id);
        // get lock for distributor, it must not receive updates while initializing the observable
        let mut distributor = self.distributor.lock().unwrap();

        // update new observer with currently accumulated state
        let mut init_updates = self
            .get_current_state()
            .into_iter()
            .flat_map(|(relid, vs)| {
                vs.into_iter()
                    .map(|v| Update::Insert { relid, v })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        if !init_updates.is_empty() {
            let updates = init_updates.drain(..);
            trace!(
                "DistributingAccumulator({:?}) sending init_updates to observer: {:?}",
                self.id,
                updates
            );
            let _ = observer.on_start();
            let _ = observer.on_updates(Box::new(updates));
            let _ = observer.on_commit();
        }

        distributor.subscribe(observer)
    }

    fn unsubscribe(
        &mut self,
        subscription: &Self::Subscription,
    ) -> Option<ObserverBox<Update<V>, E>> {
        trace!(
            "DistributingAccumulator({})::unsubscribe({})",
            self.id,
            subscription
        );
        self.distributor.unsubscribe(subscription)
    }
}

/// All calls except `on_completed` of the Observer trait are delegated to the AccumulatingObserver.
/// `on_completed` triggers the deletion of the accumulated state for all observers.
impl<V, E> Observer<Update<V>, E> for DistributingAccumulator<Update<V>, V, E>
where
    V: Debug + Send + Eq + Hash + Clone + 'static,
    E: Debug + Send + 'static,
{
    fn on_start(&mut self) -> Result<(), E> {
        trace!("DistributingAccumulator({})::on_start", self.id);
        self.observer.on_start()
    }

    fn on_commit(&mut self) -> Result<(), E> {
        trace!("DistributingAccumulator({})::on_commit", self.id);
        self.observer.on_commit()
    }

    fn on_updates<'a>(
        &mut self,
        updates: Box<dyn Iterator<Item = Update<V>> + 'a>,
    ) -> Result<(), E> {
        trace!("DistributingAccumulator({})::on_updates", self.id);
        self.observer.on_updates(updates)
    }

    /// sends a deletion update to all observers, thus clearing the accumulated state.
    fn on_completed(&mut self) -> Result<(), E> {
        trace!("DistributingAccumulator({})::on_completed", self.id);
        let mut distributor = self.distributor.lock().unwrap();
        let _ = distributor.on_completed();

        let mut delete_updates = self
            .get_current_state()
            .into_iter()
            .flat_map(|(relid, vs)| {
                vs.into_iter()
                    .map(|v| Update::DeleteValue { relid, v })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        if !delete_updates.is_empty() {
            let updates = delete_updates.drain(..);
            trace!(
                "DistributingAccumulator({:?}) clearing state of observers: {:?}",
                self.id,
                updates
            );
            let _ = distributor.on_start();
            let _ = distributor.on_updates(Box::new(updates));
            let _ = distributor.on_commit();
        }

        self.observer.on_completed()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::Mutex;
    use std::vec::IntoIter;

    use crate::accumulate::{eq_updates, UpdatesMockObserver};
    use crate::MockObserver;

    fn get_usize_updates_1() -> Box<IntoIter<Update<usize>>> {
        Box::new(
            vec![
                Update::Insert { relid: 1, v: 1 },
                Update::Insert { relid: 2, v: 2 },
                Update::Insert { relid: 3, v: 3 },
            ]
            .into_iter(),
        )
    }

    fn get_usize_updates_2() -> Box<IntoIter<Update<usize>>> {
        Box::new(
            vec![
                Update::Insert { relid: 1, v: 2 },
                Update::Insert { relid: 1, v: 3 },
                Update::Insert { relid: 2, v: 3 },
            ]
            .into_iter(),
        )
    }

    fn get_usize_updates_3() -> Box<IntoIter<Update<usize>>> {
        Box::new(
            vec![
                Update::Insert { relid: 4, v: 1 },
                Update::Insert { relid: 4, v: 2 },
                Update::Insert { relid: 4, v: 3 },
                Update::Insert { relid: 4, v: 4 },
            ]
            .into_iter(),
        )
    }

    fn get_usize_delete_updates_1() -> Box<IntoIter<Update<usize>>> {
        Box::new(
            vec![
                Update::DeleteValue { relid: 1, v: 1 },
                Update::DeleteValue { relid: 2, v: 2 },
                Update::DeleteValue { relid: 3, v: 3 },
            ]
            .into_iter(),
        )
    }

    /// Test subscribing and unsubscribing for a `DistributingAccumulator`.
    /// A subscription can occur directly via `subscribe` or via `create_observable`.
    #[test]
    fn subscribe_unsubscribe() {
        let mut accumulator = DistributingAccumulator::<Update<()>, (), ()>::new();
        let observer = Box::new(MockObserver::new());

        let subscription = accumulator.subscribe(observer);
        assert!(subscription.is_ok());
        assert!(accumulator.unsubscribe(&subscription.unwrap()).is_some());

        let mut observable = accumulator.create_observable();
        let observer = Box::new(MockObserver::new());

        let subscription = observable.subscribe(observer);
        assert!(subscription.is_ok());
        assert!(observable.unsubscribe(&subscription.unwrap()).is_some());
    }

    /// Test multiple direct subscriptions via `subscribe` to a `AccumulatingObserver`.
    #[test]
    fn multiple_subscribe_direct_distributor() {
        let mut accumulator = DistributingAccumulator::<Update<usize>, usize, ()>::new();
        let mock1 = Arc::new(Mutex::new(MockObserver::new()));
        let mock2 = Arc::new(Mutex::new(MockObserver::new()));

        assert!(accumulator.subscribe(Box::new(mock1.clone())).is_ok());
        assert!(accumulator.subscribe(Box::new(mock2.clone())).is_ok());

        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 1);
        assert_eq!(mock2.lock().unwrap().called_on_start, 1);

        assert_eq!(accumulator.on_updates(get_usize_updates_1()), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_updates, 3);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 3);

        assert_eq!(accumulator.on_commit(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_commit, 1);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 1);

        assert_eq!(accumulator.on_completed(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 2);
        assert_eq!(mock2.lock().unwrap().called_on_start, 2);
        assert_eq!(mock1.lock().unwrap().called_on_updates, 6);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 6);
        assert_eq!(mock1.lock().unwrap().called_on_commit, 2);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 2);
        assert_eq!(mock1.lock().unwrap().called_on_completed, 1);
        assert_eq!(mock2.lock().unwrap().called_on_completed, 1);
    }

    /// Test multiple indirect subscriptions via `create_observable` to a `DistributingAccumulator`.
    #[test]
    fn multiple_subscribe_indirect_distributor() {
        let mut accumulator = DistributingAccumulator::<Update<usize>, usize, ()>::new();
        let mock1 = Arc::new(Mutex::new(MockObserver::new()));
        let mock2 = Arc::new(Mutex::new(MockObserver::new()));
        let mut observable1 = accumulator.create_observable();
        let mut observable2 = accumulator.create_observable();

        assert!(observable1.subscribe(Box::new(mock1.clone())).is_ok());
        assert!(observable2.subscribe(Box::new(mock2.clone())).is_ok());

        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 1);
        assert_eq!(mock2.lock().unwrap().called_on_start, 1);

        assert_eq!(accumulator.on_updates(get_usize_updates_1()), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_updates, 3);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 3);

        assert_eq!(accumulator.on_commit(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_commit, 1);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 1);
    }

    /// Test pass-through filter behaviour for transactions via a `DistributingAccumulator`.
    #[test]
    fn transparent_transactions_proxy() {
        let mut accumulator = DistributingAccumulator::<Update<usize>, usize, ()>::new();
        let mock1 = Arc::new(Mutex::new(MockObserver::new()));
        let mock2 = Arc::new(Mutex::new(MockObserver::new()));
        let mut observable = accumulator.create_observable();

        assert!(observable.subscribe(Box::new(mock1.clone())).is_ok());
        assert!(accumulator.subscribe(Box::new(mock2.clone())).is_ok());

        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 1);
        assert_eq!(mock2.lock().unwrap().called_on_start, 1);

        assert_eq!(accumulator.on_updates(get_usize_updates_1()), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_updates, 3);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 3);

        assert_eq!(accumulator.on_updates(get_usize_updates_2()), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_updates, 6);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 6);

        assert_eq!(accumulator.on_commit(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_commit, 1);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 1);

        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 2);
        assert_eq!(mock2.lock().unwrap().called_on_start, 2);

        assert_eq!(accumulator.on_updates(get_usize_updates_3()), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_updates, 10);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 10);

        assert_eq!(accumulator.on_commit(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_commit, 2);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 2);

        assert_eq!(accumulator.on_completed(), Ok(()));
        assert_eq!(mock1.lock().unwrap().called_on_start, 3);
        assert_eq!(mock2.lock().unwrap().called_on_start, 3);
        assert_eq!(mock1.lock().unwrap().called_on_updates, 20);
        assert_eq!(mock2.lock().unwrap().called_on_updates, 20);
        assert_eq!(mock1.lock().unwrap().called_on_commit, 3);
        assert_eq!(mock2.lock().unwrap().called_on_commit, 3);
        assert_eq!(mock1.lock().unwrap().called_on_completed, 1);
        assert_eq!(mock2.lock().unwrap().called_on_completed, 1);
    }

    /// when a new downstream consumer subscribes, it should be updated with the current values
    #[test]
    fn test_fix_up_updates() {
        let mut accumulator = DistributingAccumulator::<Update<usize>, usize, ()>::new();
        let mock1 = Arc::new(Mutex::new(UpdatesMockObserver::new()));
        let mock2 = Arc::new(Mutex::new(UpdatesMockObserver::new()));
        let mock3 = Arc::new(Mutex::new(UpdatesMockObserver::new()));

        // start with one observer and give it updates
        assert!(accumulator.subscribe(Box::new(mock1.clone())).is_ok());
        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_updates_1()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));
        let received_updates = mock1.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 3);
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 1, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 2, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 3, v: 3 })));

        // create an un-accumulated observable
        let mut observable = accumulator.create_observable();

        // provide second batch of updates to the accumulator
        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_updates_2()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));

        // the observable does not see any updates until subscribed too
        assert!(observable.subscribe(Box::new(mock2.clone())).is_ok());
        let received_updates = mock2.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 0);

        // the following updates should be received via the create_observable
        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_updates_3()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));
        let received_updates = mock2.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 4);
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 4 })));

        // a new observer via subscribe() should receive all updates
        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_delete_updates_1()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));

        assert!(accumulator.subscribe(Box::new(mock3.clone())).is_ok());
        let received_updates = mock3.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 7);
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 1, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 1, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 2, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 4 })));
    }

    /// when an upstream source calls `on_completed`,
    /// the accumulated state should be removed from observers
    #[test]
    fn test_delete_updates() {
        let mut accumulator = DistributingAccumulator::<Update<usize>, usize, ()>::new();
        let mock1 = Arc::new(Mutex::new(UpdatesMockObserver::new()));
        let mock2 = Arc::new(Mutex::new(UpdatesMockObserver::new()));

        assert!(accumulator.subscribe(Box::new(mock1.clone())).is_ok());
        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_updates_1()), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_updates_3()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));

        assert_eq!(accumulator.on_start(), Ok(()));
        assert_eq!(accumulator.on_updates(get_usize_delete_updates_1()), Ok(()));
        assert_eq!(accumulator.on_commit(), Ok(()));

        assert!(accumulator.subscribe(Box::new(mock2.clone())).is_ok());
        let received_updates = mock2.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 4);
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::Insert { relid: 4, v: 4 })));

        assert_eq!(accumulator.on_completed(), Ok(()));

        let received_updates = mock1.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 14); // 3 + 4 inserts, 3 manual deletions, 4 automatic deletions
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 4 })));

        let received_updates = mock2.lock().unwrap().received_updates.clone();
        assert_eq!(received_updates.len(), 8); // 4 inserts, 4 automatic deletions
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 1 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 2 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 3 })));
        assert!(received_updates
            .iter()
            .any(|u| eq_updates(u, &Update::DeleteValue { relid: 4, v: 4 })));
    }
}
