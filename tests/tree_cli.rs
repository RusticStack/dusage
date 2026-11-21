//! End-to-end tests of `dusage tree`: the shape against the real `tree`, the sizes against
//! the files we wrote, and the filters tree has no opinion about.
//!
//! The comparison with `tree` is skipped, with a note on stderr, when the binary is absent.
//! A skip is printed rather than passed silently: a green run on a box without tree must not
//! read as agreement with it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Create `link` as a directory link to `target`: a junction on Windows, a symlink on
/// Linux. Either is what the walk must print and never follow.
fn link_dir(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

fn dusage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("tree")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "tree failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout.clone()).unwrap()
}

/// Whether the reference `tree` runs here. When it does not, the comparison test says so and
/// returns; every other test in this file stands on its own.
fn real_tree() -> bool {
    match Command::new("tree").arg("--version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("skipped: tree is not installed");
            false
        }
    }
}

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    /// A small tree with an 8 KiB file, nested directories, a dotfile, and a link pointing at
    /// a directory outside the tree so "never followed" is observable.
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("dusage_tree_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("beta/nested")).unwrap();
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("alpha/first.txt"), b"alpha").unwrap();
        fs::write(root.join("beta/nested/deep.bin"), vec![b'x'; 8192]).unwrap();
        fs::write(root.join("top.log"), vec![b'y'; 4000]).unwrap();
        fs::write(root.join(".secret"), b"hidden").unwrap();
        fs::write(outside.join("huge.bin"), vec![b'z'; 500_000]).unwrap();
        if !link_dir(&root.join("link_out"), &outside) {
            eprintln!("skipped: could not create a directory link on this platform");
        }
        Fixture { base, root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// A `tree` line reduced to the same shape ours has after its size brackets are removed.
fn strip_sizes(line: &str) -> String {
    // "├── [  512   4]  name" -> "├── name"
    if let Some(open) = line.find('[') {
        if let Some(close) = line[open..].find("]  ") {
            let mut s = String::new();
            s.push_str(&line[..open]);
            s.push_str(&line[open + close + 3..]);
            return s;
        }
    }
    line.to_string()
}

fn structural(out: &str) -> Vec<String> {
    out.lines()
        .filter(|l| !l.is_empty())
        .filter(|l| !l.contains("directories,"))
        .map(strip_sizes)
        // tree pads its continuation columns with no-break spaces under a UTF-8 locale;
        // the indentation is the same four columns either way.
        .map(|l| l.replace('\u{a0}', " "))
        .collect()
}

#[test]
fn matches_the_real_tree_shape() {
    if !real_tree() {
        return;
    }
    let f = Fixture::new("shape");
    let ours = stdout(&dusage(&[
        "--no-ignore",
        "-a",
        "--json",
        f.root.to_str().unwrap(),
    ]));
    let ours_text = stdout(&dusage(&["--no-ignore", "-a", f.root.to_str().unwrap()]));
    assert!(!ours.is_empty());
    let reference = Command::new("tree")
        .args(["-a", "--noreport"])
        .arg(&f.root)
        .output()
        .unwrap();
    assert!(reference.status.success());
    let reference_text = String::from_utf8(reference.stdout).unwrap();
    assert_eq!(
        structural(&ours_text),
        structural(&reference_text),
        "our listing and tree's disagree on which entries exist or how they nest"
    );
}

#[test]
fn apparent_sizes_are_the_files_we_wrote() {
    let f = Fixture::new("sizes");
    let out = stdout(&dusage(&["--json", f.root.to_str().unwrap()]));
    let deep = line_of(&out, "deep.bin");
    let apparent = number_after(deep, "\"apparent\":");
    assert_eq!(apparent, 8192, "apparent size is the file's length");
    let on_disk = number_after(deep, "\"on_disk\":");
    assert_eq!(on_disk % 512, 0, "on-disk bytes are whole blocks");
    assert!(
        (8192..8192 + 65_536).contains(&on_disk),
        "on-disk {on_disk} for an 8192-byte file"
    );
    // The directory above it carries the subtree, as tree --du does.
    let beta = line_of(&out, "\"name\":\"beta\"");
    assert_eq!(number_after(beta, "\"apparent\":"), 8192);
    assert!(number_after(beta, "\"on_disk\":") >= on_disk);
}

#[test]
fn a_link_is_printed_and_never_followed() {
    let f = Fixture::new("link");
    let out = stdout(&dusage(&["-a", "--json", f.root.to_str().unwrap()]));
    let link = line_of(&out, "\"name\":\"link_out\"");
    assert!(link.contains("\"is_link\":true"), "{link}");
    assert!(
        !out.contains("huge.bin"),
        "the walk entered the link's target"
    );
    let total = number_after(&out, "\"apparent\":");
    assert!(total < 500_000, "the target's bytes leaked into the totals");
    let text = stdout(&dusage(&["-a", f.root.to_str().unwrap()]));
    assert!(
        text.contains("link_out -> "),
        "a link prints its target, tree's way"
    );
}

#[test]
fn hidden_entries_need_all() {
    let f = Fixture::new("hidden");
    let plain = stdout(&dusage(&[f.root.to_str().unwrap()]));
    assert!(!plain.contains(".secret"));
    let all = stdout(&dusage(&["-a", f.root.to_str().unwrap()]));
    assert!(all.contains(".secret"));
}

#[test]
fn level_limits_depth_and_dirs_only_filters() {
    let f = Fixture::new("level");
    let one = stdout(&dusage(&["--level", "1", f.root.to_str().unwrap()]));
    assert!(one.contains("alpha"));
    assert!(!one.contains("first.txt"), "--level 1 is the children");
    let dirs = stdout(&dusage(&["-d", f.root.to_str().unwrap()]));
    assert!(dirs.contains("beta"));
    assert!(!dirs.contains("top.log"));
}

#[test]
fn sort_size_puts_the_largest_subtree_first() {
    let f = Fixture::new("sort");
    let out = stdout(&dusage(&["--sort", "size", f.root.to_str().unwrap()]));
    let first = out
        .lines()
        .find(|l| l.contains("├── ") || l.contains("└── "))
        .expect("a child line");
    assert!(
        first.contains("beta"),
        "the 8192-byte subdirectory sorts first, got: {first}"
    );
}

#[test]
fn json_is_well_formed_and_agrees_with_the_footer() {
    let f = Fixture::new("json");
    let out = stdout(&dusage(&["--json", f.root.to_str().unwrap()]));
    assert!(out.starts_with("{\"root\":\""));
    assert!(out.trim_end().ends_with('}'));
    assert!(out.contains("\"entries\":["));
    let entries = out.matches("\"depth\":").count();
    assert_eq!(entries, 8, "the root plus its seven visible entries");
    let dirs = number_after(&out, "\"dirs\":");
    let files = number_after(&out, "\"files\":");
    assert_eq!(dirs + files, (entries - 1) as u64);
    let text = stdout(&dusage(&[f.root.to_str().unwrap()]));
    assert!(
        text.contains(&format!("{dirs} directories, {files} files")),
        "the footer counts what the JSON counts: {text}"
    );
}

#[test]
fn human_units_and_misuse() {
    let f = Fixture::new("human");
    let out = stdout(&dusage(&["-h", f.root.to_str().unwrap()]));
    assert!(out.contains("K]") || out.contains("5.0K") || out.contains("8.0K"));
    let bad = dusage(&["--sort", "mtime", f.root.to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(2));
    let missing = dusage(&[Path::new("/nonexistent/dusage/tree").to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(1));
    let help = stdout(&dusage(&["--help"]));
    assert!(help.contains("--level"));
}

/// The JSON object of the entry whose line holds `needle`, as text up to the next object.
fn line_of<'a>(json: &'a str, needle: &str) -> &'a str {
    let at = json.find(needle).unwrap_or_else(|| panic!("no {needle}"));
    let start = json[..at].rfind('{').unwrap_or(at);
    let end = json[start..]
        .find("},{")
        .map(|i| start + i + 1)
        .unwrap_or(json.len());
    &json[start..end]
}

fn number_after(s: &str, key: &str) -> u64 {
    let at = s.find(key).unwrap_or_else(|| panic!("no {key} in {s}"));
    let rest = &s[at + key.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("no number after {key}"))
}
