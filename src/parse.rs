use memchr::{memchr, memchr_iter};

pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub const SESSION_GAP: i64 = 1_800_000;

/// The challenge permits quotes only around a complete field and never escapes.
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut result = [&[][..]; N];
    let mut start = 0;
    let mut count = 0;
    for end in memchr_iter(b',', line).chain(std::iter::once(line.len())) {
        if count == N {
            return None;
        }
        let mut field = &line[start..end];
        if field.first() == Some(&b'"') {
            if field.len() < 2 || field.last() != Some(&b'"') {
                return None;
            }
            field = &field[1..field.len() - 1];
        }
        if memchr(b'"', field).is_some() {
            return None;
        }
        result[count] = field;
        count += 1;
        start = end + 1;
    }
    (count == N).then_some(result)
}

pub fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = 0;
    memchr_iter(b'\n', bytes)
        .chain((!bytes.is_empty() && bytes.last() != Some(&b'\n')).then_some(bytes.len()))
        .map(move |end| {
            let line = &bytes[start..end];
            start = end + 1;
            line
        })
}

/// Each mask bit classifies the corresponding byte of a complete 64-byte block.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    target_feature = "bmi2"
))]
#[derive(Default)]
struct Structural {
    separators: u64,
    newlines: u64,
    quotes: u64,
}

#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    target_feature = "bmi2"
))]
#[inline]
fn classify(block: &[u8; 64]) -> Structural {
    use std::arch::x86_64::*;
    let mut masks = Structural::default();
    // Both unaligned 32-byte loads are within the array, and the build target
    // provides AVX2. No lookahead load is taken from outside the mapped file.
    unsafe {
        for offset in [0, 32] {
            let input = _mm256_loadu_si256(block.as_ptr().add(offset).cast());
            let matches = |byte| {
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(input, _mm256_set1_epi8(byte))) as u32 as u64
            };
            masks.newlines |= matches(b'\n' as i8) << offset;
            masks.separators |= matches(b',' as i8) << offset;
            masks.quotes |= matches(b'"' as i8) << offset;
        }
    }
    masks.separators |= masks.newlines;
    masks
}

/// Iterate over well-formed rows, preserving byte-exact field values.
pub fn rows<const N: usize>(data: &[u8]) -> FastRows<'_, N> {
    FastRows { data }
}

pub struct FastRows<'a, const N: usize> {
    data: &'a [u8],
}

#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    target_feature = "bmi2"
))]
#[inline]
fn extract(value: u128, mask: u128) -> u64 {
    use std::arch::x86_64::_pext_u64;
    // The build target provides BMI2, and mask has at most 32 set bits.
    unsafe {
        let result = _pext_u64(value as u64, mask as u64);
        result
            | (_pext_u64((value >> 64) as u64, (mask >> 64) as u64) << (mask as u64).count_ones())
    }
}

#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx2",
    target_feature = "bmi2"
))]
#[inline]
fn fast_record<const N: usize>(line: &[u8], sep: u128, quotes: u128) -> Option<[&[u8]; N]> {
    if sep.count_ones() as usize != N {
        return None;
    }
    let open_flags = if quotes == 0 {
        0
    } else {
        let openings = (sep << 1) | 1;
        let closings = sep >> 1;
        // Each quote must be exclusively at the start or end of its field.
        if quotes & !(openings ^ closings) != 0 {
            return None;
        }
        // Extract at separators to align opening and closing bits by field index.
        let open = (quotes as u64 & 1) | (extract(quotes >> 1, sep) << 1);
        let close = extract(quotes << 1, sep);
        if open != close {
            return None;
        }
        open
    };
    let mut sep_lo = sep as u64;
    let mut sep_hi = (sep >> 64) as u64;
    let mut previous = 0;
    let mut result = [&[][..]; N];
    for (index, field) in result.iter_mut().enumerate() {
        let end = if sep_lo != 0 {
            let end = sep_lo.trailing_zeros() as usize;
            sep_lo &= sep_lo - 1;
            end
        } else {
            let end = 64 + sep_hi.trailing_zeros() as usize;
            sep_hi &= sep_hi - 1;
            end
        };
        let trim = (open_flags >> index) as usize & 1;
        *field = &line[previous + trim..end - trim];
        previous = end + 1;
    }
    Some(result)
}

impl<'a, const N: usize> Iterator for FastRows<'a, N> {
    type Item = [&'a [u8]; N];
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while !self.data.is_empty() {
            let data = self.data;
            #[cfg(all(
                target_arch = "x86_64",
                target_feature = "avx2",
                target_feature = "bmi2"
            ))]
            if N < 32 && data.len() >= 128 {
                let a = classify(data[..64].try_into().unwrap());
                let (sep, quotes, len) = if a.newlines == 0 {
                    let b = classify(data[64..128].try_into().unwrap());
                    (
                        (a.separators as u128) | ((b.separators as u128) << 64),
                        (a.quotes as u128) | ((b.quotes as u128) << 64),
                        64 + b.newlines.trailing_zeros() as usize,
                    )
                } else {
                    (
                        a.separators as u128,
                        a.quotes as u128,
                        a.newlines.trailing_zeros() as usize,
                    )
                };
                if len < 128 {
                    let mask = u128::MAX >> (127 - len);
                    self.data = &data[len + 1..];
                    if let Some(row) = fast_record::<N>(&data[..len], sep & mask, quotes & mask) {
                        return Some(row);
                    }
                    continue;
                }
            }
            let (line, next) = match memchr(b'\n', data) {
                Some(end) => (&data[..end], &data[end + 1..]),
                None => (data, &[][..]),
            };
            self.data = next;
            if let Some(row) = fields::<N>(line) {
                return Some(row);
            }
        }
        None
    }
}
pub fn unsigned(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    if bytes.len() <= 18 {
        let (mut value, tail) = if let Some(first) = bytes.first_chunk::<8>() {
            let digits = u64::from_le_bytes(*first) ^ 0x3030_3030_3030_3030;
            if digits & 0xf0f0_f0f0_f0f0_f0f0 != 0
                || digits.wrapping_add(0x0606_0606_0606_0606) & 0x1010_1010_1010_1010 != 0
            {
                return None;
            }
            // Combine eight decimal digits in pairs, then as two 4-digit values.
            let pairs = (digits.wrapping_mul(10) + (digits >> 8)) & 0x00ff_00ff_00ff_00ff;
            let quads = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
            (
                ((quads as u32 as i64) * 10000) + ((quads >> 32) as i64),
                &bytes[8..],
            )
        } else {
            (0, bytes)
        };
        for &byte in tail {
            let digit = byte.wrapping_sub(b'0');
            if digit > 9 {
                return None;
            }
            value = value * 10 + i64::from(digit);
        }
        return Some(value);
    }
    let mut value = 0i64;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((byte - b'0') as i64)?;
    }
    Some(value)
}

fn digits(bytes: &[u8]) -> Option<i64> {
    let mut value = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as i64;
    }
    Some(value)
}

// Gregorian civil date to days since 1970-01-01, including leap centuries.
fn epoch_day(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    era * 146097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719468
}

pub fn timestamp(bytes: &[u8]) -> Option<i64> {
    TimestampParser::new().timestamp(bytes)
}

#[derive(Default, Clone, Copy)]
struct CachedDate {
    year_month: u64,
    day_bytes: u16,
    midnight: i64,
}

/// The same UTC dates occur repeatedly in a chunk. Cache only dates that
/// passed calendar validation, and compare all ten bytes on every hit.
#[derive(Default)]
pub struct TimestampParser {
    dates: [CachedDate; 32],
}

impl TimestampParser {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn midnight(&mut self, bytes: &[u8]) -> Option<i64> {
        let year_month = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        let day_bytes = u16::from_le_bytes([bytes[8], bytes[9]]);
        let day = (bytes[8] & 15) * 10 + (bytes[9] & 15);
        let slot = &mut self.dates[(day & 31) as usize];
        if slot.year_month != year_month || slot.day_bytes != day_bytes {
            if bytes[4] != b'-' || bytes[7] != b'-' {
                return None;
            }
            let year = digits(&bytes[..4])?;
            let month = digits(&bytes[5..7])?;
            let day = digits(&bytes[8..10])?;
            let last = match month {
                2 => 28 + i64::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)),
                4 | 6 | 9 | 11 => 30,
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                _ => return None,
            };
            if year == 0 || day == 0 || day > last {
                return None;
            }
            *slot = CachedDate {
                year_month,
                day_bytes,
                midnight: epoch_day(year, month, day) * DAY,
            };
        }
        Some(slot.midnight)
    }

    #[inline]
    pub fn timestamp(&mut self, bytes: &[u8]) -> Option<i64> {
        if !matches!(bytes.len(), 20 | 25) || bytes[4] != b'-' {
            return unsigned(bytes);
        }
        if bytes[10] != b'T' {
            return None;
        }
        let word = u64::from_le_bytes(bytes[11..19].try_into().unwrap());
        let clock = word ^ u64::from_le_bytes(*b"00:00:00");
        // Require literal separators and six ASCII decimal digits. The high
        // nibble check catches non-ASCII bytes before decimal conversion.
        if clock & 0xf0f0_fff0_f0ff_f0f0 != 0
            || clock.wrapping_add(0x0606_0006_0600_0606) & 0x1010_0010_1000_1010 != 0
        {
            return None;
        }
        let hour = pair(bytes, 11);
        let minute = pair(bytes, 14);
        let second = pair(bytes, 17);
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let offset = if bytes.len() == 20 {
            if bytes[19] != b'Z' {
                return None;
            }
            0
        } else {
            if !matches!(bytes[19], b'+' | b'-') || bytes[22] != b':' {
                return None;
            }
            let hours = digits(&bytes[20..22])?;
            let minutes = digits(&bytes[23..25])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            (hours * 60 + minutes) * if bytes[19] == b'+' { 60 } else { -60 }
        };
        Some(self.midnight(bytes)? + (hour * 3600 + minute * 60 + second - offset) * 1000)
    }

    /// The input contract guarantees that ingest_ts has valid UTC F1 syntax.
    #[inline]
    pub fn ingest(&mut self, bytes: &[u8]) -> i64 {
        let bytes: &[u8; 20] = bytes.try_into().expect("ingest_ts must be valid F1");
        self.midnight(bytes)
            .expect("ingest_ts must have a calendar date")
            + (pair(bytes, 11) * 3600 + pair(bytes, 14) * 60 + pair(bytes, 17)) * 1000
    }
}

#[inline]
fn pair(bytes: &[u8], start: usize) -> i64 {
    i64::from(bytes[start] & 15) * 10 + i64::from(bytes[start + 1] & 15)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_dialect_keeps_spaces_and_carriage_returns() {
        assert_eq!(
            fields::<3>(b"\" u1\",\"\",1000\r"),
            Some([&b" u1"[..], &b""[..], &b"1000\r"[..]])
        );
        for row in [
            &b"u1,\"bad\"tail,p1"[..],
            b"u1,ba\"d,p1",
            b"u1,p1",
            b"u1,p1,2,3",
            b"",
        ] {
            assert!(fields::<3>(row).is_none());
        }
    }

    #[test]
    fn timestamps_are_strict_and_offsets_preserve_milliseconds() {
        assert_eq!(timestamp(b"1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            timestamp(b"2026-10-03T19:58:10-04:00"),
            timestamp(b"2026-10-03T23:58:10Z")
        );
        assert_eq!(timestamp(b"00000000000000086400000"), Some(DAY));
        assert_eq!(
            timestamp(b"2028-02-29T00:00:00-00:00"),
            timestamp(b"2028-02-29T00:00:00Z")
        );
        for value in [
            &b"2026-02-29T00:00:00Z"[..],
            b"2028-02-30T00:00:00Z",
            b"2028-01-01T24:00:00Z",
            b"2026-01-01T00:00:00+24:00",
            b"2026-01-01T00:00:00+05:60",
            b"2026-01-01T00:00:00z",
            b"1791071890000.0",
            b"99999999999999999999999999999999",
            b"2026-01-01T00:00:00Z\r",
        ] {
            assert!(timestamp(value).is_none(), "{:?}", value);
        }
    }

    #[test]
    fn block_csv_agrees_with_field_parser_at_all_alignments() {
        let cases: &[&[u8]] = &[
            b"a,b,c",
            b"aa,\"b\",c",
            b"\"\",,\"\"",
            b"\"\",,",
            b"aa,b\",c",
            b"aa,\",c",
            b",\"b\"z,c",
            b"\"a\"x,b,c",
            b"aa,b",
            b"aa,b,c,d",
            b"aa,b,c\r",
            b"\r,\" \",",
            b"a,,\r",
            b"\"\",a,b,c",
            b"",
        ];
        for prefix in 0..130 {
            let mut bytes = vec![b'x'; prefix];
            bytes.extend_from_slice(b",a,b\n");
            for &case in cases {
                bytes.extend_from_slice(case);
                bytes.push(b'\n');
            }
            bytes.extend_from_slice(b"\",");
            bytes.extend_from_slice(&[b'x'; 257]);
            bytes.extend_from_slice(b",y\n");
            bytes.extend_from_slice(&[b'z'; 257]);
            bytes.extend_from_slice(b",\":\",");
            for terminate in [false, true] {
                if terminate {
                    bytes.push(b'\n');
                }
                assert_eq!(
                    rows::<3>(&bytes).collect::<Vec<_>>(),
                    lines(&bytes).filter_map(fields::<3>).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn epoch_decimal_and_cached_calendar_boundaries() {
        let mut parser = TimestampParser::new();
        for n in [0, 1, 99999999, 1234567890123, 999999999999999999, i64::MAX] {
            for width in [1, 13, 20, 25, 100] {
                let text = format!("{n:0width$}");
                assert_eq!(parser.timestamp(text.as_bytes()), Some(n));
            }
        }
        assert_eq!(parser.ingest(b"2000-02-29T01:02:03Z"), 951786123000);
        assert!(parser.timestamp(b"1900-02-29T01:02:03Z").is_none());
        assert_eq!(
            parser.timestamp(b"2000-02-29T01:02:03Z"),
            Some(951786123000)
        );
        for offset in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
            for bad in [0, b'/', b':', b'@', 0x80, 0xff] {
                let mut time = *b"2000-02-29T01:02:03Z";
                time[offset] = bad;
                assert_eq!(parser.timestamp(&time), None);
            }
        }
    }
}
