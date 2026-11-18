//! Text and JSON rendering of a `ScanResult`. Owned by wave-1 lane `report`.
//!
//! Formatting helpers here are deliberately independent of `cli.rs`.

use crate::model::{ScanResult, Totals};
use crate::search::{SearchOptions, SearchReport};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let slash = if t.is_dir {
            std::path::MAIN_SEPARATOR_STR
        } else {
            ""
        };
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
    if r.skipped_mounts > 0 {
        foot.push(format!(
            "skipped {} mount points (--one-filesystem)",
            r.skipped_mounts
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
        ",\"skipped_reparse\":{},\"skipped_mounts\":{},\"hardlink_dupes\":{},\"hardlink_dupe_bytes\":{},\"stream_bytes\":{}",
        r.skipped_reparse,
        r.skipped_mounts,
        r.hardlink_dupes,
        r.hardlink_dupe_bytes,
        r.stream_bytes
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

/// ANSI wrappers used by `--color=always` and by a terminal on stdout.
pub mod color {
    pub const RESET: &str = "\x1b[0m";
    pub const PATH: &str = "\x1b[35m";
    pub const LINE: &str = "\x1b[32m";
    pub const MATCH: &str = "\x1b[1;31m";
    pub const STATS: &str = "\x1b[36m";
}

/// `2026-11-18 14:03:22` in UTC, or `-` when the time is unknown.
pub fn fmt_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "-".to_string() };
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Howard Hinnant's days-from-civil inverse: days since 1970-01-01 to (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn write_match_line(
    out: &mut dyn Write,
    text: &[u8],
    spans: &[(usize, usize)],
    color: bool,
) -> io::Result<()> {
    if !color || spans.is_empty() {
        return out.write_all(text);
    }
    let mut at = 0;
    for &(s, e) in spans {
        if s >= text.len() || e > text.len() || s < at {
            continue;
        }
        out.write_all(&text[at..s])?;
        out.write_all(color::MATCH.as_bytes())?;
        out.write_all(&text[s..e])?;
        out.write_all(color::RESET.as_bytes())?;
        at = e;
    }
    out.write_all(&text[at..])
}

/// Text output for `dusage search`: `path:line:text`, `-o`, `-c`, `-l` and context groups.
pub fn render_search(
    r: &SearchReport,
    o: &SearchOptions,
    color: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    // The CLI decides from the roots (several paths, or a directory walk); a bare
    // `dusage search needle file.txt` prints lines without a file name.
    let many = o.with_filename.unwrap_or(r.files.len() > 1);
    for f in &r.files {
        if o.files_with_matches || o.files_without_matches {
            if o.null {
                write!(out, "{}\0", f.path)?;
            } else {
                writeln!(out, "{}", f.path)?;
            }
            continue;
        }
        if o.count {
            if many {
                let sep = if o.null { '\0' } else { ':' };
                writeln!(out, "{}{sep}{}", f.path, f.matches)?;
            } else {
                writeln!(out, "{}", f.matches)?;
            }
            continue;
        }
        if f.binary && !o.text {
            writeln!(out, "Binary file {} matches", f.path)?;
            continue;
        }
        let mut last: Option<u64> = None;
        for h in &f.hits {
            if let Some(l) = last {
                if h.number > l + 1 {
                    writeln!(out, "--")?;
                }
            }
            last = Some(h.number);
            let colon = if h.matched { ':' } else { '-' };
            if many {
                if color {
                    write!(out, "{}{}{}", color::PATH, f.path, color::RESET)?;
                } else {
                    write!(out, "{}", f.path)?;
                }
                if o.null {
                    write!(out, "\0")?;
                } else {
                    write!(out, "{colon}")?;
                }
            }
            if o.line_numbers {
                if color {
                    write!(out, "{}{}{}{colon}", color::LINE, h.number, color::RESET)?;
                } else {
                    write!(out, "{}", h.number)?;
                    write!(out, "{colon}")?;
                }
            }
            if h.matched {
                write_match_line(out, &h.text, &h.spans, color)?;
            } else {
                out.write_all(&h.text)?;
            }
            writeln!(out)?;
        }
    }
    Ok(())
}

/// One JSON document for `dusage search --json`, with the report's stats appended.
pub fn render_search_json(
    r: &SearchReport,
    o: &SearchOptions,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut s = String::new();
    s.push_str("{\"files\":[");
    for (i, f) in r.files.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"path\":");
        jstr(&f.path, &mut s);
        let _ = write!(s, ",\"matches\":{},\"binary\":{},", f.matches, f.binary);
        if o.count || o.files_with_matches || o.files_without_matches {
            s.push_str("\"lines\":[]}");
            continue;
        }
        s.push_str("\"lines\":[");
        for (j, h) in f.hits.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"line_number\":{},\"context\":{},",
                h.number, !h.matched
            );
            s.push_str("\"text\":");
            jstr(&String::from_utf8_lossy(&h.text), &mut s);
            s.push_str(",\"submatches\":[");
            for (k, &(a, b)) in h.spans.iter().enumerate() {
                if k > 0 {
                    s.push(',');
                }
                let _ = write!(s, "{{\"start\":{a},\"end\":{b},\"text\":");
                jstr(
                    &String::from_utf8_lossy(&h.text[a.min(h.text.len())..b.min(h.text.len())]),
                    &mut s,
                );
                s.push('}');
            }
            s.push_str("]}");
        }
        s.push_str("]}");
    }
    let st = &r.stats;
    let _ = writeln!(
        s,
        "],\"stats\":{{\"files_scanned\":{},\"files_matched\":{},\"files_binary\":{},\"bytes_scanned\":{},\"matches\":{},\"errors\":{},\"elapsed_secs\":{}}}}}",
        st.files_scanned,
        st.files_matched,
        st.files_binary,
        st.bytes_scanned,
        st.matches,
        st.errors.total(),
        jf64(st.elapsed.as_secs_f64())
    );
    out.write_all(s.as_bytes())
}

/// Text output for `dusage find`: one path per line, or a long listing.
pub fn render_paths(
    r: &SearchReport,
    long: bool,
    color: bool,
    null: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    for c in &r.paths {
        if long {
            writeln!(
                out,
                "{:>12}  {}  {}{}",
                crate::cli::fmt_bytes(c.size),
                fmt_time(c.modified),
                c.rel,
                if c.is_dir { "/" } else { "" }
            )?;
        } else if null {
            write!(out, "{}\0", c.rel)?;
        } else if color {
            writeln!(out, "{}{}{}", color::PATH, c.rel, color::RESET)?;
        } else {
            writeln!(out, "{}", c.rel)?;
        }
    }
    Ok(())
}

/// JSON for `dusage find --json`.
pub fn render_paths_json(r: &SearchReport, out: &mut dyn Write) -> io::Result<()> {
    let mut s = String::new();
    s.push_str("{\"paths\":[");
    for (i, c) in r.paths.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"rel\":");
        jstr(&c.rel, &mut s);
        s.push_str(",\"path\":");
        jstr(&c.path.to_string_lossy(), &mut s);
        let _ = write!(
            s,
            ",\"size\":{},\"is_dir\":{},\"modified\":\"{}\"}}",
            c.size,
            c.is_dir,
            fmt_time(c.modified)
        );
    }
    let _ = writeln!(s, "],\"count\":{}}}", r.paths.len());
    out.write_all(s.as_bytes())
}

/// A one-line statistics footer, printed to stderr by the binaries.
pub fn stats_line(r: &SearchReport) -> String {
    let st = &r.stats;
    format!(
        "{} files scanned ({} binary), {} matched, {} matches, {} errors, {:.2?}",
        st.files_scanned,
        st.files_binary,
        st.files_matched,
        st.matches,
        st.errors.total(),
        st.elapsed
    )
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
            String::new(),
            "=== TOTAL: 3.50 GB | files 11 | dirs 2 | elapsed 2s (6 files/s) | source batched ==="
                .to_string(),
            String::new(),
            r"--- top 5 entries under C:\data ---".to_string(),
            format!("      3.00 GB  big{}", std::path::MAIN_SEPARATOR),
            "      0.50 GB  f.bin".to_string(),
            String::new(),
            "--- files >= 1.00 GB (top 5) ---".to_string(),
            r"      2.00 GB  C:\data\big\x.iso".to_string(),
            String::new(),
        ]
        .join("\n");
        assert_eq!(text(&sample(), &opts(false, false)), expect);
    }

    #[test]
    fn golden_alloc_and_ext() {
        let s = text(&sample(), &opts(true, true));
        assert!(s.contains("| on disk 3.60 GB |"));
        assert!(s.contains(&format!(
            "      3.00 GB        3.10 GB on disk  big{}\n",
            std::path::MAIN_SEPARATOR
        )));
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

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(None), "-");
        assert_eq!(fmt_time(Some(UNIX_EPOCH)), "1970-01-01 00:00:00");
        assert_eq!(
            fmt_time(Some(
                UNIX_EPOCH + std::time::Duration::from_secs(1_763_500_802)
            )),
            "2025-11-18 21:20:02"
        );
        // 2000-02-29 handled by the civil algorithm.
        assert_eq!(
            fmt_time(Some(
                UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)
            )),
            "2000-02-29 00:00:00"
        );
    }

    fn search_sample() -> (crate::search::SearchReport, crate::search::SearchOptions) {
        use crate::pattern::Pattern;
        use crate::search::{FileHits, LineHit, SearchOptions, SearchReport, Stats};
        use std::path::PathBuf;
        let mut o = SearchOptions::new(
            Pattern::new("needle", false, false, false).unwrap(),
            vec![PathBuf::from("/data")],
        );
        o.line_numbers = true;
        let mut r = SearchReport {
            files: vec![FileHits {
                path: "/data/a.txt".into(),
                hits: vec![
                    LineHit {
                        number: 2,
                        text: b"a needle".to_vec(),
                        matched: true,
                        spans: vec![(2, 8)],
                    },
                    LineHit {
                        number: 5,
                        text: b"another needle".to_vec(),
                        matched: true,
                        spans: vec![(8, 14)],
                    },
                ],
                matches: 2,
                binary: false,
            }],
            stats: Stats {
                files_scanned: 3,
                files_matched: 1,
                matches: 2,
                ..Default::default()
            },
            any_match: true,
            ..Default::default()
        };
        r.stats.elapsed = Duration::from_millis(5);
        (r, o)
    }

    #[test]
    fn search_text_and_json() {
        let (r, mut o) = search_sample();
        o.with_filename = Some(true);
        let mut v = Vec::new();
        render_search(&r, &o, false, &mut v).unwrap();
        let text = String::from_utf8(v).unwrap();
        assert!(text.contains("a.txt:2:a needle"), "{text}");
        assert!(
            text.contains("--\n"),
            "non-contiguous groups are separated: {text}"
        );
        let mut v = Vec::new();
        render_search_json(&r, &o, &mut v).unwrap();
        let json = String::from_utf8(v).unwrap();
        assert!(
            json.starts_with("{\"files\":[{\"path\":\"/data/a.txt\""),
            "{json}"
        );
        assert!(json.contains("\"line_number\":2"), "{json}");
        assert!(
            json.contains("\"start\":2,\"end\":8,\"text\":\"needle\""),
            "{json}"
        );
        assert!(json.trim_end().ends_with('}'), "{json}");
        assert_eq!(json.matches("{\"path\"").count(), 1);
    }

    #[test]
    fn count_and_files_listing() {
        let (mut r, mut o) = search_sample();
        o.count = true;
        r.files[0].hits.clear();
        let mut v = Vec::new();
        render_search(&r, &o, false, &mut v).unwrap();
        assert_eq!(String::from_utf8(v).unwrap(), "2\n");
        o.count = false;
        o.files_with_matches = true;
        let mut v = Vec::new();
        render_search(&r, &o, false, &mut v).unwrap();
        assert_eq!(String::from_utf8(v).unwrap(), "/data/a.txt\n");
    }

    #[test]
    fn path_listing_modes() {
        use crate::search::{Candidate, SearchReport};
        let r = SearchReport {
            paths: vec![Candidate {
                path: "/data/x.rs".into(),
                rel: "x.rs".into(),
                size: 1536,
                modified: Some(UNIX_EPOCH),
                is_dir: false,
                is_link: false,
            }],
            ..Default::default()
        };
        let mut v = Vec::new();
        render_paths(&r, false, false, false, &mut v).unwrap();
        assert_eq!(String::from_utf8(v).unwrap(), "x.rs\n");
        let mut v = Vec::new();
        render_paths(&r, true, false, false, &mut v).unwrap();
        assert_eq!(
            String::from_utf8(v).unwrap(),
            "     1.54 KB  1970-01-01 00:00:00  x.rs\n"
        );
        let mut v = Vec::new();
        render_paths_json(&r, &mut v).unwrap();
        let json = String::from_utf8(v).unwrap();
        assert!(json.contains("\"count\":1"), "{json}");
        assert!(json.contains("\"rel\":\"x.rs\""), "{json}");
    }
}
