mod input;
mod output;
mod parse;

use std::collections::hash_map::Entry;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Instant;

use ahash::{AHashMap, RandomState};
use rayon::prelude::*;

use input::{Dimensions, Inputs, People};
use output::UserDay;
use parse::{fields, lines, timestamp, unsigned, DAY, SESSION_GAP, WEEK};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type CampaignTotals = AHashMap<(u32, i32), CampaignStats>;

#[derive(Clone, Copy)]
enum EventKey<'a> {
    Inline { bytes: [u8; 15], length: u8 },
    Borrowed(&'a [u8]),
}

impl<'a> EventKey<'a> {
    fn new(value: &'a [u8]) -> Self {
        if value.len() <= 15 {
            let mut bytes = [0; 15];
            bytes[..value.len()].copy_from_slice(value);
            Self::Inline {
                bytes,
                length: value.len() as u8,
            }
        } else {
            Self::Borrowed(value)
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Self::Inline { bytes, length } => &bytes[..*length as usize],
            Self::Borrowed(bytes) => bytes,
        }
    }
}

impl Hash for EventKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bytes().hash(state);
    }
}

impl PartialEq for EventKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for EventKey<'_> {}

#[derive(Clone, Copy)]
struct Event<'a> {
    id: EventKey<'a>,
    person: u32,
    ts: i64,
    ingest: i64,
    revenue: u32,
    category: u32,
}

#[derive(Clone, Copy)]
struct PersonEvent {
    person: u32,
    ts: i64,
    revenue: u32,
    category: u32,
}

struct Click<'a> {
    person: u32,
    id: &'a [u8],
    ts: i64,
    campaign: u32,
}

#[derive(Default)]
struct CampaignStats {
    sessions: i64,
    purchasing_sessions: i64,
    revenue: i64,
    assists: i64,
}

struct Context {
    people: People,
    dimensions: Dimensions,
    start: i64,
    end: i64,
    hash: RandomState,
    person_shards: usize,
    event_shards: usize,
    output: PathBuf,
}

impl Context {
    fn person(&self, user: &[u8]) -> u32 {
        self.people.ids.get(user).copied().unwrap_or(u32::MAX)
    }

    fn person_shard(&self, person: u32) -> usize {
        self.hash.hash_one(person) as usize & (self.person_shards - 1)
    }
}

fn buckets<T>(count: usize) -> Vec<Vec<T>> {
    (0..count).map(|_| Vec::new()).collect()
}

// Transpose ownership of buffers without copying their rows.
fn transpose<T>(groups: Vec<Vec<Vec<T>>>, count: usize) -> Vec<Vec<Vec<T>>> {
    let mut result = buckets(count);
    for group in groups {
        for (index, values) in group.into_iter().enumerate() {
            if !values.is_empty() {
                result[index].push(values);
            }
        }
    }
    result
}

fn flatten<T>(parts: Vec<Vec<T>>) -> Vec<T> {
    let mut result = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for mut part in parts {
        result.append(&mut part);
    }
    result
}

fn read_events<'a>(input: &'a Inputs, context: &Context) -> Vec<Vec<Vec<Event<'a>>>> {
    let chunks = input.chunks();
    let groups: Vec<_> = chunks
        .par_chunks(16)
        .map(|chunks| {
            let mut groups = buckets(context.event_shards);
            for &chunk in chunks {
                for line in lines(chunk) {
                    let Some([id, user, ts, ingest, kind, product, price]) = fields::<7>(line)
                    else {
                        continue;
                    };
                    if id.is_empty() {
                        continue;
                    }
                    let ingest = timestamp(ingest).expect("ingest_ts must be valid F1");
                    let purchase = kind == b"purchase";
                    let revenue = if purchase {
                        unsigned(price)
                            .filter(|&price| (1..=10_000_000).contains(&price))
                            .unwrap_or(0) as u32
                    } else {
                        0
                    };
                    let valid = !user.is_empty()
                        && !product.is_empty()
                        && (kind == b"view" || kind == b"cart" || (purchase && revenue > 0));
                    let ts = timestamp(ts)
                        .filter(|&ts| valid && ts >= context.start && ts < context.end)
                        .unwrap_or(i64::MIN);
                    let category = if revenue > 0 {
                        *context
                            .dimensions
                            .products
                            .get(product)
                            .unwrap_or(&context.dimensions.unknown)
                    } else {
                        context.dimensions.unknown
                    };
                    let hash = context.hash.hash_one(id);
                    let shard = hash as usize & (context.event_shards - 1);
                    // Validation is recorded here, but invalid records remain until
                    // global event_id deduplication chooses the earliest ingestion.
                    groups[shard].push(Event {
                        id: EventKey::new(id),
                        person: context.person(user),
                        ts,
                        ingest,
                        revenue,
                        category,
                    });
                }
            }
            groups
        })
        .collect();
    transpose(groups, context.event_shards)
}

fn deduplicate<'a>(parts: Vec<Vec<Event<'a>>>, context: &Context) -> Vec<Vec<PersonEvent>> {
    let count: usize = parts.iter().map(Vec::len).sum();
    let mut earliest: AHashMap<EventKey<'a>, Event<'a>> = AHashMap::with_capacity(count);
    for part in parts {
        for event in part {
            match earliest.entry(event.id) {
                Entry::Vacant(entry) => {
                    entry.insert(event);
                }
                Entry::Occupied(mut entry) => {
                    if event.ingest < entry.get().ingest {
                        entry.insert(event);
                    }
                }
            }
        }
    }
    let mut groups = buckets(context.person_shards);
    for (_, event) in earliest {
        if event.ts != i64::MIN && event.ingest - event.ts <= DAY {
            groups[context.person_shard(event.person)].push(PersonEvent {
                person: event.person,
                ts: event.ts,
                revenue: event.revenue,
                category: event.category,
            });
        }
    }
    groups
}

fn read_clicks<'a>(input: &'a Inputs, context: &Context) -> Vec<Vec<Vec<Click<'a>>>> {
    let chunks = input.chunks();
    let groups: Vec<_> = chunks
        .par_chunks(16)
        .map(|chunks| {
            let mut groups = buckets(context.person_shards);
            for &chunk in chunks {
                for line in lines(chunk) {
                    let Some([id, user, campaign, ts]) = fields::<4>(line) else {
                        continue;
                    };
                    if id.is_empty() || user.is_empty() {
                        continue;
                    }
                    let Some(&campaign) = context.dimensions.campaigns.get(campaign) else {
                        continue;
                    };
                    let Some(ts) =
                        timestamp(ts).filter(|&ts| ts >= context.start - WEEK && ts < context.end)
                    else {
                        continue;
                    };
                    let person = context.person(user);
                    groups[context.person_shard(person)].push(Click {
                        person,
                        id,
                        ts,
                        campaign,
                    });
                }
            }
            groups
        })
        .collect();
    transpose(groups, context.person_shards)
}

struct Attribution {
    counts: Vec<usize>,
    positions: Vec<usize>,
    active: Vec<u32>,
    head: usize,
    tail: usize,
    winner: Option<usize>,
}

impl Attribution {
    fn new(campaigns: usize) -> Self {
        Self {
            counts: vec![0; campaigns],
            positions: vec![0; campaigns],
            active: Vec::new(),
            head: 0,
            tail: 0,
            winner: None,
        }
    }

    fn reset(&mut self) {
        for campaign in self.active.drain(..) {
            self.counts[campaign as usize] = 0;
        }
        self.head = 0;
        self.tail = 0;
        self.winner = None;
    }

    fn advance(&mut self, clicks: &[Click<'_>], start: i64, organic: u32) -> u32 {
        while self.tail < clicks.len() && clicks[self.tail].ts <= start {
            let click = &clicks[self.tail];
            let campaign = click.campaign as usize;
            if self.counts[campaign] == 0 {
                self.positions[campaign] = self.active.len();
                self.active.push(click.campaign);
            }
            self.counts[campaign] += 1;
            // Sorting ascending by ID puts the smallest ID first at each ts.
            if self
                .winner
                .is_none_or(|winner| clicks[winner].ts < click.ts)
            {
                self.winner = Some(self.tail);
            }
            self.tail += 1;
        }
        while self.head < self.tail && clicks[self.head].ts < start - WEEK {
            let campaign = clicks[self.head].campaign as usize;
            self.counts[campaign] -= 1;
            if self.counts[campaign] == 0 {
                let position = self.positions[campaign];
                self.active.swap_remove(position);
                if position < self.active.len() {
                    self.positions[self.active[position] as usize] = position;
                }
            }
            self.head += 1;
        }
        self.winner
            .filter(|&winner| winner >= self.head)
            .map_or(organic, |winner| clicks[winner].campaign)
    }
}

fn finish_day<'a>(rows: &mut Vec<UserDay<'a>>, mut row: UserDay<'a>, categories: &mut [i64]) {
    let mut highest = 0;
    // Category indices are assigned in byte order, so the first maximum wins.
    for (index, revenue) in categories.iter_mut().enumerate() {
        if *revenue > highest {
            highest = *revenue;
            row.category = Some(index as u32);
        }
        *revenue = 0;
    }
    rows.push(row);
}

fn process_people(
    events: Vec<Vec<PersonEvent>>,
    clicks: Vec<Vec<Click<'_>>>,
    shard: usize,
    context: &Context,
) -> Result<CampaignTotals> {
    let mut events = flatten(events);
    let mut clicks = flatten(clicks);
    events.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)));
    clicks.sort_unstable_by(|a, b| {
        a.person
            .cmp(&b.person)
            .then(a.ts.cmp(&b.ts))
            .then(a.id.cmp(b.id))
    });
    // Copies are identical after CSV parsing. They are adjacent in this order.
    clicks.dedup_by(|a, b| a.person == b.person && a.ts == b.ts && a.id == b.id);
    let mut rows = Vec::new();
    let mut totals = CampaignTotals::new();
    let mut categories = vec![0i64; context.dimensions.categories.len()];
    let mut attribution = Attribution::new(context.dimensions.campaign_names.len());
    let mut click_at = 0;
    for person_events in events.chunk_by(|a, b| a.person == b.person) {
        let person = person_events[0].person;
        while click_at < clicks.len() && clicks[click_at].person < person {
            click_at += 1;
        }
        let first_click = click_at;
        while click_at < clicks.len() && clicks[click_at].person == person {
            click_at += 1;
        }
        let person_clicks = &clicks[first_click..click_at];
        attribution.reset();
        let mut current: Option<UserDay<'_>> = None;
        let mut at = 0;
        while at < person_events.len() {
            let first = at;
            let start = person_events[first].ts;
            let day = start.div_euclid(DAY) as i32;
            at += 1;
            while at < person_events.len()
                && person_events[at].ts - person_events[at - 1].ts <= SESSION_GAP
            {
                at += 1;
            }
            if current.as_ref().is_some_and(|row| row.day != day) {
                finish_day(&mut rows, current.take().unwrap(), &mut categories);
            }
            let row = current.get_or_insert(UserDay {
                person: &context.people.names[person as usize],
                day,
                sessions: 0,
                events: 0,
                revenue: 0,
                category: None,
            });
            let mut revenue = 0i64;
            for event in &person_events[first..at] {
                revenue += event.revenue as i64;
                categories[event.category as usize] += event.revenue as i64;
            }
            row.sessions += 1;
            row.events += (at - first) as i64;
            row.revenue += revenue;
            let campaign = attribution.advance(person_clicks, start, context.dimensions.organic);
            let stats = totals.entry((campaign, day)).or_default();
            stats.sessions += 1;
            stats.purchasing_sessions += i64::from(revenue > 0);
            stats.revenue += revenue;
            for &campaign in &attribution.active {
                totals.entry((campaign, day)).or_default().assists += 1;
            }
        }
        if let Some(row) = current {
            finish_day(&mut rows, row, &mut categories);
        }
    }
    output::users(
        &context
            .output
            .join("user_daily")
            .join(format!("part-{shard:03}.parquet")),
        &rows,
        &context.dimensions,
    )?;
    Ok(totals)
}

fn run() -> Result<()> {
    let started = Instant::now();
    let data = PathBuf::from(env::var_os("DATA_DIR").unwrap_or_else(|| "/data".into()));
    let output = PathBuf::from(env::var_os("OUTPUT_DIR").unwrap_or_else(|| "/output".into()));
    let threads: usize = env::var("NCPU")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, usize::from))
        .max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let start = timestamp(
        manifest["window_start"]
            .as_str()
            .ok_or("missing window_start")?
            .as_bytes(),
    )
    .ok_or("bad window_start")?;
    let end = timestamp(
        manifest["window_end"]
            .as_str()
            .ok_or("missing window_end")?
            .as_bytes(),
    )
    .ok_or("bad window_end")?;
    if end <= start {
        return Err("window_end must follow window_start".into());
    }
    let links = Inputs::open(&data.join("links"))?;
    let events = Inputs::open(&data.join("events"))?;
    let clicks = Inputs::open(&data.join("clicks"))?;
    let small = events.bytes() < 1024 * 1024;
    let context = Context {
        people: People::load(&links, &events, &clicks),
        dimensions: Dimensions::load(&data)?,
        start,
        end,
        hash: RandomState::with_seeds(11, 23, 47, 83),
        person_shards: if small {
            1
        } else {
            (threads * 8).next_power_of_two().min(128)
        },
        event_shards: if small { 1 } else { 256 },
        output,
    };
    eprintln!(
        "identity and lookups {:.3}s; user IDs {}",
        started.elapsed().as_secs_f64(),
        context.people.ids.len()
    );
    let raw = read_events(&events, &context);
    eprintln!("parsed events {:.3}s", started.elapsed().as_secs_f64());
    let groups: Vec<_> = raw
        .into_par_iter()
        .map(|parts| deduplicate(parts, &context))
        .collect();
    let events_by_person = transpose(groups, context.person_shards);
    eprintln!(
        "deduplicated events {:.3}s",
        started.elapsed().as_secs_f64()
    );
    let clicks_by_person = read_clicks(&clicks, &context);
    eprintln!("parsed clicks {:.3}s", started.elapsed().as_secs_f64());
    let totals = events_by_person
        .into_par_iter()
        .zip(clicks_by_person)
        .enumerate()
        .map(|(shard, (events, clicks))| process_people(events, clicks, shard, &context))
        .try_reduce(CampaignTotals::new, |mut left, right| {
            for (key, value) in right {
                let stats = left.entry(key).or_default();
                stats.sessions += value.sessions;
                stats.purchasing_sessions += value.purchasing_sessions;
                stats.revenue += value.revenue;
                stats.assists += value.assists;
            }
            Ok(left)
        })?;
    output::campaigns(
        &context.output.join("campaign_daily/part-000.parquet"),
        &totals,
        &context.dimensions,
    )?;
    eprintln!(
        "complete {:.3}s; sessions {}; revenue_cents {}",
        started.elapsed().as_secs_f64(),
        totals.values().map(|row| row.sessions).sum::<i64>(),
        totals.values().map(|row| row.revenue).sum::<i64>()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("shopping-sessions: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_event_ids_preserve_full_values() {
        let values: [&[u8]; 5] = [
            b"event-a",
            b"event-a\0",
            b"123456789012345",
            b"1234567890123456",
            b"1234567890123457",
        ];
        let mut table = AHashMap::new();
        for (index, value) in values.iter().enumerate() {
            table.insert(EventKey::new(value), index);
        }
        assert_eq!(table.len(), values.len());
        for (index, value) in values.iter().enumerate() {
            assert_eq!(table[&EventKey::new(value)], index);
        }
    }
}
