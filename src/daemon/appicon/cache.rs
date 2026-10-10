//! A tiny least-recently-used cache bounded by both entry count and bytes. The app bubble keeps the
//! icons of the apps the user just worked in (a handful of 64 x 64 pictures), so a lookup at the
//! hotkey press is a hit; everything older is evicted instead of growing for the daemon's lifetime.
//! Linear scans are fine at this size and keep it allocation-light.

/// Entries ordered from least to most recently used.
pub struct Lru<K, V> {
    items: Vec<(K, V, usize)>,
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
}

impl<K: PartialEq, V> Lru<K, V> {
    pub const fn new(max_entries: usize, max_bytes: usize) -> Self {
        Lru { items: Vec::new(), max_entries, max_bytes, bytes: 0 }
    }

    /// Look `key` up and mark it most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let at = self.items.iter().position(|(k, _, _)| k == key)?;
        let last = self.items.len() - 1;
        self.items[at..].rotate_left(1);
        Some(&self.items[last].1)
    }

    /// Whether `key` is present, without touching the order.
    pub fn contains(&self, key: &K) -> bool {
        self.items.iter().any(|(k, _, _)| k == key)
    }

    /// Store `value` (`bytes` is its size) as most recently used, evicting the oldest entries until
    /// both bounds hold. A value larger than the whole byte budget is not stored.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        if let Some(at) = self.items.iter().position(|(k, _, _)| *k == key) {
            self.bytes -= self.items.remove(at).2;
        }
        if bytes > self.max_bytes || self.max_entries == 0 {
            return;
        }
        while !self.items.is_empty() && (self.items.len() >= self.max_entries || self.bytes + bytes > self.max_bytes) {
            self.bytes -= self.items.remove(0).2;
        }
        self.bytes += bytes;
        self.items.push((key, value, bytes));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_and_miss() {
        let mut c: Lru<&str, u32> = Lru::new(4, 100);
        assert!(c.get(&"a").is_none());
        c.insert("a", 1, 10);
        assert_eq!(c.get(&"a"), Some(&1));
        assert!(c.get(&"b").is_none());
        assert!(c.contains(&"a") && !c.contains(&"b"));
    }

    #[test]
    fn evicts_the_least_recently_used_entry_at_the_count_bound() {
        let mut c: Lru<u32, u32> = Lru::new(3, 1000);
        for k in 0..3 {
            c.insert(k, k, 1);
        }
        // touching 0 makes 1 the oldest
        assert!(c.get(&0).is_some());
        c.insert(3, 3, 1);
        assert_eq!(c.len(), 3);
        assert!(!c.contains(&1), "the oldest untouched entry went");
        assert!(c.contains(&0) && c.contains(&2) && c.contains(&3));
    }

    #[test]
    fn the_byte_budget_bounds_the_total() {
        let mut c: Lru<u32, ()> = Lru::new(100, 50);
        for k in 0..20 {
            c.insert(k, (), 20);
            assert!(c.bytes() <= 50 && c.len() <= 2, "after {k}: {} bytes, {} entries", c.bytes(), c.len());
        }
        assert!(c.contains(&19) && c.contains(&18));
    }

    #[test]
    fn a_value_over_the_whole_budget_is_not_stored_and_does_not_evict() {
        let mut c: Lru<u32, ()> = Lru::new(4, 50);
        c.insert(1, (), 20);
        c.insert(2, (), 51);
        assert!(c.contains(&1) && !c.contains(&2));
        assert_eq!(c.bytes(), 20);
    }

    #[test]
    fn reinserting_replaces_and_recounts() {
        let mut c: Lru<u32, u32> = Lru::new(4, 100);
        c.insert(1, 1, 30);
        c.insert(2, 2, 30);
        c.insert(1, 9, 10);
        assert_eq!((c.len(), c.bytes()), (2, 40));
        assert_eq!(c.get(&1), Some(&9));
        // 1 is now the newest, so 2 is evicted first
        c.insert(3, 3, 70);
        assert!(!c.contains(&2) && c.contains(&1) && c.contains(&3));
    }

    #[test]
    fn a_zero_sized_cache_stores_nothing() {
        let mut c: Lru<u32, u32> = Lru::new(0, 100);
        c.insert(1, 1, 1);
        assert_eq!(c.len(), 0);
    }
}
