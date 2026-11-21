mod ingest;
mod input;
mod output;
mod parse;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use ahash::{AHashMap, RandomState};
use rayon::prelude::*;

use input::{Dimensions, Inputs, People};
use output::UserDay;
use parse::{timestamp, DAY, SESSION_GAP, WEEK};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type CampaignTotals = AHashMap<(u32, i32), CampaignStats>;

#[derive(Clone, Copy)]
struct PersonEvent {
    person: u64,
    ts: i64,
    revenue: u32,
    category: u32,
}

struct Click<'a> {
    person: u64,
    id: &'a [u8],
    ts: i64,
    campaign: u32,
}

#[derive(Clone, Copy, Default)]
struct CampaignStats {
    sessions: i64,
    purchasing_sessions: i64,
    revenue: i64,
    assists: i64,
}

impl CampaignStats {
    fn add(&mut self, other: Self) {
        self.sessions += other.sessions;
        self.purchasing_sessions += other.purchasing_sessions;
        self.revenue += other.revenue;
        self.assists += other.assists;
    }
}

// A short manifest window permits direct indexing for both credit and assists.
// Keep a sparse representation when the date range or catalog is large.
enum DailyCampaigns {
    Dense {
        first_day: i32,
        width: usize,
        cells: Vec<CampaignStats>,
    },
    Sparse(CampaignTotals),
}

impl DailyCampaigns {
    fn new(context: &Context) -> Self {
        let first_day = context.start.div_euclid(DAY);
        let days = (context.end - 1).div_euclid(DAY) - first_day + 1;
        let width = context.dimensions.campaign_names.len();
        match (days as usize).checked_mul(width) {
            Some(size) if size <= 131072 => Self::Dense {
                first_day: first_day as i32,
                width,
                cells: vec![CampaignStats::default(); size],
            },
            _ => Self::Sparse(CampaignTotals::new()),
        }
    }

    fn record(&mut self, day: i32, campaign: u32, revenue: i64, assists: &[u32]) {
        let credited = match self {
            Self::Dense {
                first_day,
                width,
                cells,
            } => {
                let begin = (day - *first_day) as usize * *width;
                let daily = &mut cells[begin..begin + *width];
                for &assist in assists {
                    daily[assist as usize].assists += 1;
                }
                &mut daily[campaign as usize]
            }
            Self::Sparse(totals) => {
                for &assist in assists {
                    totals.entry((assist, day)).or_default().assists += 1;
                }
                totals.entry((campaign, day)).or_default()
            }
        };
        credited.sessions += 1;
        credited.purchasing_sessions += i64::from(revenue > 0);
        credited.revenue += revenue;
    }

    fn merge(mut self, other: Self) -> Self {
        match (&mut self, other) {
            (Self::Dense { cells: left, .. }, Self::Dense { cells: right, .. }) => {
                for (a, b) in left.iter_mut().zip(right) {
                    a.add(b);
                }
            }
            (Self::Sparse(left), Self::Sparse(right)) => {
                for (key, stats) in right {
                    left.entry(key).or_default().add(stats);
                }
            }
            _ => unreachable!("all shards share one manifest and catalog"),
        }
        self
    }

    fn into_totals(self) -> CampaignTotals {
        match self {
            Self::Dense {
                first_day,
                width,
                cells,
            } => cells
                .chunks(width)
                .enumerate()
                .flat_map(|(offset, day)| {
                    day.iter()
                        .copied()
                        .enumerate()
                        .filter(|(_, c)| c.sessions != 0 || c.assists != 0)
                        .map(move |(campaign, c)| ((campaign as u32, first_day + offset as i32), c))
                })
                .collect(),
            Self::Sparse(totals) => totals,
        }
    }
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
    fn person(&self, user: &[u8]) -> u64 {
        self.people.resolve(user)
    }

    fn person_shard(&self, person: u64) -> usize {
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

fn finish_day(rows: &mut Vec<UserDay>, mut row: UserDay, categories: &mut [i64]) {
    if row.revenue == 0 {
        rows.push(row);
        return;
    }
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
) -> Result<DailyCampaigns> {
    let mut events = flatten(events);
    let mut clicks = flatten(clicks);
    events.sort_unstable_by(|a, b| a.person.cmp(&b.person).then(a.ts.cmp(&b.ts)));
    clicks.sort_unstable_by(|a, b| {
        a.person
            .cmp(&b.person)
            .then(a.ts.cmp(&b.ts))
            .then_with(|| a.id.cmp(b.id))
    });
    // Copies are identical after CSV parsing. They are adjacent in this order.
    clicks.dedup_by(|a, b| a.person == b.person && a.ts == b.ts && a.id == b.id);
    let mut rows = Vec::new();
    let mut totals = DailyCampaigns::new(context);
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
        let mut current: Option<UserDay> = None;
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
                person,
                day,
                sessions: 0,
                events: 0,
                revenue: 0,
                category: None,
            });
            let mut revenue = 0i64;
            for event in &person_events[first..at] {
                if event.revenue != 0 {
                    revenue += event.revenue as i64;
                    categories[event.category as usize] += event.revenue as i64;
                }
            }
            row.sessions += 1;
            row.events += (at - first) as i64;
            row.revenue += revenue;
            let campaign = attribution.advance(person_clicks, start, context.dimensions.organic);
            totals.record(day, campaign, revenue, &attribution.active);
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
        &context.people,
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
        people: People::new(),
        dimensions: Dimensions::load(&data)?,
        start,
        end,
        hash: RandomState::with_seeds(11, 23, 47, 83),
        person_shards: if small {
            1
        } else {
            (threads * 4).next_power_of_two().min(128)
        },
        event_shards: if small { 1 } else { 128 },
        output,
    };
    let (raw, ()) = rayon::join(
        || ingest::scan_events(&events, &context),
        || context.people.load_links(&links),
    );
    eprintln!(
        "events and links {:.3}s; aliases {}",
        started.elapsed().as_secs_f64(),
        context.people.len()
    );
    let (events_by_person, clicks_by_person) = rayon::join(
        || ingest::finish_events(raw, &context),
        || ingest::clicks(&clicks, &context),
    );
    eprintln!(
        "deduplication and clicks {:.3}s",
        started.elapsed().as_secs_f64()
    );
    let totals = events_by_person
        .into_par_iter()
        .zip(clicks_by_person)
        .enumerate()
        .map(|(shard, (events, clicks))| process_people(events, clicks, shard, &context))
        .try_reduce_with(|left, right| Ok(left.merge(right)))
        .expect("at least one person shard")?
        .into_totals();
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
