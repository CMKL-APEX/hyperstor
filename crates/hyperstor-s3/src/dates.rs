//! The three ways S3 writes a time, all UTC: `20130524T000000Z` (SigV4),
//! `Fri, 24 May 2013 00:00:00 GMT` (HTTP headers, RFC 7231) and
//! `2013-05-24T00:00:00.000Z` (XML). Days to dates by Howard Hinnant's
//! `civil_from_days`, which holds for every year here.

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

struct Parts {
    y: i64,
    mo: u32,
    d: u32,
    h: u64,
    mi: u64,
    s: u64,
}

fn parts(secs: u64) -> (Parts, i64) {
    let days = (secs / 86400) as i64;
    let (y, mo, d) = civil_from_days(days);
    let t = secs % 86400;
    (
        Parts {
            y,
            mo,
            d,
            h: t / 3600,
            mi: t / 60 % 60,
            s: t % 60,
        },
        days,
    )
}

fn secs_of(y: i64, mo: u32, d: u32, h: u64, mi: u64, s: u64) -> Option<u64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let days = u64::try_from(days_from_civil(y, mo, d)).ok()?;
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `yyyymmddThhmmssZ` as Unix seconds. ASCII only: the length is in
/// bytes and the fields are sliced by byte, so anything else is refused
/// before slicing.
pub fn parse_amz(s: &str) -> Option<u64> {
    if s.len() != 16 || !s.is_ascii() || s.as_bytes()[8] != b'T' || !s.ends_with('Z') {
        return None;
    }
    secs_of(
        num(&s[0..4])?,
        num(&s[4..6])?,
        num(&s[6..8])?,
        num(&s[9..11])?,
        num(&s[11..13])?,
        num(&s[13..15])?,
    )
}

pub fn amz(secs: u64) -> String {
    let (p, _) = parts(secs);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        p.y, p.mo, p.d, p.h, p.mi, p.s
    )
}

/// RFC 7231's IMF-fixdate as Unix seconds; or RFC 2822's, in UTC, as
/// Python's `email.utils.formatdate` (and so botocore) writes it.
pub fn parse_http(s: &str) -> Option<u64> {
    let f: Vec<&str> = s.split_whitespace().collect();
    let [_, d, mo, y, t, "GMT" | "-0000" | "+0000"] = f[..] else {
        return None;
    };
    let mo = MONTHS.iter().position(|m| *m == mo)? as u32 + 1;
    let t: Vec<&str> = t.split(':').collect();
    let [h, mi, sec] = t[..] else {
        return None;
    };
    secs_of(num(y)?, mo, num(d)?, num(h)?, num(mi)?, num(sec)?)
}

pub fn http(secs: u64) -> String {
    let (p, days) = parts(secs);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        p.d,
        MONTHS[p.mo as usize - 1],
        p.y,
        p.h,
        p.mi,
        p.s
    )
}

/// ISO 8601 with milliseconds, from Unix milliseconds.
pub fn iso(ms: u64) -> String {
    let (p, _) = parts(ms / 1000);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        p.y,
        p.mo,
        p.d,
        p.h,
        p.mi,
        p.s,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_and_parse() {
        // 2013-05-24T00:00:00Z, a Friday (AWS's examples).
        let t = 1_369_353_600;
        assert_eq!(parse_amz("20130524T000000Z"), Some(t));
        assert_eq!(amz(t), "20130524T000000Z");
        assert_eq!(http(t), "Fri, 24 May 2013 00:00:00 GMT");
        assert_eq!(parse_http("Fri, 24 May 2013 00:00:00 GMT"), Some(t));
        assert_eq!(parse_http("Fri, 24 May 2013 00:00:00 -0000"), Some(t));
        assert_eq!(parse_http("Fri, 24 May 2013 00:00:00 +0700"), None);
        assert_eq!(iso(t * 1000 + 7), "2013-05-24T00:00:00.007Z");
        assert_eq!(http(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // Leap days, and every day for 200 years both ways.
        assert_eq!(amz(951_782_400), "20000229T000000Z");
        for d in 0..73_000u64 {
            let s = d * 86400 + 3723;
            assert_eq!(parse_amz(&amz(s)), Some(s));
            assert_eq!(parse_http(&http(s)), Some(s));
        }
        for bad in [
            "20131324T000000Z",
            "2013052T0000000Z",
            "20130524T000000",
            "+0130524T000000Z",
            // 16 bytes with a 'T' at byte 8, but a character of three
            // bytes across the year's and month's fields.
            "201\u{20ac}05T000000Z",
            "20130\u{e9}4T000000Z",
        ] {
            assert_eq!(parse_amz(bad), None, "{bad}");
        }
    }
}
