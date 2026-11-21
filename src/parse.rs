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
    // split_terminator semantics preserve CR bytes and interior empty lines.
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.split(|&byte| byte == b'\n')
}

pub fn unsigned(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
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
    if bytes.len() != 20 && bytes.len() != 25 {
        return unsigned(bytes);
    }
    // A 20/25-digit F3 value is still an epoch value, including leading zeros.
    if bytes[4] != b'-' {
        return unsigned(bytes);
    }
    if bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year = digits(&bytes[..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    if year == 0 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > days {
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
        (hours * 60 + minutes) * 60 * if bytes[19] == b'+' { 1 } else { -1 }
    };
    Some(epoch_day(year, month, day) * DAY + (hour * 3600 + minute * 60 + second - offset) * 1000)
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
}
