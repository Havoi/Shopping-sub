use std::fs::{self, File};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use ahash::AHashMap;
use memmap2::Mmap;
use rayon::prelude::*;

use crate::parse::{fields, lines, rows};
use crate::Result;

pub struct Inputs {
    maps: Vec<Mmap>,
}

impl Inputs {
    pub fn open(folder: &Path) -> Result<Self> {
        let mut paths = fs::read_dir(folder)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.retain(|path| path.extension().is_some_and(|extension| extension == "csv"));
        paths.sort();
        let mut maps = Vec::new();
        for path in paths {
            let file = File::open(path)?;
            if file.metadata()?.len() > 0 {
                // /data is mounted read-only. Maps own the backing storage for
                // every borrowed ID until all processing and output finish.
                maps.push(unsafe { Mmap::map(&file)? });
            }
        }
        Ok(Self { maps })
    }

    pub fn bytes(&self) -> usize {
        self.maps.iter().map(|map| map.len()).sum()
    }

    pub fn chunks(&self) -> Vec<&[u8]> {
        self.chunks_with_size(4 * 1024 * 1024)
    }
    fn chunks_with_size(&self, chunk_bytes: usize) -> Vec<&[u8]> {
        let mut chunks = Vec::new();
        for map in &self.maps {
            // Each individual file has its own header, even header-only files.
            let mut start = memchr::memchr(b'\n', map).map_or(map.len(), |at| at + 1);
            while start < map.len() {
                let target = (start + chunk_bytes).min(map.len());
                let end = if target == map.len() {
                    target
                } else {
                    memchr::memchr(b'\n', &map[target..]).map_or(map.len(), |at| target + at + 1)
                };
                chunks.push(&map[start..end]);
                start = end;
            }
        }
        chunks
    }
}

const GENERIC: u64 = 1 << 63;
const INTERNED: u64 = 3 << 62;

// The most common representation stores a 'u' prefix and up to eight suffix
// bytes. A second range stores arbitrary short strings. A third refers to
// exact byte strings in the interner. A terminal NUL takes the interner path
// because the inline representation uses trailing zero padding.
#[inline]
fn inline_user(name: &[u8]) -> Option<u64> {
    if name.len() == 9 && name[0] == b'u' {
        let last = name[8];
        if last != 0 && last < 0x80 {
            return Some(u64::from_le_bytes(name[1..9].try_into().unwrap()));
        }
    }
    if name.first() == Some(&b'u') && name.len() <= 9 {
        let suffix = &name[1..];
        if suffix.last() == Some(&0) {
            return None;
        }
        let mut bytes = [0; 8];
        bytes[..suffix.len()].copy_from_slice(suffix);
        let word = u64::from_le_bytes(bytes);
        if word & GENERIC == 0 {
            return Some(word);
        }
    }
    if name.len() <= 7 && name.last() != Some(&0) {
        let mut bytes = [0; 8];
        bytes[..name.len()].copy_from_slice(name);
        return Some(GENERIC | u64::from_le_bytes(bytes));
    }
    None
}

#[derive(Default)]
struct LongNames {
    ids: AHashMap<Arc<[u8]>, u64>,
    names: Vec<Arc<[u8]>>,
}

impl LongNames {
    fn intern(&mut self, name: &[u8]) -> u64 {
        if let Some(key) = self.ids.get(name) {
            return *key;
        }
        let key = INTERNED | self.names.len() as u64;
        let name: Arc<[u8]> = name.into();
        self.names.push(name.clone());
        self.ids.insert(name, key);
        key
    }
}

// Inline generic keys occupy only the low 56 bits below their tag. This
// unused tag value cannot be produced by inline_user or the interner.
const VACANT: u64 = GENERIC | (1 << 60);

#[derive(Clone, Copy)]
struct Slot {
    key: u64,
    value: u64,
}

// Keys and values occupy the same cache line on a successful lookup.
// Linear probing also fetches neighbouring candidates on collisions.
struct AliasTable {
    slots: Vec<Slot>,
    shift: u32,
    len: usize,
}

impl AliasTable {
    #[inline]
    fn bucket(&self, key: u64) -> usize {
        let mixed = crate::mix_key(key);
        (mixed >> self.shift) as usize
    }

    #[inline]
    fn position(&self, key: u64) -> usize {
        let mut index = self.bucket(key);
        let mask = self.slots.len() - 1;
        loop {
            let candidate = self.slots[index].key;
            if candidate == VACANT || candidate == key {
                return index;
            }
            index = (index + 1) & mask;
        }
    }

    #[inline]
    fn get(&self, key: u64) -> u64 {
        let slot = self.slots[self.position(key)];
        if slot.key == key {
            slot.value
        } else {
            key
        }
    }

    fn vertex(&mut self, key: u64) -> usize {
        let index = self.position(key);
        let slot = &mut self.slots[index];
        if slot.key == VACANT {
            *slot = Slot {
                key,
                value: index as u64,
            };
        }
        index
    }

    fn root(&mut self, mut index: usize) -> usize {
        loop {
            let parent = self.slots[index].value as usize;
            if index == parent {
                return index;
            }
            let grandparent = self.slots[parent].value as usize;
            self.slots[index].value = grandparent as u64;
            if grandparent == parent {
                return parent;
            }
            index = grandparent;
        }
    }

    #[inline]
    fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            let slot = &self.slots[self.bucket(key)];
            // The slot is inside this live table. The instruction is a cache hint.
            unsafe {
                _mm_prefetch((slot as *const Slot).cast(), _MM_HINT_T0);
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = key;
    }
}

pub struct People {
    aliases: OnceLock<AliasTable>,
    long: RwLock<LongNames>,
}

impl People {
    pub fn new() -> Self {
        Self {
            aliases: OnceLock::new(),
            long: RwLock::new(LongNames::default()),
        }
    }

    pub fn load_links(&self, links: &Inputs) {
        let edges: Vec<Vec<(u64, u64)>> = links
            .chunks_with_size(128 * 1024)
            .into_par_iter()
            .map(|chunk| {
                rows::<2>(chunk)
                    .filter(|[a, b]| !a.is_empty() && !b.is_empty() && a != b)
                    .map(|[a, b]| (self.key(a), self.key(b)))
                    .collect()
            })
            .collect();
        let count: usize = edges.iter().map(Vec::len).sum();
        // There are at most two new keys per edge. The table never grows
        // while parent links refer to its slot indices.
        let capacity = (count * 2 + count / 2 + 8).next_power_of_two();
        let mut aliases = AliasTable {
            slots: vec![
                Slot {
                    key: VACANT,
                    value: 0
                };
                capacity
            ],
            shift: 64 - capacity.trailing_zeros(),
            len: 0,
        };
        for chunk in edges {
            for (index, &(a, b)) in chunk.iter().enumerate() {
                if let Some(&(c, d)) = chunk.get(index + 24) {
                    aliases.prefetch(c);
                    aliases.prefetch(d);
                }
                let l = aliases.vertex(a);
                let r = aliases.vertex(b);
                let left = aliases.root(l);
                let right = aliases.root(r);
                if left == right {
                    continue;
                }
                let lk = aliases.slots[left].key;
                let rk = aliases.slots[right].key;
                let less = if (lk & GENERIC == 0 && rk & GENERIC == 0)
                    || (lk & INTERNED == GENERIC && rk & INTERNED == GENERIC)
                {
                    lk.swap_bytes() < rk.swap_bytes()
                } else {
                    self.name(lk) < self.name(rk)
                };
                let (small, large) = if less { (left, right) } else { (right, left) };
                aliases.slots[large].value = small as u64;
                aliases.len += 1;
            }
        }
        for i in 0..capacity {
            if aliases.slots[i].key != VACANT {
                aliases.slots[i].value = aliases.root(i) as u64;
            }
        }
        for i in 0..capacity {
            if aliases.slots[i].key != VACANT {
                let root = aliases.slots[i].value as usize;
                aliases.slots[i].value = aliases.slots[root].key;
            }
        }
        assert!(self.aliases.set(aliases).is_ok(), "links already loaded");
    }

    #[inline]
    pub fn key(&self, name: &[u8]) -> u64 {
        inline_user(name).unwrap_or_else(|| self.intern_long(name))
    }

    #[inline]
    pub fn canonical(&self, key: u64) -> u64 {
        self.aliases.get().expect("links must be loaded").get(key)
    }

    #[inline]
    pub fn resolve(&self, name: &[u8]) -> u64 {
        self.canonical(self.key(name))
    }

    #[cold]
    fn intern_long(&self, name: &[u8]) -> u64 {
        if let Some(key) = self.long.read().unwrap().ids.get(name) {
            return *key;
        }
        self.long.write().unwrap().intern(name)
    }

    pub fn with_name<T>(&self, key: u64, use_name: impl FnOnce(&[u8]) -> T) -> T {
        if key & INTERNED == INTERNED {
            let long = self.long.read().unwrap();
            return use_name(&long.names[(key & !INTERNED) as usize]);
        }
        let payload = key & !GENERIC;
        let len = 8 - (payload.leading_zeros() as usize / 8);
        let mut name = [0u8; 9];
        name[0] = b'u';
        name[1..].copy_from_slice(&payload.to_le_bytes());
        let start = usize::from(key & GENERIC != 0);
        use_name(&name[start..1 + len])
    }

    pub fn name(&self, key: u64) -> Vec<u8> {
        self.with_name(key, |name| name.to_vec())
    }

    pub fn len(&self) -> usize {
        self.aliases.get().map_or(0, |aliases| aliases.len)
    }
}

struct DimensionIndex {
    prefix: &'static [u8],
    dense: Vec<u32>,
    other: AHashMap<Vec<u8>, u32>,
}

impl DimensionIndex {
    fn new(prefix: &'static [u8], rows: usize) -> Self {
        Self {
            prefix,
            dense: vec![u32::MAX; rows + 1],
            other: AHashMap::new(),
        }
    }

    // Only the canonical ASCII decimal spelling can index the array. IDs
    // with leading zeros or arbitrary bytes use exact byte matching.
    #[inline]
    fn slot(&self, name: &[u8]) -> Option<usize> {
        let value = name.strip_prefix(self.prefix)?;
        if value.is_empty() || value.len() > 8 || (value.len() > 1 && value[0] == b'0') {
            return None;
        }
        let mut index = 0;
        for byte in value {
            let digit = byte.wrapping_sub(b'0');
            if digit > 9 {
                return None;
            }
            index = 10 * index + usize::from(digit);
        }
        (index < self.dense.len()).then_some(index)
    }

    fn insert(&mut self, name: &[u8], value: u32) {
        if let Some(index) = self.slot(name) {
            self.dense[index] = value;
        } else {
            self.other.insert(name.to_vec(), value);
        }
    }

    #[inline]
    fn get(&self, name: &[u8]) -> Option<&u32> {
        if let Some(index) = self.slot(name) {
            let value = &self.dense[index];
            (*value != u32::MAX).then_some(value)
        } else {
            self.other.get(name)
        }
    }
}

pub struct Dimensions {
    products: DimensionIndex,
    pub categories: Vec<Vec<u8>>,
    pub unknown: u32,
    campaigns: DimensionIndex,
    pub campaign_names: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
    pub organic: u32,
}

impl Dimensions {
    #[inline]
    pub fn category_for(&self, product: &[u8]) -> u32 {
        self.products.get(product).copied().unwrap_or(self.unknown)
    }

    #[inline]
    pub fn campaign_for(&self, campaign: &[u8]) -> Option<u32> {
        self.campaigns.get(campaign).copied()
    }

    pub fn load(data: &Path) -> Result<Self> {
        let product_bytes = fs::read(data.join("products.csv"))?;
        let product_rows: Vec<_> = lines(&product_bytes)
            .skip(1)
            .filter_map(fields::<2>)
            .collect();
        let mut labels: Vec<&[u8]> = product_rows.iter().map(|row| row[1]).collect();
        labels.push(b"unknown");
        labels.sort_unstable();
        labels.dedup();
        let unknown = labels.binary_search(&b"unknown".as_slice()).unwrap() as u32;
        let mut products = DimensionIndex::new(b"p", product_rows.len());
        for [product, category] in product_rows {
            products.insert(product, labels.binary_search(&category).unwrap() as u32);
        }
        let categories = labels.into_iter().map(Vec::from).collect();
        let campaign_bytes = fs::read(data.join("campaigns.csv"))?;
        let campaign_rows: Vec<_> = lines(&campaign_bytes)
            .skip(1)
            .filter_map(fields::<2>)
            .collect();
        let mut campaigns = DimensionIndex::new(b"c_", campaign_rows.len());
        let mut campaign_names = Vec::new();
        let mut channels = Vec::new();
        for [campaign, channel] in campaign_rows {
            campaigns.insert(campaign, campaign_names.len() as u32);
            campaign_names.push(campaign.to_vec());
            channels.push(channel.to_vec());
        }
        let organic = campaign_names.len() as u32;
        campaign_names.push(b"organic".to_vec());
        channels.push(b"organic".to_vec());
        Ok(Self {
            products,
            categories,
            unknown,
            campaigns,
            campaign_names,
            channels,
            organic,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn user_keys_roundtrip_exact_bytes_in_all_representations() {
        let people = People::new();
        let values: &[&[u8]] = &[
            b"",
            b"u",
            b"u\0",
            b"u\0a",
            b"a",
            b"a\0",
            b"u12345678",
            b"u1234567\0",
            b"u12345678\0",
            b"1234567",
            b"12345678",
            b"u1234567\x80",
            "雪".as_bytes(),
            "u雪雪雪".as_bytes(),
        ];
        let mut keys = HashSet::new();
        for &value in values {
            let key = people.key(value);
            assert!(keys.insert(key));
            assert_eq!(people.key(value), key);
            assert_eq!(people.name(key), value);
        }
    }

    #[test]
    fn dense_dimension_slots_keep_noncanonical_names_distinct() {
        let mut campaigns = DimensionIndex::new(b"c_", 4);
        for (index, name) in [b"c_2".as_slice(), b"c_02", b"c_22", b"other", b"c_2\0"]
            .iter()
            .enumerate()
        {
            campaigns.insert(name, index as u32);
            assert_eq!(campaigns.get(name), Some(&(index as u32)));
        }
        assert_eq!(campaigns.get(b"c_2"), Some(&0));
        assert_eq!(campaigns.get(b"c_1"), None);
        assert_eq!(campaigns.get(b"c_0002"), None);
    }
}
