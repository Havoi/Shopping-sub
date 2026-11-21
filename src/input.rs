use std::fs::{self, File};
use std::path::Path;

use ahash::{AHashMap, AHashSet};
use memmap2::Mmap;
use rayon::prelude::*;

use crate::parse::{fields, lines};
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
        let mut chunks = Vec::new();
        for map in &self.maps {
            // Each individual file has its own header, even header-only files.
            let mut start = memchr::memchr(b'\n', map).map_or(map.len(), |at| at + 1);
            while start < map.len() {
                let target = (start + 4 * 1024 * 1024).min(map.len());
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

fn find(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

pub fn linked_people(input: &Inputs) -> AHashMap<&[u8], &[u8]> {
    let mut nodes = AHashMap::new();
    let mut parent = Vec::new();
    let mut sizes = Vec::new();
    let mut smallest = Vec::new();
    for chunk in input.chunks() {
        for line in lines(chunk) {
            let Some([a, b]) = fields::<2>(line) else {
                continue;
            };
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let mut pair = [0; 2];
            for (at, name) in [a, b].into_iter().enumerate() {
                pair[at] = *nodes.entry(name).or_insert_with(|| {
                    let index = parent.len();
                    parent.push(index);
                    sizes.push(1);
                    smallest.push(name);
                    index
                });
            }
            let mut left = find(&mut parent, pair[0]);
            let mut right = find(&mut parent, pair[1]);
            if left != right {
                if sizes[left] < sizes[right] {
                    std::mem::swap(&mut left, &mut right);
                }
                parent[right] = left;
                sizes[left] += sizes[right];
                smallest[left] = smallest[left].min(smallest[right]);
            }
        }
    }
    nodes
        .into_iter()
        .map(|(name, index)| (name, smallest[find(&mut parent, index)]))
        .collect()
}

pub struct People {
    pub ids: AHashMap<Vec<u8>, u32>,
    pub names: Vec<Vec<u8>>,
}

impl People {
    pub fn load(links: &Inputs, events: &Inputs, clicks: &Inputs) -> Self {
        let linked = linked_people(links);
        let mut names = Vec::new();
        let mut components = AHashMap::new();
        let mut ids = AHashMap::with_capacity(linked.len());
        for (&id, &name) in &linked {
            let index = *components.entry(name).or_insert_with(|| {
                let index = names.len() as u32;
                names.push(name.to_vec());
                index
            });
            ids.insert(id.to_vec(), index);
        }
        drop(components);
        drop(linked);

        // Intern singleton IDs too, so sorting never needs to revisit scattered
        // user strings in the large CSV maps after the parsing phase.
        let chunks: Vec<_> = events
            .chunks()
            .into_iter()
            .map(|bytes| (7, bytes))
            .chain(clicks.chunks().into_iter().map(|bytes| (4, bytes)))
            .collect();
        let unlinked = chunks
            .par_iter()
            .fold(AHashSet::new, |mut found, &(width, bytes)| {
                for line in lines(bytes) {
                    let user = if width == 7 {
                        fields::<7>(line).map(|row| row[1])
                    } else {
                        fields::<4>(line).map(|row| row[1])
                    };
                    if let Some(user) =
                        user.filter(|user| !user.is_empty() && !ids.contains_key(*user))
                    {
                        found.insert(user);
                    }
                }
                found
            })
            .reduce(AHashSet::new, |mut left, mut right| {
                if left.len() < right.len() {
                    std::mem::swap(&mut left, &mut right);
                }
                left.extend(right);
                left
            });
        for user in unlinked {
            let index = names.len() as u32;
            names.push(user.to_vec());
            ids.insert(user.to_vec(), index);
        }
        Self { ids, names }
    }
}

pub struct Dimensions {
    pub products: AHashMap<Vec<u8>, u32>,
    pub categories: Vec<Vec<u8>>,
    pub unknown: u32,
    pub campaigns: AHashMap<Vec<u8>, u32>,
    pub campaign_names: Vec<Vec<u8>>,
    pub channels: Vec<Vec<u8>>,
    pub organic: u32,
}

impl Dimensions {
    pub fn load(data: &Path) -> Result<Self> {
        let product_bytes = fs::read(data.join("products.csv"))?;
        let product_rows: Vec<_> = lines(&product_bytes)
            .skip(1)
            .filter_map(fields::<2>)
            .collect();
        let mut categories: Vec<Vec<u8>> = product_rows.iter().map(|row| row[1].to_vec()).collect();
        categories.push(b"unknown".to_vec());
        categories.sort();
        categories.dedup();
        let unknown = categories.binary_search(&b"unknown".to_vec()).unwrap() as u32;
        let products = product_rows
            .into_iter()
            .map(|[product, category]| {
                let index = categories
                    .binary_search_by(|value| value.as_slice().cmp(category))
                    .unwrap();
                (product.to_vec(), index as u32)
            })
            .collect();
        let campaign_bytes = fs::read(data.join("campaigns.csv"))?;
        let mut campaigns = AHashMap::new();
        let mut campaign_names = Vec::new();
        let mut channels = Vec::new();
        for [campaign, channel] in lines(&campaign_bytes).skip(1).filter_map(fields::<2>) {
            campaigns.insert(campaign.to_vec(), campaign_names.len() as u32);
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
