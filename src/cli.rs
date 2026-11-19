//! Hand-rolled argument parsing and human units (`7d`, `2h`, `300M`). Owned by wave-1 lane `cli`.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parsed command line: positionals plus `--flag` / `--name value` options.
#[derive(Debug, Default, Clone)]
pub struct Args {
    positionals: Vec<String>,
    bools: Vec<String>,
    values: HashMap<String, Vec<String>>,
}

/// A plain decimal number: optional leading `-`, digits, at most one `.`; no exponent/inf/nan.
fn plain_number(s: &str) -> Option<f64> {
    let body = s.strip_prefix('-').unwrap_or(s);
    let mut dots = 0;
    let mut digits = 0;
    for c in body.chars() {
        match c {
            '0'..='9' => digits += 1,
            '.' => dots += 1,
            _ => return None,
        }
    }
    if digits == 0 || dots > 1 {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

impl Args {
    pub fn parse(
        raw: impl IntoIterator<Item = String>,
        value_flags: &[&str],
        bool_flags: &[&str],
    ) -> Result<Args, String> {
        let mut a = Args::default();
        let mut it = raw.into_iter();
        while let Some(tok) = it.next() {
            if tok == "--" {
                a.positionals.extend(it.by_ref());
                break;
            }
            // "-" alone and negative numbers ("-1", "-0.5") are positionals.
            if !tok.starts_with('-') || tok == "-" || plain_number(&tok).is_some() {
                a.positionals.push(tok);
                continue;
            }
            // A bundle of short flags: `-in`, `-ilw`, and `-inm5` where the last one takes a
            // value. Anything with a value flag not at the end is rejected by the expansion.
            if !tok.starts_with("--") && tok.len() > 2 && !value_flags.contains(&&tok[..2]) {
                let mut rest = tok.as_str();
                while let Some(head) = rest.get(..2) {
                    let short = &rest[1..2];
                    let flag = format!("-{short}");
                    if bool_flags.contains(&flag.as_str()) {
                        if !a.bools.contains(&flag) {
                            a.bools.push(flag);
                        }
                        rest = &rest[2..];
                        if rest.is_empty() {
                            break;
                        }
                        continue;
                    }
                    if value_flags.contains(&flag.as_str()) {
                        let v = match rest.get(2..) {
                            Some(v) if !v.is_empty() => v.to_string(),
                            _ => it
                                .next()
                                .ok_or_else(|| format!("option {flag} needs a value"))?,
                        };
                        a.values.entry(flag).or_default().push(v);
                        rest = "";
                        break;
                    }
                    return Err(format!("unknown option: -{short} (in {head}...)"));
                }
                continue;
            }
            let (name, inline) = match tok.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                // An attached short value: `-C2`, `-m5`, `-g*.rs`, `-tjs`.
                _ if !tok.starts_with("--") && tok.len() > 2 => {
                    let head = &tok[..2];
                    if value_flags.contains(&head) {
                        (head.to_string(), Some(tok[2..].to_string()))
                    } else {
                        (tok.clone(), None)
                    }
                }
                _ => (tok.clone(), None),
            };
            if value_flags.contains(&name.as_str()) {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .ok_or_else(|| format!("option {name} needs a value"))?,
                };
                a.values.entry(name).or_default().push(v);
            } else if bool_flags.contains(&name.as_str()) {
                if inline.is_some() {
                    return Err(format!("option {name} does not take a value"));
                }
                if !a.bools.contains(&name) {
                    a.bools.push(name);
                }
            } else {
                return Err(format!("unknown option: {tok}"));
            }
        }
        Ok(a)
    }

    pub fn positional(&self, i: usize) -> Option<&str> {
        self.positionals.get(i).map(String::as_str)
    }

    pub fn positionals(&self) -> &[String] {
        &self.positionals
    }

    /// Whether the boolean flag (spelled with its dashes, e.g. `--dry`) was given.
    pub fn flag(&self, name: &str) -> bool {
        self.bools.iter().any(|b| b == name)
    }

    /// The value of a value flag (`--age 7d` or `--age=7d`); the last occurrence wins.
    /// The last value given for `name` (repeatable options keep their history for
    /// [`Args::values`]).
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    /// Every value given for `name`, in order: `-g a -g b` yields both, as rg does.
    pub fn values(&self, name: &str) -> &[String] {
        self.values.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Parse a byte size: plain integer, or number with K/M/G/T and optional `B`/`iB`
/// (case-insensitive, fractions allowed, binary multiples: 1K = 1024).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    if num.is_empty() {
        return Err(format!("invalid size: {s:?}"));
    }
    let mult: u64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("invalid size unit in {s:?} (use K, M, G, T)")),
    };
    let too_big = || format!("size too large: {s:?}");
    if !num.contains('.') {
        let n: u64 = num.parse().map_err(|_| too_big())?;
        return n.checked_mul(mult).ok_or_else(too_big);
    }
    if num.matches('.').count() > 1 || num == "." {
        return Err(format!("invalid size: {s:?}"));
    }
    let v: f64 = num.parse().map_err(|_| format!("invalid size: {s:?}"))?;
    let bytes = v * mult as f64;
    if !bytes.is_finite() || bytes >= u64::MAX as f64 {
        return Err(too_big());
    }
    Ok(bytes as u64)
}

/// Parse an age. `30s`, `15m`, `2h`, `7d`, `1w`; a bare number is days (`0.0834`).
/// Any negative bare number means "no age filter" (`Ok(None)`); anything else is an error.
pub fn parse_age(s: &str) -> Result<Option<Duration>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty age".into());
    }
    let bad = || format!("invalid age {s:?} (use e.g. 30s, 15m, 2h, 7d, 1w, or a number of days)");
    if let Some(n) = plain_number(t) {
        if n < 0.0 {
            return Ok(None);
        }
        return secs(n * 86400.0, s).map(Some);
    }
    let unit = t.chars().last().ok_or_else(bad)?;
    let per = match unit.to_ascii_lowercase() {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        'w' => 604800.0,
        _ => return Err(bad()),
    };
    let num = &t[..t.len() - 1];
    match plain_number(num) {
        Some(n) if n >= 0.0 && !num.starts_with('-') => secs(n * per, s).map(Some),
        _ => Err(bad()),
    }
}

/// Parse an absolute date, the way `find -newermt` takes one: `2030-01-31`,
/// `2030-01-31 08:30`, `2030-01-31T08:30:15`, or `@1700000000` for a Unix timestamp. Times
/// are read as UTC, since a local-zone library is a dependency this tool does not have.
pub fn parse_date(s: &str) -> Result<SystemTime, String> {
    let t = s.trim();
    let bad = || format!("invalid date {s:?} (use YYYY-MM-DD, YYYY-MM-DD HH:MM[:SS], or @SECONDS)");
    if let Some(rest) = t.strip_prefix('@') {
        let secs: u64 = rest.trim().parse().map_err(|_| bad())?;
        return Ok(UNIX_EPOCH + Duration::from_secs(secs));
    }
    let date = t.split([' ', 'T']).next().unwrap_or("");
    let mut parts = date.split('-');
    let year: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let time = t[date.len()..].trim_start_matches([' ', 'T']);
    let (mut hh, mut mm, mut ss) = (0i64, 0i64, 0i64);
    if !time.is_empty() {
        let mut tparts = time.split(':');
        hh = tparts
            .next()
            .ok_or_else(bad)?
            .trim()
            .parse()
            .map_err(|_| bad())?;
        mm = tparts
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| bad())?;
        ss = tparts
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| bad())?;
        if !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..61).contains(&ss) {
            return Err(bad());
        }
    }
    let secs = days_from_civil(year, month, day) * 86_400 + hh * 3600 + mm * 60 + ss;
    Ok(if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs((-secs) as u64)
    })
}

/// Days from 1970-01-01 to `y-m-d`, by the civil-from-days inverse (no leap-year tables).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Format an age the way `parse_age` accepts it: the largest whole unit, then the rest.
pub fn format_age(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = match s {
        n if n % 604_800 == 0 && n > 0 => (n / 604_800, "w"),
        n if n % 86_400 == 0 && n > 0 => (n / 86_400, "d"),
        n if n % 3_600 == 0 && n > 0 => (n / 3_600, "h"),
        n if n % 60 == 0 && n > 0 => (n / 60, "m"),
        n => (n, "s"),
    };
    format!("{n}{unit}")
}

fn secs(v: f64, orig: &str) -> Result<Duration, String> {
    Duration::try_from_secs_f64(v).map_err(|_| format!("age too large: {orig:?}"))
}

pub fn parse_threads(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "invalid thread count {s:?} (must be an integer >= 1)"
        )),
    }
}

/// One worker per core, never fewer than four: the scan is syscall-bound, so a few extra
/// threads help hide latency while heavy oversubscription (4x cores) measurably hurts.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(4, 16))
        .unwrap_or(8)
}

/// Decimal units, e.g. `1.23 GB`.
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if b < 1000 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 999.995 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{v:.2} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn legacy_dusage_positionals() {
        let a = Args::parse(s(&["C:\\", "314572800", "96", "100"]), &[], &[]).unwrap();
        assert_eq!(a.positionals(), &s(&["C:\\", "314572800", "96", "100"])[..]);
        assert_eq!(a.positional(1), Some("314572800"));
        assert_eq!(a.positional(4), None);
    }

    #[test]
    fn legacy_fastdel_negative_and_flag() {
        let a = Args::parse(
            s(&[
                "C:\\devcache",
                "-1",
                "build-target,cargo-target",
                "64",
                "--dry",
            ]),
            &["--age"],
            &["--dry", "--help", "-h"],
        )
        .unwrap();
        assert_eq!(a.positional(1), Some("-1"));
        assert_eq!(a.positional(3), Some("64"));
        assert!(a.flag("--dry"));
        assert!(!a.flag("--help"));
        let b = Args::parse(s(&["X", "0.0834", ".session-markers", "96"]), &[], &[]).unwrap();
        assert_eq!(b.positional(1), Some("0.0834"));
        let c = Args::parse(s(&["X", "-0.5", "-.5"]), &[], &[]).unwrap();
        assert_eq!(c.positionals().len(), 3);
    }

    #[test]
    fn value_flags_both_forms() {
        let a = Args::parse(
            s(&["--age", "7d", "x", "--top=5", "--dry"]),
            &["--age", "--top"],
            &["--dry"],
        )
        .unwrap();
        assert_eq!(a.value("--age"), Some("7d"));
        assert_eq!(a.value("--top"), Some("5"));
        assert_eq!(a.value("--nope"), None);
        assert_eq!(a.positionals(), &s(&["x"])[..]);
        // A value flag consumes the next token even if it looks like a flag or negative number.
        let b = Args::parse(s(&["--age", "-1"]), &["--age"], &[]).unwrap();
        assert_eq!(b.value("--age"), Some("-1"));
        let c = Args::parse(s(&["--age=", "x"]), &["--age"], &[]).unwrap();
        assert_eq!(c.value("--age"), Some(""));
    }

    #[test]
    fn errors() {
        assert!(Args::parse(s(&["--bogus"]), &[], &[]).is_err());
        assert!(Args::parse(s(&["-x"]), &[], &["--dry"]).is_err());
        assert!(Args::parse(s(&["--age"]), &["--age"], &[]).is_err());
        assert!(Args::parse(s(&["--dry=1"]), &[], &["--dry"]).is_err());
        assert!(Args::parse(s(&["--=x"]), &[], &[]).is_err());
    }

    #[test]
    fn double_dash_ends_flags() {
        let a = Args::parse(s(&["--dry", "--", "--weird", "-h", "--"]), &[], &["--dry"]).unwrap();
        assert!(a.flag("--dry"));
        assert_eq!(a.positionals(), &s(&["--weird", "-h", "--"])[..]);
    }

    #[test]
    fn short_help_and_lone_dash() {
        let a = Args::parse(s(&["-h", "-"]), &[], &["-h", "--help"]).unwrap();
        assert!(a.flag("-h"));
        assert_eq!(a.positionals(), &s(&["-"])[..]);
    }

    #[test]
    fn size_ok() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1073741824"), Ok(1 << 30));
        assert_eq!(parse_size("314572800"), Ok(300 << 20));
        assert_eq!(parse_size("300M"), Ok(300 << 20));
        assert_eq!(parse_size("300m"), Ok(300 << 20));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("512KiB"), Ok(512 << 10));
        assert_eq!(parse_size("512kib"), Ok(512 << 10));
        assert_eq!(parse_size("2T"), Ok(2 << 40));
        assert_eq!(parse_size("2tb"), Ok(2 << 40));
        assert_eq!(parse_size("10B"), Ok(10));
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("0.5K"), Ok(512));
        assert_eq!(parse_size(" 4g "), Ok(4 << 30));
        assert_eq!(parse_size("1GiB"), Ok(1 << 30));
    }

    #[test]
    fn size_err() {
        for bad in [
            "",
            "abc",
            "M",
            "-5",
            "1.2.3",
            ".",
            "5X",
            "1 G",
            "1e3",
            "99999999999999999999",
            "20000000T",
            "1.5.",
            "inf",
            "nan",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn age_units() {
        let d = |x: u64| Ok(Some(Duration::from_secs(x)));
        assert_eq!(parse_age("30s"), d(30));
        assert_eq!(parse_age("15m"), d(900));
        assert_eq!(parse_age("2h"), d(7200));
        assert_eq!(parse_age("7d"), d(7 * 86400));
        assert_eq!(parse_age("1w"), d(604800));
        assert_eq!(parse_age("1W"), d(604800));
        assert_eq!(parse_age("0"), d(0));
        assert_eq!(parse_age("0d"), d(0));
        assert_eq!(parse_age("1.5h"), d(5400));
    }

    #[test]
    fn age_bare_days_and_negative() {
        assert_eq!(parse_age("1"), Ok(Some(Duration::from_secs(86400))));
        assert_eq!(parse_age("96"), Ok(Some(Duration::from_secs(96 * 86400))));
        let got = parse_age("0.0834").unwrap().unwrap();
        assert!((got.as_secs_f64() - 0.0834 * 86400.0).abs() < 1e-3);
        assert_eq!(parse_age("-1"), Ok(None));
        assert_eq!(parse_age("-0.5"), Ok(None));
        assert_eq!(parse_age("-100"), Ok(None));
    }

    #[test]
    fn age_garbage_is_error() {
        for bad in [
            "",
            " ",
            "7x",
            "abc",
            "d",
            "-7d",
            "7dd",
            "1.2.3d",
            "nan",
            "inf",
            "1e3",
            "--1",
            "h7",
            "99999999999999999999999d",
        ] {
            assert!(parse_age(bad).is_err(), "{bad:?} should be an error");
        }
    }

    #[test]
    fn threads() {
        assert_eq!(parse_threads("1"), Ok(1));
        assert_eq!(parse_threads("96"), Ok(96));
        for bad in ["0", "-1", "", "x", "1.5"] {
            assert!(parse_threads(bad).is_err(), "{bad:?}");
        }
        assert!(default_threads() >= 4);
    }

    #[test]
    fn bytes_format() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1000), "1.00 KB");
        assert_eq!(fmt_bytes(1_230_000_000), "1.23 GB");
        assert_eq!(fmt_bytes(314_572_800), "314.57 MB");
        assert_eq!(fmt_bytes(999_999_999), "1.00 GB");
        assert_eq!(fmt_bytes(u64::MAX), "18446.74 PB");
    }
}
