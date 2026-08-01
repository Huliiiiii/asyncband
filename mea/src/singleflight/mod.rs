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

//! Singleflight provides a duplicate function call suppression mechanism.

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

/// Group represents a class of work and forms a namespace in which
/// units of work can be executed with duplicate suppression.
#[derive(Debug)]
pub struct Group<K, V, S = RandomState> {
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

    fn acquire(&self) {
        self.active_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active_calls| {
                active_calls.checked_add(1)
            })
            .expect("too many concurrent singleflight calls");
    }

    fn release(&self) -> bool {
        let active_calls = self.active_calls.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(active_calls > 0);
        active_calls == 1
    }
}

struct EntryGuard<'a, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    map: &'a Mutex<HashMap<K, Arc<Entry<V>>, S>>,
    key: &'a K,
    entry: Arc<Entry<V>>,
}

impl<K, V, S> Drop for EntryGuard<'_, K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    fn drop(&mut self) {
        if !self.entry.release() || self.entry.cell.get().is_some() {
            return;
        }

        // Keep the empty entry reachable while another caller can retry it. Removing it on the
        // first failed or cancelled attempt could let a new caller start a separate computation
        // alongside callers that are already waiting on this entry.
        let removed = {
            let mut map = self.map.lock();
            let should_remove = self.entry.active_calls.load(Ordering::Relaxed) == 0
                && self.entry.cell.get().is_none()
                && map
                    .get(self.key)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry));

            should_remove.then(|| map.remove_entry(self.key)).flatten()
        };

        drop(removed);
    }
}

impl<K, V, S> Default for Group<K, V, S>
where
    K: Eq + Hash + Clone,
    V: Clone,
    S: BuildHasher + Clone + Default,
{
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K, V> Group<K, V, RandomState>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    /// Creates a new Group with the default hasher.
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V, S> Group<K, V, S>
where
    K: Eq + Hash + Clone,
    V: Clone,
    S: BuildHasher + Clone,
{
    /// Creates a new Group with the given hasher.
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            map: Mutex::new(HashMap::with_hasher(hasher)),
        }
    }

    /// Executes and returns the results of the given function, making sure that only one execution
    /// is in-flight for a given key at a time.
    ///
    /// If a duplicate comes in, the duplicate caller waits for the original to complete and
    /// receives the same results.
    ///
    /// If the computation is cancelled or panics, another current caller may retry it. The empty
    /// entry is removed once no current callers remain.
    ///
    /// Once the function completes, the key, if not [`forgotten`], is removed from the group,
    /// allowing future calls with the same key to execute the function again.
    ///
    /// [`forgotten`]: Self::forget
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::AtomicUsize;
    /// use std::sync::atomic::Ordering;
    /// use std::time::Duration;
    ///
    /// use mea::singleflight::Group;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let group = Group::new();
    /// let counter = Arc::new(AtomicUsize::new(0));
    ///
    /// let c1 = counter.clone();
    /// let fut1 = group.work("key", || async move {
    ///     c1.fetch_add(1, Ordering::SeqCst);
    ///     // simulate heavy work to avoid immediate completion
    ///     tokio::time::sleep(Duration::from_millis(100)).await;
    ///     "result"
    /// });
    ///
    /// let c2 = counter.clone();
    /// let fut2 = group.work("key", || async move {
    ///     c2.fetch_add(1, Ordering::SeqCst);
    ///     // simulate heavy work to avoid immediate completion
    ///     tokio::time::sleep(Duration::from_millis(100)).await;
    ///     "result"
    /// });
    ///
    /// let (r1, r2) = tokio::join!(fut1, fut2);
    ///
    /// assert_eq!(r1, "result");
    /// assert_eq!(r2, "result");
    /// assert_eq!(counter.load(Ordering::SeqCst), 1);
    /// # }
    /// ```
    pub async fn work<F>(&self, key: K, func: F) -> V
    where
        F: AsyncFnOnce() -> V,
    {
        // 1. Get or create the OnceCell.
        let entry = {
            let mut map = self.map.lock();
            let entry = map
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Entry::new()))
                .clone();
            entry.acquire();
            entry
        };
        let guard = EntryGuard {
            map: &self.map,
            key: &key,
            entry,
        };

        // 2. Try to initialize the cell.
        // OnceCell::get_or_init guarantees that only one task executes the closure.
        let res = guard
            .entry
            .cell
            .get_or_init(async || {
                let result = func().await;

                // Cleanup: remove the key from the map.
                // We must ensure we remove the entry corresponding to *this* cell.
                let mut map = self.map.lock();
                if let Some(existing) = map.get(&key) {
                    // Check if the map still points to our cell.
                    if Arc::ptr_eq(&guard.entry, existing) {
                        map.remove(&key);
                    }
                }

                result
            })
            .await;

        res.clone()
    }

    /// Executes and returns the results of the given function, making sure that only one execution
    /// is in-flight for a given key at a time.
    ///
    /// If a duplicate comes in, the duplicate caller waits for the original to complete and
    /// receives the same results.
    ///
    /// If the computation fails, the error is returned for the caller. Other tasks waiting for the
    /// result will retry the computation. If the computation fails, is cancelled or panics and no
    /// current callers remain, the empty entry is removed.
    ///
    /// Once the function completes successfully, the key, if not [`forgotten`], is removed from
    /// the group, allowing future calls with the same key to execute the function again.
    ///
    /// [`forgotten`]: Self::forget
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::AtomicUsize;
    /// use std::sync::atomic::Ordering;
    /// use std::time::Duration;
    ///
    /// use mea::singleflight::Group;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let group = Group::new();
    ///
    /// let fut1 = group.try_work("key", || async move {
    ///     // simulate heavy work to avoid immediate completion
    ///     tokio::time::sleep(Duration::from_millis(100)).await;
    ///     Err::<_, &'static str>("fut1")
    /// });
    ///
    /// let fut2 = group.try_work("key", || async move {
    ///     // simulate heavy work to avoid immediate completion
    ///     tokio::time::sleep(Duration::from_millis(200)).await;
    ///     Ok::<_, &'static str>("fut2")
    /// });
    ///
    /// let (r1, r2) = tokio::join!(fut1, fut2);
    ///
    /// assert_eq!(r1, Err("fut1"));
    /// assert_eq!(r2, Ok("fut2"));
    /// # }
    /// ```
    pub async fn try_work<E, F>(&self, key: K, func: F) -> Result<V, E>
    where
        F: AsyncFnOnce() -> Result<V, E>,
    {
        // 1. Get or create the OnceCell.
        let entry = {
            let mut map = self.map.lock();
            let entry = map
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Entry::new()))
                .clone();
            entry.acquire();
            entry
        };
        let guard = EntryGuard {
            map: &self.map,
            key: &key,
            entry,
        };

        // 2. Try to initialize the cell.
        // OnceCell::get_or_try_init guarantees that only one task executes the closure.
        let res = guard
            .entry
            .cell
            .get_or_try_init(async || {
                let result = func().await?;

                // Cleanup: remove the key from the map.
                // We must ensure we remove the entry corresponding to *this* cell.
                let mut map = self.map.lock();
                if let Some(existing) = map.get(&key) {
                    // Check if the map still points to our cell.
                    if Arc::ptr_eq(&guard.entry, existing) {
                        map.remove(&key);
                    }
                }

                Ok(result)
            })
            .await?;

        Ok(res.clone())
    }

    /// Forgets about the given key.
    ///
    /// Future calls to `work` for this key will call the function rather than waiting for an
    /// earlier call to complete. Existing calls to `work` for this key are not affected.
    pub fn forget<Q>(&self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut map = self.map.lock();
        map.remove(key);
    }
}
