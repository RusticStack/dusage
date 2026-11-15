//! Text and JSON rendering of a `ScanResult`. Owned by wave-1 lane `report`.
//!
//! Formatting helpers here are deliberately independent of `cli.rs`.

use crate::model::{ScanResult, Totals};
use std::fmt::Write as _;
use std::io::{self, Write};

pub struct ReportOptions {
    pub top_n: usize,
    pub min_big: u64,
    pub show_alloc: bool,
    pub by_ext: bool,
}

/// Decimal gigabytes with two decimals, e.g. `1.23`.
fn gb(b: u64) -> String {
    format!("{:.2}", b as f64 / 1e9)
}

fn bytes_col(bytes: u64, alloc: u64, show_alloc: bool) -> String {
    if show_alloc {
        format!("{:>10} GB  {:>10} GB on disk", gb(bytes), gb(alloc))
    } else {
        format!("{:>10} GB", gb(bytes))
    }
}

fn rate(files: u64, secs: f64) -> f64 {
    files as f64 / secs.max(1e-6)
}

pub fn render_text(r: &ScanResult, o: &ReportOptions, out: &mut dyn Write) -> io::Result<()> {
    let secs = r.elapsed.as_secs_f64();
    let alloc = if o.show_alloc {
        format!(" | on disk {} GB", gb(r.total.alloc))
    } else {
        String::new()
    };
    writeln!(
        out,
        "\n=== TOTAL: {} GB{} | files {} | dirs {} | elapsed {:?} ({:.0} files/s) | source {} ===",
        gb(r.total.bytes),
        alloc,
        r.total.files,
        r.total.dirs,
        r.elapsed,
        rate(r.total.files, secs),
        r.source.label()
    )?;

    writeln!(out, "\n--- top {} entries under {} ---", o.top_n, r.root)?;
    for t in r.tops.iter().take(o.top_n) {
        let slash = if t.is_dir { "\\" } else { "" };
        writeln!(
            out,
            "{}  {}{}",
            bytes_col(t.totals.bytes, t.totals.alloc, o.show_alloc),
            t.name,
            slash
        )?;
    }

    writeln!(
        out,
        "\n--- files >= {} GB (top {}) ---",
        gb(o.min_big),
        o.top_n
    )?;
    for f in r.big_files.iter().take(o.top_n) {
        writeln!(
            out,
            "{}  {}",
            bytes_col(f.bytes, f.alloc, o.show_alloc),
            f.path
        )?;
    }

    if o.by_ext {
        writeln!(out, "\n--- by extension (top {}) ---", o.top_n)?;
        for (ext, t) in r.by_ext.iter().take(o.top_n) {
            let label = if ext.is_empty() {
                "(none)".to_string()
            } else {
                format!(".{ext}")
            };
            writeln!(
                out,
                "{}  {:>10} files  {}",
                bytes_col(t.bytes, t.alloc, o.show_alloc),
                t.files,
                label
            )?;
        }
    }

    let mut foot: Vec<String> = Vec::new();
    if r.skipped_reparse > 0 {
        foot.push(format!(
            "skipped {} reparse points (not followed)",
            r.skipped_reparse
        ));
    }
    if r.hardlink_dupes > 0 {
        foot.push(format!(
            "{} hardlink duplicates not double-counted ({} GB)",
            r.hardlink_dupes,
            gb(r.hardlink_dupe_bytes)
        ));
    }
    if r.stream_bytes > 0 {
        foot.push(format!(
            "{} GB in named alternate data streams",
            gb(r.stream_bytes)
        ));
    }
    let e = &r.errors;
    let cats = [
        ("access_denied", e.access_denied),
        ("sharing_violation", e.sharing_violation),
        ("not_found", e.not_found),
        ("dir_not_empty", e.dir_not_empty),
        ("other", e.other),
    ];
    if e.total() > 0 {
        let parts: Vec<String> = cats
            .iter()
            .filter(|c| c.1 > 0)
            .map(|c| format!("{} {}", c.0, c.1))
            .collect();
        foot.push(format!("errors: {} ({})", e.total(), parts.join(", ")));
    }
    if !foot.is_empty() {
        writeln!(out)?;
        for l in foot {
            writeln!(out, "note: {l}")?;
        }
    }
    Ok(())
}

/// Append `s` to `out` as the *contents* of a JSON string (no surrounding quotes).
pub fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

fn jstr(s: &str, out: &mut String) {
    out.push('"');
    json_escape(s, out);
    out.push('"');
}

fn jtotals(t: &Totals, out: &mut String) {
    let _ = write!(
        out,
        "\"bytes\":{},\"alloc\":{},\"files\":{},\"dirs\":{}",
        t.bytes, t.alloc, t.files, t.dirs
    );
}

/// JSON number for an f64; non-finite values (never expected) become 0.
fn jf64(v: f64) -> String {
    if v.is_finite() {
        let s = format!("{v}");
        if s.contains(['.', 'e', 'E']) {
            s
        } else {
            format!("{s}.0")
        }
    } else {
        "0.0".to_string()
    }
}

pub fn render_json(r: &ScanResult, out: &mut dyn Write) -> io::Result<()> {
    let mut s = String::new();
    s.push_str("{\"root\":");
    jstr(&r.root, &mut s);
    s.push_str(",\"source\":");
    jstr(r.source.label(), &mut s);
    let _ = write!(
        s,
        ",\"threads\":{},\"elapsed_secs\":{}",
        r.threads,
        jf64(r.elapsed.as_secs_f64())
    );
    s.push_str(",\"total\":{");
    jtotals(&r.total, &mut s);
    s.push('}');
    let _ = write!(
        s,
        ",\"skipped_reparse\":{},\"hardlink_dupes\":{},\"hardlink_dupe_bytes\":{},\"stream_bytes\":{}",
        r.skipped_reparse, r.hardlink_dupes, r.hardlink_dupe_bytes, r.stream_bytes
    );
    let e = &r.errors;
    let _ = write!(
        s,
        ",\"errors\":{{\"access_denied\":{},\"sharing_violation\":{},\"not_found\":{},\"dir_not_empty\":{},\"other\":{}}}",
        e.access_denied, e.sharing_violation, e.not_found, e.dir_not_empty, e.other
    );
    s.push_str(",\"tops\":[");
    for (i, t) in r.tops.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        jstr(&t.name, &mut s);
        let _ = write!(s, ",\"is_dir\":{},", t.is_dir);
        jtotals(&t.totals, &mut s);
        s.push('}');
    }
    s.push_str("],\"big_files\":[");
    for (i, f) in r.big_files.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"path\":");
        jstr(&f.path, &mut s);
        let _ = write!(s, ",\"bytes\":{},\"alloc\":{}}}", f.bytes, f.alloc);
    }
    s.push_str("],\"by_ext\":[");
    for (i, (ext, t)) in r.by_ext.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"ext\":");
        jstr(ext, &mut s);
        s.push(',');
        jtotals(t, &mut s);
        s.push('}');
    }
    s.push_str("]}\n");
    out.write_all(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BigFile, ErrorCounts, Source, TopEntry};
    use std::time::Duration;

    fn t(bytes: u64, alloc: u64, files: u64, dirs: u64) -> Totals {
        Totals {
            bytes,
            alloc,
            files,
            dirs,
        }
    }

    fn sample() -> ScanResult {
        ScanResult {
            root: "C:\\data".into(),
            tops: vec![
                TopEntry {
                    name: "big".into(),
                    is_dir: true,
                    totals: t(3_000_000_000, 3_100_000_000, 10, 2),
                },
                TopEntry {
                    name: "f.bin".into(),
                    is_dir: false,
                    totals: t(500_000_000, 500_000_000, 1, 0),
                },
            ],
            big_files: vec![BigFile {
                path: "C:\\data\\big\\x.iso".into(),
                bytes: 2_000_000_000,
                alloc: 2_000_004_096,
            }],
            by_ext: vec![
                ("iso".into(), t(2_000_000_000, 2_000_004_096, 1, 0)),
                (String::new(), t(5, 4096, 2, 0)),
            ],
            total: t(3_500_000_000, 3_600_000_000, 11, 2),
            elapsed: Duration::from_secs(2),
            threads: 8,
            source: Source::Batched,
            ..Default::default()
        }
    }

    fn opts(alloc: bool, ext: bool) -> ReportOptions {
        ReportOptions {
            top_n: 5,
            min_big: 1_000_000_000,
            show_alloc: alloc,
            by_ext: ext,
        }
    }

    fn text(r: &ScanResult, o: &ReportOptions) -> String {
        let mut v = Vec::new();
        render_text(r, o, &mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn golden_plain() {
        let expect = [
            "",
            "=== TOTAL: 3.50 GB | files 11 | dirs 2 | elapsed 2s (6 files/s) | source batched ===",
            "",
            r"--- top 5 entries under C:\data ---",
            r"      3.00 GB  big\",
            "      0.50 GB  f.bin",
            "",
            "--- files >= 1.00 GB (top 5) ---",
            r"      2.00 GB  C:\data\big\x.iso",
            "",
        ]
        .join("\n");
        assert_eq!(text(&sample(), &opts(false, false)), expect);
    }

    #[test]
    fn golden_alloc_and_ext() {
        let s = text(&sample(), &opts(true, true));
        assert!(s.contains("| on disk 3.60 GB |"));
        assert!(s.contains("      3.00 GB        3.10 GB on disk  big\\\n"));
        assert!(s.contains("\n--- by extension (top 5) ---\n"));
        assert!(s.contains("  .iso\n"));
        assert!(s.contains("  (none)\n"));
        let plain = text(&sample(), &opts(false, false));
        assert!(!plain.contains("on disk") && !plain.contains("by extension"));
    }

    #[test]
    fn footer_only_nonzero() {
        assert!(!text(&sample(), &opts(false, false)).contains("note:"));
        let mut r = sample();
        r.skipped_reparse = 3;
        r.hardlink_dupes = 4;
        r.hardlink_dupe_bytes = 2_500_000_000;
        r.stream_bytes = 1_000_000_000;
        r.errors = ErrorCounts {
            access_denied: 2,
            other: 1,
            ..Default::default()
        };
        let s = text(&r, &opts(false, false));
        assert!(s.contains("note: skipped 3 reparse points (not followed)\n"));
        assert!(s.contains("note: 4 hardlink duplicates not double-counted (2.50 GB)\n"));
        assert!(s.contains("note: 1.00 GB in named alternate data streams\n"));
        assert!(s.contains("note: errors: 3 (access_denied 2, other 1)\n"));
        let mut r2 = sample();
        r2.skipped_reparse = 1;
        let s2 = text(&r2, &opts(false, false));
        assert!(s2.contains("reparse") && !s2.contains("hardlink") && !s2.contains("errors"));
    }

    #[test]
    fn top_n_truncates() {
        let mut o = opts(false, false);
        o.top_n = 1;
        let s = text(&sample(), &o);
        assert!(s.contains("big\\") && !s.contains("f.bin"));
    }

    #[test]
    fn escape_cases() {
        let mut o = String::new();
        json_escape("a\"b\\c\nd\re\tf\u{1}g\u{1f}\u{8}\u{c}", &mut o);
        assert_eq!(o, "a\\\"b\\\\c\\nd\\re\\tf\\u0001g\\u001f\\b\\f");
        let mut o = String::new();
        json_escape("é😀日本", &mut o);
        assert_eq!(o, "é😀日本");
    }

    /// Minimal strict JSON validator (objects, arrays, strings, numbers, literals).
    struct P<'a> {
        b: &'a [u8],
        i: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
                self.i += 1;
            }
        }
        fn eat(&mut self, c: u8) -> bool {
            self.ws();
            if self.b.get(self.i) == Some(&c) {
                self.i += 1;
                true
            } else {
                false
            }
        }
        fn val(&mut self) -> bool {
            self.ws();
            match self.b.get(self.i) {
                Some(b'{') => {
                    self.i += 1;
                    if self.eat(b'}') {
                        return true;
                    }
                    loop {
                        self.ws();
                        if !self.string() || !self.eat(b':') || !self.val() {
                            return false;
                        }
                        if self.eat(b',') {
                            continue;
                        }
                        return self.eat(b'}');
                    }
                }
                Some(b'[') => {
                    self.i += 1;
                    if self.eat(b']') {
                        return true;
                    }
                    loop {
                        if !self.val() {
                            return false;
                        }
                        if self.eat(b',') {
                            continue;
                        }
                        return self.eat(b']');
                    }
                }
                Some(b'"') => self.string(),
                Some(b't') => self.lit("true"),
                Some(b'f') => self.lit("false"),
                Some(b'n') => self.lit("null"),
                Some(c) if *c == b'-' || c.is_ascii_digit() => {
                    let s = self.i;
                    self.i += 1;
                    while self.i < self.b.len()
                        && matches!(
                            self.b[self.i],
                            b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
                        )
                    {
                        self.i += 1;
                    }
                    std::str::from_utf8(&self.b[s..self.i])
                        .unwrap()
                        .parse::<f64>()
                        .is_ok()
                }
                _ => false,
            }
        }
        fn lit(&mut self, l: &str) -> bool {
            if self.b[self.i..].starts_with(l.as_bytes()) {
                self.i += l.len();
                true
            } else {
                false
            }
        }
        fn string(&mut self) -> bool {
            if self.b.get(self.i) != Some(&b'"') {
                return false;
            }
            self.i += 1;
            while let Some(&c) = self.b.get(self.i) {
                match c {
                    b'"' => {
                        self.i += 1;
                        return true;
                    }
                    0..=0x1f => return false,
                    b'\\' => match self.b.get(self.i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 2,
                        Some(b'u') => {
                            let h = self.b.get(self.i + 2..self.i + 6);
                            if !h.is_some_and(|h| h.iter().all(|c| c.is_ascii_hexdigit())) {
                                return false;
                            }
                            self.i += 6;
                        }
                        _ => return false,
                    },
                    _ => self.i += 1,
                }
            }
            false
        }
    }

    fn valid(s: &str) -> bool {
        let mut p = P {
            b: s.as_bytes(),
            i: 0,
        };
        p.val() && {
            p.ws();
            p.i == s.len()
        }
    }

    fn json(r: &ScanResult) -> String {
        let mut v = Vec::new();
        render_json(r, &mut v).unwrap();
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn validator_sanity() {
        assert!(valid("{\"a\":[1,2.5,\"x\\u0001\"],\"b\":null}"));
        assert!(!valid("{\"a\":\"x\ny\"}"));
        assert!(!valid("{\"a\":1,}"));
    }

    #[test]
    fn json_nasty_strings() {
        let nasty = "q\"uote \\ back\nnl \u{1} 😀 é";
        let mut r = sample();
        r.root = nasty.into();
        r.tops[0].name = nasty.into();
        r.big_files[0].path = nasty.into();
        r.by_ext[0].0 = nasty.into();
        let j = json(&r);
        assert!(valid(&j), "{j}");
        assert!(j.contains("😀") && j.contains("\\u0001") && j.contains("\\\\"));
        assert!(!j.contains('\u{1}'));
    }

    #[test]
    fn json_keys_and_order() {
        let j = json(&sample());
        assert!(valid(&j), "{j}");
        let keys = [
            "\"root\"",
            "\"source\":\"batched\"",
            "\"threads\":8",
            "\"elapsed_secs\":2.0",
            "\"total\"",
            "\"skipped_reparse\"",
            "\"hardlink_dupes\"",
            "\"hardlink_dupe_bytes\"",
            "\"stream_bytes\"",
            "\"errors\"",
            "\"access_denied\"",
            "\"sharing_violation\"",
            "\"not_found\"",
            "\"dir_not_empty\"",
            "\"other\"",
            "\"tops\"",
            "\"is_dir\":true",
            "\"big_files\"",
            "\"by_ext\"",
            "\"ext\":\"iso\"",
        ];
        let mut last = 0;
        for k in keys {
            let p = j.find(k).unwrap_or_else(|| panic!("missing {k}"));
            assert!(p >= last, "order {k}");
            last = p;
        }
        assert_eq!(json(&sample()), j);
    }

    #[test]
    fn json_empty_result() {
        let j = json(&ScanResult::default());
        assert!(valid(&j), "{j}");
        assert!(j.contains("\"tops\":[]") && j.contains("\"source\":\"readdir\""));
    }

    #[test]
    fn json_fractional_elapsed() {
        let mut r = sample();
        r.elapsed = Duration::from_millis(1500);
        assert!(json(&r).contains("\"elapsed_secs\":1.5"));
    }
}
