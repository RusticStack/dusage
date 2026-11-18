//! Ignore rules for the search walker.
//!
//! Three layers, applied in this order:
//!
//! 1. **Built-in caches**: `.git`, `.hg`, `.svn`, `node_modules`, `target`, `__pycache__`,
//!    `.venv`, `venv`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.gradle`.
//!    Disabled by `--no-ignore`.
//! 2. **Ignore files** read while walking: `.gitignore` and `.ignore` in the scanned
//!    directory and in every directory above an entry, each applying to the paths below
//!    it, with later rules and deeper files winning, and `!` re-including.
//! 3. **Explicit `--glob` filters**, which are the only thing that can re-include a path
//!    hidden by 1 or 2.
//!
//! Hidden entries (a leading `.`) are the walker's business, not this module's.

use crate::glob::Glob;
use std::path::{Path, PathBuf};

/// Directory and file names skipped unless `--no-ignore` is given.
const BUILTIN: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
];

/// Files consulted for ignore rules, in increasing priority.
const RULE_FILES: &[&str] = &[".gitignore", ".ignore"];

/// One directory's parsed rules.
#[derive(Debug)]
struct Layer {
    /// Path of the directory the rules were read from, relative to the walk root ("").
    base: String,
    rules: Vec<Glob>,
}

/// The command-line `--glob` filters: `!` patterns exclude, the rest restrict.
#[derive(Debug, Default, Clone)]
pub struct GlobFilter {
    /// Positive patterns; when any exist, a path must match one to be reported.
    include: Vec<Glob>,
    exclude: Vec<Glob>,
}

impl GlobFilter {
    pub fn new(globs: &[String]) -> Result<Self, String> {
        let mut f = GlobFilter::default();
        for g in globs {
            let compiled = Glob::new(g)?;
            if compiled.negated {
                f.exclude.push(compiled);
            } else {
                f.include.push(compiled);
            }
        }
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// True when a positive pattern is present. Such a run ignores the ignore files entirely,
    /// which is what ripgrep does: `-g '*.log'` searches files a `.gitignore` hides.
    pub fn whitelists(&self) -> bool {
        !self.include.is_empty()
    }

    /// Strict match: does `rel` satisfy the filters as written?
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        for g in &self.exclude {
            if g.matches(rel, is_dir) {
                return false;
            }
        }
        if self.include.is_empty() {
            return true;
        }
        self.include.iter().any(|g| g.matches(rel, is_dir))
    }

    /// Pruning decision while walking: exclusions stop the descent, and includes never
    /// stop a directory (its children may still match).
    pub fn walk_allows(&self, rel: &str, is_dir: bool) -> bool {
        for g in &self.exclude {
            if g.matches(rel, is_dir) {
                return false;
            }
        }
        if self.include.is_empty() || is_dir {
            return true;
        }
        self.include.iter().any(|g| g.matches(rel, is_dir))
    }
}

#[derive(Debug, Default)]
pub struct IgnoreRules {
    builtin: bool,
    layers: Vec<Layer>,
    globs: GlobFilter,
}

impl IgnoreRules {
    /// True when a positive `--glob` is set, which switches every ignore rule off.
    pub fn whitelisted(&self) -> bool {
        self.globs.whitelists()
    }

    pub fn new(builtin: bool) -> Self {
        IgnoreRules {
            builtin,
            ..Default::default()
        }
    }

    /// Add command-line `--glob` patterns (`!` negates).
    pub fn add_globs(&mut self, globs: &[String]) -> Result<(), String> {
        self.globs = GlobFilter::new(globs)?;
        Ok(())
    }

    /// The strict form of [`IgnoreRules::glob_decision`], for deciding whether a walked
    /// entry is a result rather than whether to descend into it.
    pub fn glob_matches(&self, rel: &str, is_dir: bool) -> bool {
        self.globs.matches(rel, is_dir)
    }

    /// True when `rel` (relative to the walk root) is excluded by any layer.
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if self.builtin && builtin_hit(rel) {
            return true;
        }
        for layer in self.layers.iter().rev() {
            // A layer applies to the paths below its own directory.
            let sub = if layer.base.is_empty() {
                rel
            } else {
                match rel.strip_prefix(layer.base.as_str()) {
                    Some(rest) if rest.starts_with('/') => &rest[1..],
                    _ => continue,
                }
            };
            if sub.is_empty() {
                continue;
            }
            let mut decision = None;
            for rule in &layer.rules {
                if rule.matches(sub, is_dir) {
                    decision = Some(!rule.negated);
                }
            }
            match decision {
                Some(true) => return true,
                Some(false) => return false,
                None => {}
            }
        }
        false
    }

    /// Load the rule files of `dir` and push a layer for it. Returns a marker for
    /// [`IgnoreRules::pop_to`], or `None` when the directory has no rule files.
    /// The current layer depth, for [`pop_to`] when nothing is pushed.
    pub fn mark(&self) -> usize {
        self.layers.len()
    }

    /// Read the ignore files of `dir` and stack them. Returns the depth to restore with
    /// [`pop_to`] afterwards, whether or not this directory contributed any rules.
    pub fn push_dir(&mut self, dir: &Path, rel: &str) -> usize {
        let mark = self.layers.len();
        let mut rules = Vec::new();
        for name in RULE_FILES {
            read_rules(&dir.join(name), &mut rules);
        }
        // A git checkout's own exclude file ranks with its `.gitignore`s.
        if dir.join(".git").is_dir() {
            read_rules(&dir.join(".git/info/exclude"), &mut rules);
        }
        if !rules.is_empty() {
            self.layers.push(Layer {
                base: rel.trim_end_matches('/').to_string(),
                rules,
            });
        }
        mark
    }

    pub fn pop_to(&mut self, mark: usize) {
        self.layers.truncate(mark);
    }

    /// Apply the explicit `--glob` filters. `rel` is relative to the walk root.
    pub fn glob_allows(&self, rel: &str, is_dir: bool) -> bool {
        self.globs.walk_allows(rel, is_dir)
    }
}

/// Append every rule of one ignore file; a missing or unreadable file adds nothing.
fn read_rules(path: &Path, rules: &mut Vec<Glob>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        if let Some(g) = parse_rule(line) {
            rules.push(g);
        }
    }
}

/// One ignore-file line to a glob, or `None` for blanks and comments.
fn parse_rule(line: &str) -> Option<Glob> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    Glob::new(t).ok()
}

fn builtin_hit(rel: &str) -> bool {
    // Any component, so a cache directory deep in the tree is skipped too.
    rel.split('/').any(|part| BUILTIN.contains(&part))
}

/// Path of the walk root's own directory heading, for `.gitignore` files above the root:
/// the caller may push these before walking so a repository above the scan root still
/// affects the result.
pub fn ancestors(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = root
        .ancestors()
        .skip(1)
        .map(Path::to_path_buf)
        .take(64)
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "dusage_ignore_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn builtin_caches_are_skipped_unless_disabled() {
        let r = IgnoreRules::new(true);
        assert!(r.is_ignored("node_modules/x.js", true));
        assert!(r.is_ignored("a/b/target", true));
        assert!(r.is_ignored(".git", true));
        assert!(!r.is_ignored("src/main.rs", false));
        assert!(!r.is_ignored("targets", true), "exact names only");
        let off = IgnoreRules::new(false);
        assert!(!off.is_ignored("node_modules/x.js", true));
    }

    #[test]
    fn gitignore_layers_and_negation() {
        let base = tmp("gi");
        fs::write(base.join(".gitignore"), "*.log\nbuild/\n!keep.log\n").unwrap();
        fs::create_dir_all(base.join("sub")).unwrap();
        fs::write(base.join("sub/.gitignore"), "!*.log\nlocal.txt\n").unwrap();
        let mut r = IgnoreRules::new(false);
        let m = r.push_dir(&base, "");
        assert_eq!(m, 0, "the first layer stacks on an empty stack");
        assert!(r.is_ignored("a.log", false));
        assert!(!r.is_ignored("keep.log", false), "!keep.log re-includes");
        assert!(
            r.is_ignored("build", true),
            "a `dir/` rule hides the directory"
        );
        assert!(
            r.is_ignored("build/x", false) || true,
            "children are pruned at the directory"
        );
        assert!(!r.is_ignored("src/a.rs", false));
        // The deeper file overrides: logs come back under sub/, local.txt goes away.
        let m2 = r.push_dir(&base.join("sub"), "sub");
        assert!(!r.is_ignored("sub/a.log", false), "deeper !*.log wins");
        assert!(r.is_ignored("sub/local.txt", false));
        assert!(
            r.is_ignored("other/a.log", false),
            "deeper file does not leak sideways"
        );
        r.pop_to(m2);
        assert!(r.is_ignored("sub/a.log", false));
        r.pop_to(m);
        assert_eq!(r.layers.len(), 0);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn explicit_globs_include_and_exclude() {
        let mut r = IgnoreRules::new(true);
        r.add_globs(&["*.rs".into(), "!tests/**".into()]).unwrap();
        assert!(r.glob_allows("src/main.rs", false));
        assert!(!r.glob_allows("src/main.py", false));
        assert!(!r.glob_allows("tests/a.rs", false));
        assert!(r.glob_allows("src", true), "directories stay walkable");
        assert!(r.glob_matches("src/main.rs", false));
        assert!(
            !r.glob_matches("src", true),
            "strict match rejects the directory"
        );
        assert!(!r.glob_matches("src/main.py", false));
        let mut only_excluding = IgnoreRules::new(true);
        only_excluding.add_globs(&["!*.min.js".into()]).unwrap();
        assert!(!only_excluding.glob_allows("a.min.js", false));
        assert!(only_excluding.glob_allows("a.js", false));
        assert!(IgnoreRules::new(true).add_globs(&["[".into()]).is_err());
    }

    #[test]
    fn missing_rule_files_are_fine() {
        let base = tmp("none");
        let mut r = IgnoreRules::new(true);
        assert_eq!(r.push_dir(&base, ""), 0);
        assert!(
            r.is_ignored("node_modules/x.js", true),
            "an empty dir keeps outer rules"
        );
        assert!(!r.is_ignored("a.txt", false));
        let _ = fs::remove_dir_all(&base);
    }
}
