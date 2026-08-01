// Copyright 2024 tison <wander4096@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;
use std::hash::Hash;
use std::hash::RandomState;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::internal::Mutex;
use crate::once::OnceCell;

#[cfg(test)]
mod tests;

/// A hash map that runs computation only once for each key and stores the result.
///
/// Note that this always clones the value out of the underlying map. Because of this, it's common
/// to wrap the `V` in an `Arc<V>` to make cloning cheap.
#[derive(Debug)]
pub struct OnceMap<K, V, S = RandomState> {
    map: Mutex<HashMap<K, Arc<Entry<V>>, S>>,
}

struct Entry<V> {
    cell: OnceCell<V>,
    // This counter only tracks entry liveness; the map mutex serializes attachment and cleanup.
    active_calls: AtomicUsize,
}

impl<V: fmt::Debug> fmt::Debug for Entry<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cell.fmt(f)
    }
}

impl<V> Entry<V> {
    fn new() -> Self {
        Self {
            cell: OnceCell::new(),
            active_calls: AtomicUsize::new(0),
        }
    }

    fn from_value(value: V) -> Self {
        Self {
            cell: OnceCell::from_value(value),
            active_calls: AtomicUsize::new(0),
        }
    }

    fn acquire(&self) {
        self.active_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active_calls| {
                active_calls.checked_add(1)
            })
            .expect("too many concurrent OnceMap calls");
    }

    fn release(&self) -> bool {
        let active_calls = self.active_calls.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(active_calls > 0);
        active_calls == 1
    }
}

struct EntryGuard<'a, K, V, S> {
    map: &'a Mutex<HashMap<K, Arc<Entry<V>>, S>>,
    entry: Arc<Entry<V>>,
}

impl<K, V, S> Drop for EntryGuard<'_, K, V, S> {
    fn drop(&mut self) {
        if !self.entry.release() || self.entry.cell.get().is_some() {
            return;
        }

        let mut map = self.map.lock();
        if self.entry.active_calls.load(Ordering::Relaxed) != 0 || self.entry.cell.get().is_some() {
            return;
        }

        // K does not need to be Clone, so locate the failed entry by Arc identity. This scan only
        // runs when the last caller leaves an entry uninitialized.
        map.retain(|_, entry| !Arc::ptr_eq(entry, &self.entry));
    }
}

impl<K, V, S> Default for OnceMap<K, V, S>
where
    K: Eq + Hash,
    V: Clone,
    S: BuildHasher + Clone + Default,
{
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K, V> OnceMap<K, V, RandomState>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Creates a new OnceMap with the default hasher.
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a new OnceMap with the default hasher and the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::with_capacity(capacity)),
        }
    }
}

impl<K, V, S> OnceMap<K, V, S>
where
    K: Eq + Hash,
    V: Clone,
    S: BuildHasher + Clone,
{
    /// Creates a new OnceMap with the given hasher.
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            map: Mutex::new(HashMap::with_hasher(hasher)),
        }
    }

    /// Create a OnceMap with the specified capacity and hasher.
    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            map: Mutex::new(HashMap::with_capacity_and_hasher(capacity, hasher)),
        }
    }

    /// Compute the value for the given key if absent.
    ///
    /// If the value for the key is already being computed by another task, this task will wait for
    /// the computation to finish and return the result.
    ///
    /// If the computation is cancelled or panics, another current caller may retry it. The empty
    /// entry is removed once no current callers remain.
    pub async fn compute<F>(&self, key: K, func: F) -> V
    where
        F: AsyncFnOnce() -> V,
    {
        // 1. Get or create the OnceCell.
        let entry = {
            let mut map = self.map.lock();
            let entry = map
                .entry(key)
                .or_insert_with(|| Arc::new(Entry::new()))
                .clone();
            entry.acquire();
            entry
        };
        let guard = EntryGuard {
            map: &self.map,
            entry,
        };

        // 2. Try to initialize the cell.
        // OnceCell::get_or_init guarantees that only one task executes the closure.
        let res = guard.entry.cell.get_or_init(func).await;
        res.clone()
    }

    /// Compute the value for the given key if absent.
    ///
    /// If the value for the key is already being computed by another task, this task will wait for
    /// the computation to finish and return the result.
    ///
    /// If the computation fails, the error is returned and the value is not stored. Other tasks
    /// waiting for the value will retry the computation. Once no current callers remain, the empty
    /// entry is removed.
    pub async fn try_compute<E, F>(&self, key: K, func: F) -> Result<V, E>
    where
        F: AsyncFnOnce() -> Result<V, E>,
    {
        // 1. Get or create the OnceCell.
        let entry = {
            let mut map = self.map.lock();
            let entry = map
                .entry(key)
                .or_insert_with(|| Arc::new(Entry::new()))
                .clone();
            entry.acquire();
            entry
        };
        let guard = EntryGuard {
            map: &self.map,
            entry,
        };

        // 2. Try to initialize the cell.
        // OnceCell::get_or_try_init guarantees that only one task executes the closure.
        let res = guard.entry.cell.get_or_try_init(func).await?;
        Ok(res.clone())
    }

    /// Get a clone of the value for the given key if exists.
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let map = self.map.lock();
        let entry = map.get(key)?;
        entry.cell.get().cloned()
    }

    /// Remove the given key from the map.
    ///
    /// If you need to get the value that has been removed, use the [`remove`] method instead.
    ///
    /// [`remove`]: Self::remove
    pub fn discard<Q>(&self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut map = self.map.lock();
        map.remove(key);
    }

    /// Remove the given key from the map and return a *clone* of the value if exists.
    ///
    /// If you do not need to get the value that has been removed, use the [`discard`] method
    /// instead.
    ///
    /// [`discard`]: Self::discard
    pub fn remove<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let entry = self.map.lock().remove(key)?;
        entry.cell.get().cloned()
    }
}

impl<K, V, S> FromIterator<(K, V)> for OnceMap<K, V, S>
where
    K: Eq + Hash + Clone,
    V: Clone,
    S: Default + BuildHasher + Clone,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Self {
            map: Mutex::new(
                iter.into_iter()
                    .map(|(k, v)| (k, Arc::new(Entry::from_value(v))))
                    .collect(),
            ),
        }
    }
}
