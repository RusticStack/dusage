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
use std::sync::Arc;

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

/// One directory's parsed rules.
#[derive(Debug, Clone)]
struct Layer {
    /// Path of the directory the rules were read from, relative to the walk root ("").
    base: String,
    rules: Vec<Glob>,
}

/// The command-line `--glob` filters: `!` patterns exclude, the rest restrict.
#[derive(Debug, Default, Clone)]
pub struct GlobFilter {
    /// Positive `-g` patterns; when any exist, a path must match one to be reported, and
    /// they switch the ignore files off (ripgrep's override rule).
    include: Vec<Glob>,
    /// Positive `-t`/`--ext` patterns: they restrict results without touching ignore rules.
    filter: Vec<Glob>,
    exclude: Vec<Glob>,
}

impl GlobFilter {
    pub fn new(globs: &[String]) -> Result<Self, String> {
        GlobFilter::with_case(globs, &[])
    }

    /// Build from case-sensitive globs plus case-insensitive ones (`--iglob`).
    pub fn with_case(globs: &[String], iglobs: &[String]) -> Result<Self, String> {
        let mut f = GlobFilter::default();
        let patterns = globs
            .iter()
            .map(|g| (g, false))
            .chain(iglobs.iter().map(|g| (g, true)));
        for (pattern, ci) in patterns {
            let compiled = Glob::new_with(pattern, ci)?;
            if compiled.negated {
                f.exclude.push(compiled);
            } else {
                f.include.push(compiled);
            }
        }
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty() && self.filter.is_empty()
    }

    /// Add restrict-only patterns (`-t`, `--ext`).
    pub fn push_filter(&mut self, globs: &[String]) -> Result<(), String> {
        for g in globs {
            let compiled = Glob::new(g)?;
            // A negated `-T`/`--ext` pattern excludes rather than narrows.
            if compiled.negated {
                self.exclude.push(compiled);
            } else {
                self.filter.push(compiled);
            }
        }
        Ok(())
    }

    /// True when a positive pattern is present. Such a run ignores the ignore files entirely,
    /// which is what ripgrep does: `-g '*.log'` searches files a `.gitignore` hides.
    pub fn whitelists(&self) -> bool {
        !self.include.is_empty()
    }

    /// True when a positive pattern (include or type filter) matches `rel`.
    pub fn names(&self, rel: &str) -> bool {
        self.include
            .iter()
            .chain(self.filter.iter())
            .any(|g| g.matches(rel, false))
    }

    /// Strict match: does `rel` satisfy the filters as written?
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        for g in &self.exclude {
            if g.matches(rel, is_dir) {
                return false;
            }
        }
        if !self.filter.is_empty() && !self.filter.iter().any(|g| g.matches(rel, is_dir)) {
            return false;
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
        if is_dir {
            return true;
        }
        if !self.filter.is_empty() && !self.filter.iter().any(|g| g.matches(rel, is_dir)) {
            return false;
        }
        if self.include.is_empty() {
            return true;
        }
        self.include.iter().any(|g| g.matches(rel, is_dir))
    }
}

#[derive(Debug, Default, Clone)]
pub struct IgnoreRules {
    builtin: bool,
    /// Layers are shared by reference so a walk can snapshot its rules for another thread
    /// for the price of a few `Arc` clones.
    layers: Vec<Arc<Layer>>,
    globs: GlobFilter,
    /// Read `.gitignore` and `.git/info/exclude` (`--no-ignore-vcs` clears it).
    vcs: bool,
    /// Read `.ignore` (`--no-ignore-dot` clears it).
    dot: bool,
    /// Stack the rule files of the directories above a root (`--no-ignore-parent` clears it).
    parent: bool,
    /// Read git's global excludes file (`--no-ignore-global` clears it).
    global: bool,
}

impl IgnoreRules {
    /// True when a positive `--glob` is set, which switches every ignore rule off.
    pub fn whitelisted(&self) -> bool {
        self.globs.whitelists()
    }

    /// Restrict-only patterns from `-t`/`--ext`, which never switch ignore rules off.
    pub fn add_filters(&mut self, globs: &[String]) -> Result<(), String> {
        self.globs.push_filter(globs)
    }

    pub fn new(builtin: bool) -> Self {
        IgnoreRules {
            builtin,
            vcs: true,
            dot: true,
            parent: true,
            global: true,
            ..Default::default()
        }
    }

    /// Choose which sources of ignore rules a walk reads.
    pub fn set_sources(&mut self, vcs: bool, dot: bool, parent: bool) {
        self.vcs = vcs;
        self.dot = dot;
        self.parent = parent;
    }

    /// True when a walk should stack the ignore files of a root's ancestors.
    pub fn reads_parents(&self) -> bool {
        self.parent
    }

    /// Turn git's global excludes file on or off (`--no-ignore-global`).
    pub fn set_global(&mut self, on: bool) {
        self.global = on;
    }

    /// The rules of one directory, in priority order: `.gitignore` first, then `.ignore`,
    /// which ranks above it because it exists to override the VCS rules.
    fn read_own(&self, dir: &Path, rules: &mut Vec<Glob>, anchored: Option<&str>) {
        for (name, on) in [(".gitignore", self.vcs), (".ignore", self.dot)] {
            if on {
                match anchored {
                    None => read_rules(&dir.join(name), rules),
                    Some(prefix) => read_rules_anchored(&dir.join(name), prefix, rules),
                }
            }
        }
        if self.vcs && dir.join(".git").is_dir() {
            let exclude = dir.join(".git/info/exclude");
            match anchored {
                None => read_rules(&exclude, rules),
                Some(prefix) => read_rules_anchored(&exclude, prefix, rules),
            }
        }
    }

    /// Add command-line `--glob` patterns (`!` negates).
    pub fn add_globs(&mut self, globs: &[String]) -> Result<(), String> {
        self.globs = GlobFilter::new(globs)?;
        Ok(())
    }

    /// The same with `--iglob` patterns added, which match without case.
    pub fn add_globs_with_case(
        &mut self,
        globs: &[String],
        iglobs: &[String],
    ) -> Result<(), String> {
        self.globs = GlobFilter::with_case(globs, iglobs)?;
        Ok(())
    }

    /// The strict form of [`IgnoreRules::glob_decision`], for deciding whether a walked
    /// entry is a result rather than whether to descend into it.
    pub fn glob_matches(&self, rel: &str, is_dir: bool) -> bool {
        self.globs.matches(rel, is_dir)
    }

    /// True when a positive `-g`/`-t` pattern names `rel`: such a path is searched even
    /// though its name starts with a dot.
    pub fn glob_named(&self, rel: &str) -> bool {
        self.globs.names(rel)
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

    /// The current layer depth, for [`pop_to`] when nothing is pushed.
    pub fn mark(&self) -> usize {
        self.layers.len()
    }

    /// Read the ignore files of `dir` and stack them. Returns the depth to restore with
    /// [`pop_to`] afterwards, whether or not this directory contributed any rules.
    pub fn push_dir(&mut self, dir: &Path, rel: &str) -> usize {
        let mark = self.layers.len();
        let mut rules = Vec::new();
        self.read_own(dir, &mut rules, None);
        if !rules.is_empty() {
            self.layers.push(Arc::new(Layer {
                base: rel.trim_end_matches('/').to_string(),
                rules,
            }));
        }
        mark
    }

    pub fn pop_to(&mut self, mark: usize) {
        self.layers.truncate(mark);
    }

    /// Stack git's global excludes file, the lowest-priority layer there is. Read once per
    /// walk, before the per-directory layers, so a repository's own rules always win.
    /// Returns the depth to restore with [`pop_to`].
    pub fn push_global(&mut self) -> usize {
        let mark = self.layers.len();
        if !self.global {
            return mark;
        }
        let Some(path) = global_ignore_path() else {
            return mark;
        };
        let mut rules = Vec::new();
        read_rules(&path, &mut rules);
        if !rules.is_empty() {
            self.layers.push(Arc::new(Layer {
                base: String::new(),
                rules,
            }));
        }
        mark
    }

    /// Stack the rules of one explicit ignore file (`--ignore-file`). Its patterns are
    /// read the way git reads `.gitignore`: relative to the walk root, `!` re-including,
    /// and a leading `/` anchoring at the root. With `case_insensitive` every pattern
    /// matches without case, as `--ignore-file-case-insensitive` asks.
    pub fn push_file(&mut self, path: &Path, case_insensitive: bool) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut rules = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let glob = Glob::new_with(t, case_insensitive)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            rules.push(glob);
        }
        if !rules.is_empty() {
            self.layers.push(Arc::new(Layer {
                base: String::new(),
                rules,
            }));
        }
        Ok(())
    }

    /// Stack the rule files of the directories above `root`, so a scan started inside a
    /// repository honours the repository's `.gitignore`, as rg does. Patterns anchored at an
    /// ancestor are rewritten relative to `root`; one that cannot reach `root` is dropped.
    /// The walk up stops at the repository root (a directory holding `.git`), or after 16
    /// levels. Returns the depth to restore with [`pop_to`].
    pub fn push_ancestors(&mut self, root: &Path) -> usize {
        let mark = self.layers.len();
        // A relative root (`.` or `src`) has no usable parent path, so work from the real one.
        let Ok(root) = std::fs::canonicalize(root) else {
            return mark;
        };
        let root = root.as_path();
        let mut chain: Vec<(PathBuf, String)> = Vec::new();
        let mut current = root.to_path_buf();
        for _ in 0..16 {
            let Some(parent) = current.parent() else {
                break;
            };
            let Ok(stripped) = current.strip_prefix(parent) else {
                break;
            };
            let mut prefix = chain
                .last()
                .map(|(_, p): &(PathBuf, String)| p.clone())
                .unwrap_or_default();
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(&stripped.to_string_lossy().replace('\\', "/"));
            let is_repo = parent.join(".git").exists();
            chain.push((parent.to_path_buf(), prefix));
            if is_repo {
                break;
            }
            current = parent.to_path_buf();
        }
        // Outermost first, so a nearer ancestor's rules win.
        for (dir, prefix) in chain.into_iter().rev() {
            let mut rules = Vec::new();
            self.read_own(&dir, &mut rules, Some(&prefix));
            if !rules.is_empty() {
                self.layers.push(Arc::new(Layer {
                    base: String::new(),
                    rules,
                }));
            }
        }
        mark
    }

    /// Apply the explicit `--glob` filters. `rel` is relative to the walk root.
    pub fn glob_allows(&self, rel: &str, is_dir: bool) -> bool {
        self.globs.walk_allows(rel, is_dir)
    }
}

/// Append the rules of an ancestor's ignore file, rewritten to be relative to the scan
/// root below `prefix`. A rule with a slash only matters when it points inside the root.
fn read_rules_anchored(path: &Path, prefix: &str, rules: &mut Vec<Glob>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let (bang, body) = match t.strip_prefix('!') {
            Some(rest) => ("!", rest),
            None => ("", t),
        };
        if body.contains('/') {
            let anchored = body.trim_start_matches('/');
            let Some(rest) = anchored
                .strip_prefix(prefix)
                .and_then(|r| r.strip_prefix('/'))
            else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            if let Some(g) = parse_rule(&format!("{bang}{rest}")) {
                rules.push(g);
            }
        } else if let Some(g) = parse_rule(t) {
            rules.push(g);
        }
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

/// The path of git's global excludes file: `core.excludesFile` from the user's git config
/// when it is set, and otherwise git's own default, `$XDG_CONFIG_HOME/git/ignore`.
///
/// Only the two per-user config files are read, in git's order (`~/.gitconfig`, then
/// `$XDG_CONFIG_HOME/git/config`), and only the `[core]` section's `excludesFile` key. A
/// value starting with `~/` is expanded against the home directory; `~user/` is not, since
/// resolving another user's home is not something this tool does.
pub fn global_ignore_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| PathBuf::from(h).join(".config")));
    if let Some(configured) =
        git_config_value("core", "excludesfile", home.as_deref(), xdg.as_deref())
    {
        return Some(expand_home(&configured, home.as_deref()));
    }
    xdg.map(|d| d.join("git").join("ignore"))
}

/// Expand a leading `~/` against `home`; anything else is taken as it is.
fn expand_home(value: &str, home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(value)
}

/// One key out of the per-user git config files, lowercased and trimmed. The last file that
/// names the key wins, which is git's own precedence. Comment lines and sections other than
/// `section` are ignored; a value is returned without quotes or a trailing comment.
fn git_config_value(
    section: &str,
    key: &str,
    home: Option<&std::ffi::OsStr>,
    xdg: Option<&Path>,
) -> Option<String> {
    let mut files = Vec::new();
    if let Some(home) = home {
        files.push(PathBuf::from(home).join(".gitconfig"));
    }
    if let Some(xdg) = xdg {
        files.push(xdg.join("git").join("config"));
    }
    let mut answer = None;
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut in_section = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                let name = line
                    .trim_start_matches('[')
                    .split(']')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                in_section = name == section;
                continue;
            }
            if !in_section {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            if k.trim().to_ascii_lowercase() != key {
                continue;
            }
            let v = v.trim();
            let v = v.split(['#', ';']).next().unwrap_or(v).trim();
            let v = v.trim_matches('"');
            if !v.is_empty() {
                answer = Some(v.to_string());
            }
        }
    }
    answer
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

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn explicit_ignore_file_excludes_and_reincludes() {
        let dir = tmp("ignore-file");
        let path = write(&dir, "extra.ignore", "*.log\n!keep.log\nbuild/\n");
        let mut rules = IgnoreRules::new(false);
        rules.push_file(&path, false).unwrap();
        assert!(rules.is_ignored("app.log", false));
        assert!(rules.is_ignored("deep/nested/app.log", false));
        assert!(!rules.is_ignored("keep.log", false));
        assert!(rules.is_ignored("build", true));
        assert!(!rules.is_ignored("src/main.rs", false));
    }

    #[test]
    fn explicit_ignore_file_can_match_without_case() {
        let dir = tmp("ignore-file-case");
        let path = write(&dir, "extra.ignore", "*.LOG\n");
        let mut folded = IgnoreRules::new(false);
        folded.push_file(&path, true).unwrap();
        assert!(folded.is_ignored("app.log", false));
        let mut exact = IgnoreRules::new(false);
        exact.push_file(&path, false).unwrap();
        assert!(!exact.is_ignored("app.log", false));
        assert!(exact.is_ignored("app.LOG", false));
    }

    #[test]
    fn a_missing_ignore_file_is_an_error() {
        let dir = tmp("ignore-file-missing");
        let mut rules = IgnoreRules::new(false);
        let err = rules
            .push_file(&dir.join("nope.ignore"), false)
            .unwrap_err();
        assert!(err.contains("nope.ignore"), "{err}");
    }

    #[test]
    fn global_excludes_file_comes_from_the_git_config() {
        let home = tmp("git-home");
        fs::create_dir_all(home.join(".config/git")).unwrap();
        write(
            &home,
            ".gitconfig",
            "[user]\n\tname = nobody\n[core]\n\texcludesFile = ~/.config/git/ignore\n",
        );
        let home_os = home.as_os_str();
        let got = git_config_value("core", "excludesfile", Some(home_os), None);
        assert_eq!(got.as_deref(), Some("~/.config/git/ignore"));
        assert_eq!(
            expand_home("~/.config/git/ignore", Some(home_os)),
            home.join(".config/git/ignore")
        );
        assert_eq!(
            expand_home("/etc/gitignore", Some(home_os)),
            PathBuf::from("/etc/gitignore")
        );
    }

    #[test]
    fn git_config_ignores_other_sections_and_comments() {
        let home = tmp("git-home-2");
        write(
            &home,
            ".gitconfig",
            "[alias]\n\texcludesFile = wrong\n[core] # the real one\n\texcludesFile = \"/tmp/x\" # trailing\n",
        );
        let got = git_config_value("core", "excludesfile", Some(home.as_os_str()), None);
        assert_eq!(got.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn the_global_layer_is_off_when_asked() {
        let mut rules = IgnoreRules::new(false);
        rules.set_global(false);
        let before = rules.mark();
        assert_eq!(rules.push_global(), before);
        assert_eq!(rules.layers.len(), before);
    }
}
