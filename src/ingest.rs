use ahash::AHashMap;
use rayon::prelude::*;

use crate::input::Inputs;
use crate::parse::{rows, unsigned, TimestampParser, DAY, WEEK};
use crate::{buckets, transpose, Click, Context, PersonEvent};

#[derive(Clone, Copy)]
struct Payload {
    person: u64,
    time: i64,
    ingest: i64,
    revenue: u32,
    category: u32,
}

// Nonpurchasing events have no financial payload. Store just the exact
// millisecond window offset and whole-second ingestion for this common case.
// Purchases and timestamps outside the compact ranges use full records.
#[derive(Clone, Copy)]
struct Visit {
    person: u64,
    time: u32,
    ingest: u32,
}

impl Visit {
    #[inline]
    fn encode(row: Payload, start: i64) -> Option<Self> {
        if row.revenue != 0 {
            return None;
        }
        let ingest = u32::try_from(row.ingest / 1000).ok()?;
        let time = if row.time == i64::MIN {
            u32::MAX
        } else {
            let relative = u32::try_from(row.time - start).ok()?;
            if relative == u32::MAX {
                return None;
            }
            relative
        };
        Some(Self {
            person: row.person,
            time,
            ingest,
        })
    }

    #[inline]
    fn expand(self, start: i64, unknown: u32) -> Payload {
        let time = if self.time == u32::MAX {
            i64::MIN
        } else {
            start + i64::from(self.time)
        };
        Payload {
            person: self.person,
            time,
            ingest: i64::from(self.ingest) * 1000,
            revenue: 0,
            category: unknown,
        }
    }
}

struct Part<'a> {
    visits: Vec<(u64, Visit)>,
    detailed: Vec<(u64, Payload)>,
    general: Vec<(&'a [u8], Payload)>,
}

pub(crate) struct EventParts<'a>(Vec<Vec<Part<'a>>>);

// With a shared leading byte, up to eight remaining bytes fit in one word.
// Requiring the last byte to be nonzero makes zero padding unambiguous.
// Other strings retain their exact bytes in a separate table.
#[inline]
fn compact_key(id: &[u8], prefix: u8) -> Option<u64> {
    if id.first() != Some(&prefix) || id.len() < 2 || id.len() > 9 || id.last() == Some(&0) {
        return None;
    }
    let suffix = &id[1..];
    let word = if suffix.len() == 8 {
        u64::from_le_bytes(suffix.try_into().unwrap())
    } else {
        let mut bytes = [0; 8];
        bytes[..suffix.len()].copy_from_slice(suffix);
        u64::from_le_bytes(bytes)
    };
    // Xor shifts and odd multiplication are bijections, so equality of mixed
    // words is exact even when IDs share a hash bucket. Zero stays reserved.
    let mixed = (word ^ (word >> 29)).wrapping_mul(0x9e3779b97f4a7c15);
    Some(mixed ^ (mixed >> 32))
}

fn store_winner(slot: &mut Payload, candidate: Payload) {
    if candidate.ingest < slot.ingest {
        *slot = candidate;
    }
}

fn emit(event: Payload, output: &mut [Vec<PersonEvent>], context: &Context) {
    if event.time != i64::MIN {
        let person = context.people.canonical(event.person);
        output[context.person_shard(person)].push(PersonEvent {
            person,
            ts: event.time,
            revenue: event.revenue,
            category: event.category,
        });
    }
}

fn deduplicate(parts: Vec<Part<'_>>, context: &Context) -> Vec<Vec<PersonEvent>> {
    let count: usize = parts
        .iter()
        .map(|part| part.visits.len() + part.detailed.len())
        .sum();
    // Hash slots hit exactly once identify rows whose full ID is unique in
    // this partition. Every repeated slot must still compare entire keys.
    let bit_count = (count * 8).max(64).next_power_of_two();
    let bit_mask = bit_count - 1;
    let mut seen = vec![0u64; bit_count / 64];
    let mut repeated = vec![0u64; seen.len()];
    for part in &parts {
        let keys = part
            .visits
            .iter()
            .map(|&(key, _)| key)
            .chain(part.detailed.iter().map(|&(key, _)| key));
        for key in keys {
            let index = (key >> 17) as usize & bit_mask;
            let word = index >> 6;
            let bit = 1u64 << (index & 63);
            repeated[word] |= seen[word] & bit;
            seen[word] |= bit;
        }
    }
    let singletons: usize = seen
        .iter()
        .zip(&repeated)
        .map(|(&seen, &repeated)| (seen & !repeated).count_ones() as usize)
        .sum();
    drop(seen);
    let suspects = count - singletons;
    let capacity = (suspects + suspects / 3 + 1).next_power_of_two();
    let mask = capacity - 1;
    let mut keys = vec![0u64; capacity];
    let mut positions = vec![0u32; capacity];
    let mut winners = Vec::with_capacity(suspects);
    let mut general = AHashMap::new();
    let mut output = buckets(context.person_shards);
    for part in parts {
        let visits = part
            .visits
            .into_iter()
            .map(|(key, visit)| (key, visit.expand(context.start, context.dimensions.unknown)));
        for (key, event) in visits.chain(part.detailed) {
            let index = (key >> 17) as usize & bit_mask;
            if repeated[index >> 6] & (1u64 << (index & 63)) == 0 {
                emit(event, &mut output, context);
                continue;
            }
            let mut slot = (key >> 32) as usize & mask;
            loop {
                if keys[slot] == 0 {
                    keys[slot] = key;
                    positions[slot] =
                        u32::try_from(winners.len()).expect("too many events in one partition");
                    winners.push(event);
                    break;
                }
                if keys[slot] == key {
                    store_winner(&mut winners[positions[slot] as usize], event);
                    break;
                }
                slot = (slot + 1) & mask;
            }
        }
        for (id, event) in part.general {
            general
                .entry(id)
                .and_modify(|old| store_winner(old, event))
                .or_insert(event);
        }
    }
    for event in winners.into_iter().chain(general.into_values()) {
        emit(event, &mut output, context);
    }
    output
}

pub(crate) fn scan_events<'a>(input: &'a Inputs, context: &Context) -> EventParts<'a> {
    let chunks = input.chunks();
    let prefix = chunks
        .first()
        .into_iter()
        .flat_map(|chunk| rows::<7>(chunk))
        .find_map(|row| row[0].first().copied())
        .unwrap_or(0);
    let groups: Vec<_> = chunks
        .par_iter()
        .map(|chunk| {
            let mut parts: Vec<_> = (0..context.event_shards)
                .map(|_| Part {
                    visits: Vec::with_capacity(chunk.len() / 78 / context.event_shards),
                    detailed: Vec::new(),
                    general: Vec::new(),
                })
                .collect();
            let mut dates = TimestampParser::new();
            for row in rows::<7>(chunk) {
                let [id, user, ts, ingest, kind, product, price] = row;
                if id.is_empty() {
                    continue;
                }
                let ingest = dates.ingest(ingest);
                let revenue = if kind == b"purchase" {
                    unsigned(price)
                        .filter(|value| (1..=10_000_000).contains(value))
                        .unwrap_or(0) as u32
                } else {
                    0
                };
                let valid = !user.is_empty()
                    && !product.is_empty()
                    && (revenue > 0 || kind == b"view" || kind == b"cart");
                let time = dates
                    .timestamp(ts)
                    .filter(|&time| {
                        valid && time >= context.start && time < context.end && ingest - time <= DAY
                    })
                    .unwrap_or(i64::MIN);
                let category = if revenue > 0 {
                    context.dimensions.category_for(product)
                } else {
                    context.dimensions.unknown
                };
                // Invalid and late rows remain candidates for earliest ingest.
                let person = if time == i64::MIN {
                    0
                } else {
                    context.people.key(user)
                };
                let event = Payload {
                    person,
                    time,
                    ingest,
                    revenue,
                    category,
                };
                let mask = context.event_shards - 1;
                if let Some(key) = compact_key(id, prefix) {
                    let part = &mut parts[key as usize & mask];
                    if let Some(visit) = Visit::encode(event, context.start) {
                        part.visits.push((key, visit));
                    } else {
                        part.detailed.push((key, event));
                    }
                } else {
                    let index = context.hash.hash_one(id) as usize & mask;
                    parts[index].general.push((id, event));
                }
            }
            parts
        })
        .collect();
    let mut shards: Vec<Vec<Part<'_>>> = buckets(context.event_shards);
    for group in groups {
        for (shard, part) in shards.iter_mut().zip(group) {
            if !part.visits.is_empty() || !part.detailed.is_empty() || !part.general.is_empty() {
                shard.push(part);
            }
        }
    }
    EventParts(shards)
}

pub(crate) fn finish_events(raw: EventParts<'_>, context: &Context) -> Vec<Vec<Vec<PersonEvent>>> {
    let result: Vec<_> = raw
        .0
        .into_par_iter()
        .map(|parts| deduplicate(parts, context))
        .collect();
    transpose(result, context.person_shards)
}

pub(crate) fn clicks<'a>(input: &'a Inputs, context: &Context) -> Vec<Vec<Vec<Click<'a>>>> {
    let chunks = input.chunks();
    let groups: Vec<_> = chunks
        .par_iter()
        .map(|chunk| {
            let mut groups: Vec<_> = (0..context.person_shards)
                .map(|_| Vec::with_capacity(chunk.len() / 57 / context.person_shards))
                .collect();
            let mut dates = TimestampParser::new();
            for [id, user, campaign, ts] in rows::<4>(chunk) {
                if id.is_empty() || user.is_empty() {
                    continue;
                }
                let Some(campaign) = context.dimensions.campaign_for(campaign) else {
                    continue;
                };
                let Some(ts) = dates
                    .timestamp(ts)
                    .filter(|&ts| ts >= context.start - WEEK && ts < context.end)
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
            groups
        })
        .collect();
    transpose(groups, context.person_shards)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visits_compact_only_exact_timestamps_and_keep_invalid_winners() {
        let start = 1600000000000i64;
        let event = Payload {
            person: 19,
            time: start + 31,
            ingest: start + 1000,
            revenue: 0,
            category: 7,
        };
        assert_eq!(std::mem::size_of::<(u64, Visit)>(), 24);
        let unpacked = Visit::encode(event, start).unwrap().expand(start, 7);
        assert_eq!((unpacked.time, unpacked.ingest), (event.time, event.ingest));
        for wide in [
            Payload {
                time: start + u32::MAX as i64,
                ..event
            },
            Payload {
                ingest: -1000,
                ..event
            },
            Payload {
                ingest: 4294967296000,
                ..event
            },
            Payload {
                revenue: 1,
                ..event
            },
        ] {
            assert!(Visit::encode(wide, start).is_none());
        }
        let invalid = Visit::encode(
            Payload {
                time: i64::MIN,
                ..event
            },
            start,
        )
        .unwrap();
        assert_eq!(invalid.expand(start, 7).time, i64::MIN);
    }

    #[test]
    fn compact_encoding_is_exact_and_all_other_ids_have_a_fallback() {
        let values: &[&[u8]] = &[
            b"e1",
            b"e01",
            b"e\x001",
            b"e1\x001",
            b"e12345678",
            b"e12345679",
            b"e\xff",
        ];
        let keys: ahash::AHashSet<_> = values
            .iter()
            .map(|value| compact_key(value, b'e').unwrap())
            .collect();
        assert_eq!(keys.len(), values.len());
        assert!(!keys.contains(&0));
        for id in [&b"e"[..], b"e1\0", b"x1234", b"e123456789"] {
            assert!(compact_key(id, b'e').is_none());
        }
    }
}
