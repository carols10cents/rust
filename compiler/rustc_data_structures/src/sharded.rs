use crate::fx::{FxHashMap, FxHasher};
#[cfg(parallel_compiler)]
use crate::sync::{is_dyn_thread_safe, CacheAligned};
use crate::sync::{Lock, LockGuard};
use std::borrow::Borrow;
use std::collections::hash_map::RawEntryMut;
use std::hash::{Hash, Hasher};
use std::mem;

// 32 shards is sufficient to reduce contention on an 8-core Ryzen 7 1700,
// but this should be tested on higher core count CPUs. How the `Sharded` type gets used
// may also affect the ideal number of shards.
const SHARD_BITS: usize = 5;

#[cfg(parallel_compiler)]
const SHARDS: usize = 1 << SHARD_BITS;

/// An array of cache-line aligned inner locked structures with convenience methods.
/// A single field is used when the compiler uses only one thread.
pub enum Sharded<T> {
    Single(Lock<T>),
    #[cfg(parallel_compiler)]
    Shards(Box<[CacheAligned<Lock<T>>; SHARDS]>),
}

impl<T: Default> Default for Sharded<T> {
    #[inline]
    fn default() -> Self {
        Self::new(T::default)
    }
}

impl<T> Sharded<T> {
    #[inline]
    pub fn new(mut value: impl FnMut() -> T) -> Self {
        #[cfg(parallel_compiler)]
        if is_dyn_thread_safe() {
            return Sharded::Shards(Box::new(
                [(); SHARDS].map(|()| CacheAligned(Lock::new(value()))),
            ));
        }

        Sharded::Single(Lock::new(value()))
    }

    /// The shard is selected by hashing `val` with `FxHasher`.
    #[inline]
    pub fn get_shard_by_value<K: Hash + ?Sized>(&self, _val: &K) -> &Lock<T> {
        match self {
            Self::Single(single) => &single,
            #[cfg(parallel_compiler)]
            Self::Shards(..) => self.get_shard_by_hash(make_hash(_val)),
        }
    }

    #[inline]
    pub fn get_shard_by_hash(&self, hash: u64) -> &Lock<T> {
        self.get_shard_by_index(get_shard_hash(hash))
    }

    #[inline]
    pub fn get_shard_by_index(&self, _i: usize) -> &Lock<T> {
        match self {
            Self::Single(single) => &single,
            #[cfg(parallel_compiler)]
            Self::Shards(shards) => {
                // SAFETY: The index gets ANDed with the shard mask, ensuring it is always inbounds.
                unsafe { &shards.get_unchecked(_i & (SHARDS - 1)).0 }
            }
        }
    }

    pub fn lock_shards(&self) -> Vec<LockGuard<'_, T>> {
        match self {
            Self::Single(single) => vec![single.lock()],
            #[cfg(parallel_compiler)]
            Self::Shards(shards) => shards.iter().map(|shard| shard.0.lock()).collect(),
        }
    }

    pub fn try_lock_shards(&self) -> Option<Vec<LockGuard<'_, T>>> {
        match self {
            Self::Single(single) => Some(vec![single.try_lock()?]),
            #[cfg(parallel_compiler)]
            Self::Shards(shards) => shards.iter().map(|shard| shard.0.try_lock()).collect(),
        }
    }
}

#[inline]
pub fn shards() -> usize {
    #[cfg(parallel_compiler)]
    if is_dyn_thread_safe() {
        return SHARDS;
    }

    1
}

pub type ShardedHashMap<K, V> = Sharded<FxHashMap<K, V>>;

impl<K: Eq, V> ShardedHashMap<K, V> {
    pub fn len(&self) -> usize {
        self.lock_shards().iter().map(|shard| shard.len()).sum()
    }
}

impl<K: Eq + Hash + Copy> ShardedHashMap<K, ()> {
    #[inline]
    pub fn intern_ref<Q: ?Sized>(&self, value: &Q, make: impl FnOnce() -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(value);
        let mut shard = self.get_shard_by_hash(hash).lock();
        let entry = shard.raw_entry_mut().from_key_hashed_nocheck(hash, value);

        match entry {
            RawEntryMut::Occupied(e) => *e.key(),
            RawEntryMut::Vacant(e) => {
                let v = make();
                e.insert_hashed_nocheck(hash, v, ());
                v
            }
        }
    }

    #[inline]
    pub fn intern<Q>(&self, value: Q, make: impl FnOnce(Q) -> K) -> K
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let hash = make_hash(&value);
        let mut shard = self.get_shard_by_hash(hash).lock();
        let entry = shard.raw_entry_mut().from_key_hashed_nocheck(hash, &value);

        match entry {
            RawEntryMut::Occupied(e) => *e.key(),
            RawEntryMut::Vacant(e) => {
                let v = make(value);
                e.insert_hashed_nocheck(hash, v, ());
                v
            }
        }
    }
}

pub trait IntoPointer {
    /// Returns a pointer which outlives `self`.
    fn into_pointer(&self) -> *const ();
}

impl<K: Eq + Hash + Copy + IntoPointer> ShardedHashMap<K, ()> {
    pub fn contains_pointer_to<T: Hash + IntoPointer>(&self, value: &T) -> bool {
        let hash = make_hash(&value);
        let shard = self.get_shard_by_hash(hash).lock();
        let value = value.into_pointer();
        shard.raw_entry().from_hash(hash, |entry| entry.into_pointer() == value).is_some()
    }
}

#[inline]
pub fn make_hash<K: Hash + ?Sized>(val: &K) -> u64 {
    let mut state = FxHasher::default();
    val.hash(&mut state);
    state.finish()
}

/// Get a shard with a pre-computed hash value. If `get_shard_by_value` is
/// ever used in combination with `get_shard_by_hash` on a single `Sharded`
/// instance, then `hash` must be computed with `FxHasher`. Otherwise,
/// `hash` can be computed with any hasher, so long as that hasher is used
/// consistently for each `Sharded` instance.
#[inline]
fn get_shard_hash(hash: u64) -> usize {
    let hash_len = mem::size_of::<usize>();
    // Ignore the top 7 bits as hashbrown uses these and get the next SHARD_BITS highest bits.
    // hashbrown also uses the lowest bits, so we can't use those
    (hash >> (hash_len * 8 - 7 - SHARD_BITS)) as usize
}
