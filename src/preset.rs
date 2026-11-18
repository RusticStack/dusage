//! Reusable prune presets for `fastdel` (`dusage prune`): named bundles of prefix / age /
//! list-limit so `fastdel ~/work --preset cargo` beats remembering `--prefix target
//! --age 7d`.
//!
//! # File format
//!
//! One line-oriented, INI-ish file. `[name]` opens a preset section; every other line inside
//! a section is `key = value`. Blank lines and lines starting with `#` or `;` are ignored
//! (comments take a whole line: a `#` inside a value is part of the value, so paths with `#`
//! work). CRLF and LF both parse. Names and keys are case-sensitive.
//!
//! ```text
//! # comments start with # or ;
//! [cargo]
//! prefixes = target, cargo-target
//! age = 7d
//! list_limit = 50
//!
//! [agent-temp]
//! prefixes = tmp-, scratch-, session-
//! age = 2h
//! ```
//!
//! Keys, all optional individually but at least one of `prefixes`/`age` is required:
//!
//! - `prefixes` — comma-separated name prefixes (case-sensitive, as `--prefix`). The key may
//!   repeat and the values accumulate.
//! - `age` — anything [`crate::cli::parse_age`] accepts (`30s`, `15m`, `2h`, `7d`, `1w`, or a
//!   bare number of days). A negative age means "no age filter" on the command line, which is
//!   meaningless in a preset, so it is an error here.
//! - `list_limit` — non-negative integer, the rows a `--dry` run prints.
//!
//! A repeated key on one preset keeps the last value (`prefixes` accumulates instead). A
//! repeated section header continues the same preset rather than starting a second one, so a
//! preset can be extended further down the file; its fields merge by the rules above. Unknown
//! keys, a key before any section, an unterminated or empty header, and bad values are errors
//! naming the offending line. A preset that ends up with no prefixes and no age is an error —
//! a preset that only sets `list_limit` cleans nothing.
//!
//! # Which presets exist
//!
//! [`builtin`] ships `cargo`, `node` and `agent-temp` so the common cases need no config file.
//! [`effective`] returns the config file's presets plus every builtin the file does not define
//! by name, so a `[cargo]` section replaces (never extends) the builtin of that name.
//!
//! # Merging with the command line
//!
//! The preset supplies prefixes, age and list limit; anything given on the command line
//! overrides the preset for that field. Prefixes are the exception: the preset's come first and
//! the command line's are appended, never replacing them. So `fastdel ~/w --preset cargo
//! --prefix staging-` filters `target|staging-`, and `--age 1d --preset cargo` uses `1d` while
//! keeping the preset's prefixes.
//!
//! # Where it lives
//!
//! `$DUSAGE_CONFIG` when set and non-empty, else `$XDG_CONFIG_HOME/dusage/presets.conf` on
//! Unix, else `%APPDATA%\dusage\presets.conf` on Windows, else `~/.config/dusage/presets.conf`
//! (`$HOME`, or `%USERPROFILE%` on Windows). A missing file means "no presets", not an error.

use crate::cli::parse_age;
use std::path::PathBuf;
use std::time::Duration;

const CONFIG_FILE: &str = "presets.conf";

/// One named preset. Empty `prefixes` and `None` age are only possible for a caller building a
/// preset by hand; [`parse`] refuses that combination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preset {
    pub name: String,
    pub prefixes: Vec<String>,
    pub age: Option<Duration>,
    pub list_limit: Option<usize>,
}

/// The presets file path: `$DUSAGE_CONFIG`, else the platform config dir, else `~/.config`.
pub fn config_path() -> PathBuf {
    if let Some(p) = var_path("DUSAGE_CONFIG") {
        return p;
    }
    let dir = if cfg!(windows) {
        var_path("APPDATA")
    } else {
        var_path("XDG_CONFIG_HOME")
    };
    match dir {
        Some(dir) => dir.join("dusage").join(CONFIG_FILE),
        None => var_path("HOME")
            .or_else(|| var_path("USERPROFILE"))
            .unwrap_or_default()
            .join(".config")
            .join("dusage")
            .join(CONFIG_FILE),
    }
}

/// A set (possibly empty) environment variable as a path; empty counts as unset.
fn var_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Parse the presets file text.
pub fn parse(text: &str) -> Result<Vec<Preset>, String> {
    let mut presets: Vec<Preset> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut current: Option<usize> = None;
    for (idx, raw) in text.lines().enumerate() {
        let no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| {
                format!("line {no}: unterminated section header {line:?} (expected [name])")
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!("line {no}: empty preset name"));
            }
            if name.contains(char::is_whitespace) || name.contains('=') {
                return Err(format!(
                    "line {no}: invalid preset name {name:?} (no spaces, no `=`)"
                ));
            }
            current = Some(match presets.iter().position(|p| p.name == name) {
                Some(i) => i,
                None => {
                    presets.push(Preset {
                        name: name.to_string(),
                        ..Preset::default()
                    });
                    starts.push(no);
                    presets.len() - 1
                }
            });
            continue;
        }
        let Some(i) = current else {
            return Err(format!(
                "line {no}: {line:?} is outside a [preset] section; start one with [name]"
            ));
        };
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {no}: expected `key = value`, found {line:?}"))?;
        let (key, value) = (key.trim(), value.trim());
        let p = &mut presets[i];
        match key {
            "prefixes" => {
                let got: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if got.is_empty() {
                    return Err(format!("line {no}: `prefixes` is empty"));
                }
                p.prefixes.extend(got);
            }
            "age" => {
                let age = parse_age(value).map_err(|e| format!("line {no}: {e}"))?;
                p.age = Some(
                    age.ok_or_else(|| format!("line {no}: a preset age must not be negative"))?,
                );
            }
            "list_limit" => {
                let n = value.parse::<usize>().map_err(|_| {
                    format!(
                        "line {no}: invalid list_limit {value:?} (must be a non-negative integer)"
                    )
                })?;
                p.list_limit = Some(n);
            }
            _ => {
                return Err(format!(
                    "line {no}: unknown key {key:?} (expected prefixes, age, or list_limit)"
                ))
            }
        }
    }
    for (p, start) in presets.iter().zip(&starts) {
        if p.prefixes.is_empty() && p.age.is_none() {
            return Err(format!(
                "line {start}: preset {:?} has no prefixes and no age",
                p.name
            ));
        }
    }
    Ok(presets)
}

/// Read and parse [`config_path`]; a missing file is an empty preset list.
pub fn load() -> Result<Vec<Preset>, String> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Config presets followed by every builtin the config does not define by name.
pub fn effective() -> Result<Vec<Preset>, String> {
    let mut all = load()?;
    for b in builtin() {
        if !all.iter().any(|p| p.name == b.name) {
            all.push(b);
        }
    }
    Ok(all)
}

fn preset(name: &str, prefixes: &[&str], age: Duration) -> Preset {
    Preset {
        name: name.to_string(),
        prefixes: prefixes.iter().map(|p| (*p).to_string()).collect(),
        age: Some(age),
        list_limit: None,
    }
}

/// The presets that never need a config file.
pub fn builtin() -> Vec<Preset> {
    vec![
        preset(
            "cargo",
            &["target", "cargo-target"],
            Duration::from_secs(7 * 86400),
        ),
        preset(
            "node",
            &["node_modules", ".next", "dist", "build"],
            Duration::from_secs(14 * 86400),
        ),
        preset(
            "agent-temp",
            &["tmp-", "scratch-", "session-markers", "agent-"],
            Duration::from_secs(2 * 3600),
        ),
    ]
}

/// Look up a preset by exact name; the error lists the names that do exist, sorted.
pub fn find<'a>(presets: &'a [Preset], name: &str) -> Result<&'a Preset, String> {
    if let Some(p) = presets.iter().find(|p| p.name == name) {
        return Ok(p);
    }
    let mut names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
    if names.is_empty() {
        return Err(format!(
            "unknown preset {name:?}; no presets are defined ({} is missing or empty)",
            config_path().display()
        ));
    }
    names.sort_unstable();
    Err(format!(
        "unknown preset {name:?}; available: {}",
        names.join(", ")
    ))
}

/// A commented starter file for `fastdel --print-presets`; it parses cleanly as written.
pub fn sample_config() -> &'static str {
    concat!(
        "# dusage presets, used by `fastdel <root> --preset NAME` (and `dusage prune`).\n",
        "#\n",
        "# One [section] per preset. Keys, all optional but at least one of prefixes/age:\n",
        "#   prefixes    comma-separated name prefixes; repeat the key to add more\n",
        "#   age         entries older than this go: 30s 15m 2h 7d 1w, or a number of days\n",
        "#   list_limit  rows a --dry run prints (default 50)\n",
        "#\n",
        "# Comments start with # or ; and take a whole line. Builtins (cargo, node,\n",
        "# agent-temp) apply wherever this file does not define the same name.\n",
        "\n",
        "[cargo]\n",
        "prefixes = target, cargo-target\n",
        "age = 7d\n",
        "list_limit = 50\n",
        "\n",
        "[agent-temp]\n",
        "prefixes = tmp-, scratch-, session-\n",
        "age = 2h\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    // Tests share one process environment, so anything touching DUSAGE_CONFIG serialises.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct ConfigEnv {
        old: Option<OsString>,
        _guard: MutexGuard<'static, ()>,
    }

    impl ConfigEnv {
        /// Point `DUSAGE_CONFIG` at `value` (or unset it) until dropped.
        fn set(value: Option<&std::path::Path>) -> ConfigEnv {
            let guard = env_lock();
            let old = std::env::var_os("DUSAGE_CONFIG");
            match value {
                Some(p) => std::env::set_var("DUSAGE_CONFIG", p),
                None => std::env::remove_var("DUSAGE_CONFIG"),
            }
            ConfigEnv { old, _guard: guard }
        }
    }

    impl Drop for ConfigEnv {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var("DUSAGE_CONFIG", v),
                None => std::env::remove_var("DUSAGE_CONFIG"),
            }
        }
    }

    fn temp_file(text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dusage-presets-{}-{}.conf",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    fn d(v: u64) -> Option<Duration> {
        Some(Duration::from_secs(v))
    }

    #[test]
    fn empty_text_has_no_presets() {
        assert_eq!(parse(""), Ok(Vec::new()));
        assert_eq!(parse("\n\n   \n"), Ok(Vec::new()));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let p = parse("# header\n; also a comment\n\n   # indented\n[cargo]\n# inner\nage = 7d\n")
            .unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].name, "cargo");
        assert_eq!(p[0].age, d(7 * 86400));
        assert!(p[0].prefixes.is_empty());
    }

    #[test]
    fn all_three_keys() {
        let p =
            parse("[cargo]\nprefixes = target, cargo-target\nage = 7d\nlist_limit = 50\n").unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].prefixes, ["target", "cargo-target"]);
        assert_eq!(p[0].age, d(7 * 86400));
        assert_eq!(p[0].list_limit, Some(50));
    }

    #[test]
    fn age_only_preset_is_allowed() {
        let p = parse("[old]\nage = 30d\n").unwrap();
        assert_eq!(p[0].age, d(30 * 86400));
        assert_eq!(p[0].list_limit, None);
    }

    #[test]
    fn repeated_prefixes_accumulate() {
        let p = parse("[a]\nprefixes = x\nprefixes = y, z\nage = 1d\n").unwrap();
        assert_eq!(p[0].prefixes, ["x", "y", "z"]);
    }

    #[test]
    fn crlf_line_endings() {
        let p = parse("[cargo]\r\nprefixes = target\r\nage = 7d\r\n").unwrap();
        assert_eq!(p[0].name, "cargo");
        assert_eq!(p[0].prefixes, ["target"]);
        assert_eq!(p[0].age, d(7 * 86400));
    }

    #[test]
    fn whitespace_around_equals() {
        let p =
            parse("  [cargo]  \n\tprefixes\t=\t target ,  x \nage=7d\nlist_limit= 5 \n").unwrap();
        assert_eq!(p[0].prefixes, ["target", "x"]);
        assert_eq!(p[0].age, d(7 * 86400));
        assert_eq!(p[0].list_limit, Some(5));
    }

    #[test]
    fn section_without_keys_is_error() {
        let e = parse("[empty]\n\n[ok]\nage = 1d\n").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        assert!(e.contains("no prefixes and no age"), "{e}");
    }

    #[test]
    fn list_limit_only_preset_is_error() {
        let e = parse("[x]\nlist_limit = 5\n").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        assert!(e.contains("no prefixes and no age"), "{e}");
    }

    #[test]
    fn unknown_key_names_the_line() {
        let e = parse("[a]\nage = 1d\nbogus = 1\n").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        assert!(e.contains("bogus"), "{e}");
    }

    #[test]
    fn key_is_case_sensitive() {
        let e = parse("[a]\nPrefixes = x\n").unwrap_err();
        assert!(e.contains("unknown key"), "{e}");
    }

    #[test]
    fn duplicate_section_accumulates() {
        let p =
            parse("[a]\nprefixes = x\nage = 1d\n\n[a]\nprefixes = y\nlist_limit = 9\n").unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].prefixes, ["x", "y"]);
        assert_eq!(p[0].age, d(86400));
        assert_eq!(p[0].list_limit, Some(9));
    }

    #[test]
    fn duplicate_age_keeps_the_last() {
        let p = parse("[a]\nprefixes = x\nage = 1d\nage = 2h\n").unwrap();
        assert_eq!(p[0].age, d(7200));
    }

    #[test]
    fn bad_age_names_the_line() {
        let e = parse("[a]\nprefixes = x\nage = 7x\n").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        assert!(e.contains("invalid age"), "{e}");
    }

    #[test]
    fn negative_age_is_error() {
        let e = parse("[a]\nprefixes = x\nage = -1\n").unwrap_err();
        assert!(e.contains("line 3"), "{e}");
        assert!(e.contains("negative"), "{e}");
    }

    #[test]
    fn bad_list_limit_is_error() {
        for bad in ["abc", "-5", "1.5", ""] {
            let e = parse(&format!("[a]\nprefixes = x\nlist_limit = {bad}\n")).unwrap_err();
            assert!(e.contains("line 3"), "{e} {bad}");
            assert!(e.contains("list_limit"), "{e} {bad}");
        }
    }

    #[test]
    fn key_outside_a_section_is_error() {
        let e = parse("prefixes = x\n").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        assert!(e.contains("outside a [preset] section"), "{e}");
    }

    #[test]
    fn line_without_equals_is_error() {
        let e = parse("[a]\nprefixes x\n").unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        assert!(e.contains("key = value"), "{e}");
    }

    #[test]
    fn empty_prefixes_value_is_error() {
        let e = parse("[a]\nprefixes = , ,\nage = 1d\n").unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        assert!(e.contains("empty"), "{e}");
    }

    #[test]
    fn section_header_errors() {
        assert!(parse("[cargo\n").unwrap_err().contains("line 1"));
        assert!(parse("[]\n").unwrap_err().contains("empty preset name"));
        assert!(parse("[car go]\n")
            .unwrap_err()
            .contains("invalid preset name"));
        assert!(parse("[a=b]\n")
            .unwrap_err()
            .contains("invalid preset name"));
    }

    #[test]
    fn multiple_presets_keep_order() {
        let p = parse(
            "[cargo]\nprefixes = target\nage = 7d\n[agent-temp]\nprefixes = tmp-\nage = 2h\n",
        )
        .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "cargo");
        assert_eq!(p[1].name, "agent-temp");
        assert_eq!(p[1].prefixes, ["tmp-"]);
        assert_eq!(p[1].age, d(7200));
        assert_eq!(p[1].list_limit, None);
    }

    #[test]
    fn comment_inside_value_is_literal() {
        let p = parse("[a]\nprefixes = x # not a comment\nage = 1d\n").unwrap();
        assert_eq!(p[0].prefixes, ["x # not a comment"]);
    }

    #[test]
    fn builtin_names_and_shapes() {
        let b = builtin();
        let names: Vec<&str> = b.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["cargo", "node", "agent-temp"]);
        let cargo = find(&b, "cargo").unwrap();
        assert_eq!(cargo.prefixes, ["target", "cargo-target"]);
        assert_eq!(cargo.age, d(7 * 86400));
        let node = find(&b, "node").unwrap();
        assert_eq!(node.prefixes, ["node_modules", ".next", "dist", "build"]);
        assert_eq!(node.age, d(14 * 86400));
        let at = find(&b, "agent-temp").unwrap();
        assert_eq!(
            at.prefixes,
            ["tmp-", "scratch-", "session-markers", "agent-"]
        );
        assert_eq!(at.age, d(7200));
        assert!(b.iter().all(|p| p.list_limit.is_none()));
    }

    #[test]
    fn find_returns_the_preset() {
        let p = vec![Preset {
            name: "x".into(),
            prefixes: vec!["pre".into()],
            age: None,
            list_limit: Some(3),
        }];
        assert_eq!(find(&p, "x").unwrap().list_limit, Some(3));
    }

    #[test]
    fn find_missing_lists_names_sorted() {
        let p = parse("[zeta]\nage = 1d\n[alpha]\nage = 1d\n[mid]\nage = 1d\n").unwrap();
        let e = find(&p, "nope").unwrap_err();
        assert!(e.contains("unknown preset \"nope\""), "{e}");
        assert!(e.contains("available: alpha, mid, zeta"), "{e}");
    }

    #[test]
    fn find_missing_on_empty_list() {
        let e = find(&[], "cargo").unwrap_err();
        assert!(e.contains("unknown preset \"cargo\""), "{e}");
        assert!(e.contains("no presets are defined"), "{e}");
    }

    #[test]
    fn sample_config_parses() {
        let p = parse(sample_config()).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(find(&p, "cargo").unwrap().list_limit, Some(50));
        assert_eq!(find(&p, "agent-temp").unwrap().age, d(7200));
    }

    #[test]
    fn config_path_honours_dusage_config() {
        let path = std::path::Path::new("/tmp/somewhere/presets.conf");
        let _env = ConfigEnv::set(Some(path));
        assert_eq!(config_path(), path);
    }

    #[test]
    fn empty_dusage_config_falls_through() {
        let _env = ConfigEnv::set(None);
        std::env::set_var("DUSAGE_CONFIG", "");
        let path = config_path();
        assert!(path.ends_with("dusage/presets.conf"), "{}", path.display());
        std::env::remove_var("DUSAGE_CONFIG");
    }

    #[test]
    fn default_config_path_ends_with_presets_conf() {
        let _env = ConfigEnv::set(None);
        let path = config_path();
        assert!(path.ends_with("dusage/presets.conf"), "{}", path.display());
        assert_ne!(path, PathBuf::from("/nonexistent/presets.conf"));
    }

    #[test]
    fn missing_config_file_is_empty() {
        let _env = ConfigEnv::set(Some(std::path::Path::new(
            "/nonexistent-dusage-dir/presets.conf",
        )));
        assert_eq!(load(), Ok(Vec::new()));
    }

    #[test]
    fn load_reads_the_configured_file() {
        let path = temp_file("[cargo]\nprefixes = target\nage = 7d\n");
        let _env = ConfigEnv::set(Some(&path));
        let got = load().unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "cargo");
        assert_eq!(got[0].prefixes, ["target"]);
    }

    #[test]
    fn load_reports_parse_errors_with_the_path() {
        let path = temp_file("[cargo]\nage = 7x\n");
        let shown = path.display().to_string();
        let _env = ConfigEnv::set(Some(&path));
        let e = load().unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(e.starts_with(&shown), "{e}");
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn config_overrides_a_builtin_of_the_same_name() {
        let path = temp_file("[cargo]\nprefixes = only-this\nage = 1d\n");
        let _env = ConfigEnv::set(Some(&path));
        let all = effective().unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(all.len(), 3);
        let cargo = find(&all, "cargo").unwrap();
        assert_eq!(cargo.prefixes, ["only-this"]);
        assert_eq!(cargo.age, d(86400));
        assert_eq!(find(&all, "node").unwrap().prefixes.len(), 4);
    }

    #[test]
    fn effective_without_a_config_is_the_builtins() {
        let _env = ConfigEnv::set(Some(std::path::Path::new(
            "/nonexistent-dusage-dir/presets.conf",
        )));
        let all = effective().unwrap();
        assert_eq!(all, builtin());
    }

    #[test]
    fn effective_keeps_extra_config_presets_ahead_of_builtins() {
        let path = temp_file("[mine]\nprefixes = mine-\n");
        let _env = ConfigEnv::set(Some(&path));
        let all = effective().unwrap();
        std::fs::remove_file(&path).ok();
        let names: Vec<&str> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["mine", "cargo", "node", "agent-temp"]);
    }
}
