// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Lru-cache related utilities as quick-and-dirty wrappers around the lru-cache
//! crate.
// TODO: push changes upstream in a clean way.

extern crate heapsize;
extern crate lru_cache;

use heapsize::HeapSizeOf;
use lru_cache::LruCache;

use std::hash::Hash;

const INITIAL_CAPACITY: usize = 4;

/// An LRU-cache which operates on memory used.
pub struct MemoryLruCache<K: Eq + Hash, V: HeapSizeOf> {
	inner: LruCache<K, V>,
	cur_size: usize,
	max_size: usize,
}

// amount of memory used when the item will be put on the heap.
fn heap_size_of<T: HeapSizeOf>(val: &T) -> usize {
	::std::mem::size_of::<T>() + val.heap_size_of_children()
}

impl<K: Eq + Hash, V: HeapSizeOf> MemoryLruCache<K, V> {
	/// Create a new cache with a maximum size in bytes.
	pub fn new(max_size: usize) -> Self {
		MemoryLruCache {
			inner: LruCache::new(INITIAL_CAPACITY),
			max_size: max_size,
			cur_size: 0,
		}
	}

	/// Insert an item.
	pub fn insert(&mut self, key: K, val: V) {
		self.cur_size += heap_size_of(&val);

		// account for any element displaced from the cache.
		if let Some(lru) = self.inner.insert(key, val) {
            println!("A");
			self.cur_size -= heap_size_of(&lru);
		}
		println!("B");

		// remove elements until we are below the memory target.
		while self.cur_size > self.max_size {
			println!("C");
			match self.inner.remove_lru() {
				Some((_, v)) => self.cur_size -= heap_size_of(&v),
				_ => break,
			}
		}
	}

	/// Get a reference to an item in the cache. It is a logic error for its
	/// heap size to be altered while borrowed.
	pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
		self.inner.get_mut(key)
	}

	/// Currently-used size of values in bytes.
	pub fn current_size(&self) -> usize {
		self.cur_size
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn it_works() {
		let mut cache = MemoryLruCache::new(256);
		let val1 = vec![0u8; 100];
		let size1 = heap_size_of(&val1);
		cache.insert("hello", val1);

		assert_eq!(cache.current_size(), size1);

		let val2 = vec![0u8; 210];
		let size2 = heap_size_of(&val2);
		cache.insert("world", val2);

		assert!(cache.get_mut(&"hello").is_none());
		assert!(cache.get_mut(&"world").is_some());

		assert_eq!(cache.current_size(), size2);
	}

	#[test]
	fn it_works2() {
		let mut cache = MemoryLruCache::new(4);

		let key1 = 1;
		let val1 = true;
		cache.insert(key1, val1);

		assert!(cache.get_mut(&key1).is_some());

		assert_eq!(cache.current_size(), 1);

        let key2 = 2;
		let val2 = false;
		cache.insert(key2, val2);

		assert!(cache.get_mut(&key1).is_some());
		assert!(cache.get_mut(&key2).is_some());

		assert_eq!(cache.current_size(), 2);

        let key3 = 3;
		let val3 = false;
		cache.insert(key3, val3);

		assert!(cache.get_mut(&key1).is_some());
		assert!(cache.get_mut(&key2).is_some());
		assert!(cache.get_mut(&key3).is_some());

		assert_eq!(cache.current_size(), 3);
		assert_eq!(cache.inner.len(), 3);

		let key4 = 4;
		let val4 = false;
		cache.insert(key4, val4);

		assert!(cache.get_mut(&key1).is_some());
		assert!(cache.get_mut(&key2).is_some());
		assert!(cache.get_mut(&key3).is_some());
		assert!(cache.get_mut(&key4).is_some());

		assert_eq!(cache.current_size(), 4);
		assert_eq!(cache.inner.len(), 4);

		let key5 = 5;
		let val5 = false;
		cache.insert(key5, val5);

		assert!(cache.get_mut(&key1).is_none());
		assert!(cache.get_mut(&key2).is_none());
		assert!(cache.get_mut(&key3).is_some());
		assert!(cache.get_mut(&key4).is_some());
		assert!(cache.get_mut(&key5).is_some());

		assert_eq!(cache.current_size(), 4);
		assert_eq!(cache.inner.len(), 3);
	}
}
