//! Protected-root guard for destructive tools.
//!
//! `is_protected_root` answers "must a destructive tool refuse this path?". A path is
//! protected when, after lexical normalisation (and canonicalisation when it exists), it is a
//! volume or share root (the filesystem root on Linux), one of the well-known system/profile
//! directories, or an ancestor of one of those. Descendants of the system directories (e.g.
//! `C:\Windows\Temp`, `/var/tmp`) are not protected here. Anything that cannot be
//! normalised is treated as protected.
//!
//! The two implementations share only the entry point and `PROTECTED_SUMMARY`.

#[cfg(windows)]
use std::fs;
#[cfg(windows)]
use std::path::Path;

/// A normalised path: lowercase root key plus resolved lowercase components.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(windows)]
struct Norm {
    root: String,
    comps: Vec<String>,
}

#[cfg(windows)]
fn is_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

#[cfg(windows)]
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(windows)]
fn resolve(rest: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in rest.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => {
                // Win32 strips trailing dots and spaces from path components.
                let t = c.trim_end_matches(['.', ' ']);
                if !t.is_empty() {
                    out.push(t.to_lowercase());
                }
            }
        }
    }
    out
}

/// True if a UNC server name refers to this machine (loopback names, the machine name and
/// its DNS name). Unknown aliases cannot be detected; their `$` shares are not remapped.
#[cfg(windows)]
fn is_local_server(server: &str) -> bool {
    let s = server.trim_matches(['[', ']']).to_lowercase();
    let env = |k: &str| std::env::var(k).ok().map(|v| v.to_lowercase());
    let host = env("COMPUTERNAME").filter(|h| !h.is_empty());
    s == "localhost"
        || s == "::1"
        || s == "."
        || s.starts_with("127.")
        || s.ends_with(".ipv6-literal.net")
        || host.as_ref().is_some_and(|h| {
            s == *h || env("USERDNSDOMAIN").is_some_and(|d| s == format!("{h}.{d}"))
        })
}

#[cfg(windows)]
fn unc(rest: &str) -> Option<Norm> {
    let mut it = rest.split('\\').filter(|c| !c.is_empty());
    let server = it.next()?;
    let share = it.next();
    let tail: Vec<&str> = it.collect();
    if let Some(sh) = share.filter(|sh| is_local_server(server) && sh.ends_with('$')) {
        // `\\localhost\C$\Windows` is `C:\Windows`: judge it as the drive it names. Other
        // administrative shares (ADMIN$, IPC$, ...) fail closed.
        let b = sh.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() {
            return Some(Norm {
                root: format!("{}:", sh[..1].to_lowercase()),
                comps: resolve(&tail.join("\\")),
            });
        }
        return Some(Norm {
            root: format!("\\\\{}\\{}", server.to_lowercase(), sh.to_lowercase()),
            comps: Vec::new(),
        });
    }
    let root = match share {
        Some(sh) => format!("\\\\{}\\{}", server.to_lowercase(), sh.to_lowercase()),
        None => format!("\\\\{}", server.to_lowercase()),
    };
    Some(Norm {
        root,
        comps: resolve(&tail.join("\\")),
    })
}

#[cfg(windows)]
fn normalize(orig: &str, may_absolutize: bool) -> Option<Norm> {
    let s = orig.replace('/', "\\");
    let mut body = s.as_str();
    let mut verbatim = false;
    for pre in [r"\\?\", r"\\.\", r"\??\"] {
        if body.starts_with(pre) {
            body = &body[pre.len()..];
            verbatim = true;
            break;
        }
    }
    if verbatim {
        if starts_with_ci(body, "UNC\\") {
            return unc(&body[4..]);
        }
        if is_drive_prefix(body) {
            return Some(Norm {
                root: body[..2].to_lowercase(),
                comps: resolve(&body[2..]),
            });
        }
        let mut it = body.splitn(2, '\\');
        let first = it.next().unwrap_or("");
        if first.is_empty() {
            return None;
        }
        let kind = if starts_with_ci(first, "Volume{") {
            "vol"
        } else {
            "dev"
        };
        return Some(Norm {
            root: format!("{kind}:{}", first.to_lowercase()),
            comps: resolve(it.next().unwrap_or("")),
        });
    }
    if let Some(rest) = body.strip_prefix("\\\\") {
        return unc(rest);
    }
    if is_drive_prefix(body) {
        let rest = &body[2..];
        if rest.is_empty() {
            // Bare "C:" is the current dir of drive C; refuse it as the volume root.
            return Some(Norm {
                root: body.to_lowercase(),
                comps: Vec::new(),
            });
        }
        if rest.starts_with('\\') {
            return Some(Norm {
                root: body[..2].to_lowercase(),
                comps: resolve(rest),
            });
        }
    }
    // Relative, rooted-without-drive, or drive-relative: anchor with the OS, once.
    if !may_absolutize || body.is_empty() {
        return None;
    }
    let abs = std::path::absolute(orig).ok()?;
    normalize(abs.to_str()?, false)
}

#[cfg(windows)]
fn make_set(paths: &[String]) -> Vec<Norm> {
    let mut set = Vec::new();
    for p in paths {
        if let Some(n) = normalize(p, true) {
            set.push(n);
        }
        if let Ok(c) = fs::canonicalize(p) {
            if let Some(n) = c.to_str().and_then(|s| normalize(s, true)) {
                set.push(n);
            }
        }
    }
    set
}

/// Directories protected on this machine, read from the environment at call time.
#[cfg(windows)]
fn protected_paths() -> Vec<String> {
    let env = |k: &str| {
        std::env::var_os(k)
            .and_then(|v| v.into_string().ok())
            .filter(|v| !v.is_empty())
    };
    let mut v = Vec::new();
    for k in [
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "USERPROFILE",
    ] {
        if let Some(x) = env(k) {
            v.push(x);
        }
    }
    let sd = env("SystemDrive").unwrap_or_else(|| "C:".into());
    let sd = sd.trim_end_matches(['\\', '/']);
    for tail in [
        "Users",
        "Windows",
        "Program Files",
        "Program Files (x86)",
        "ProgramData",
    ] {
        v.push(format!("{sd}\\{tail}"));
    }
    v
}

#[cfg(windows)]
fn hits(n: &Norm, set: &[Norm]) -> bool {
    n.comps.is_empty()
        || set
            .iter()
            .any(|p| p.root == n.root && p.comps.starts_with(&n.comps))
}

#[cfg(windows)]
fn check(p: &Path, set: &[Norm]) -> bool {
    let Some(s) = p.to_str() else {
        return true; // not representable: refuse
    };
    if s.trim().is_empty() {
        return true;
    }
    match normalize(s, true) {
        None => return true,
        Some(n) if hits(&n, set) => return true,
        Some(_) => {}
    }
    // Junctions / symlinks / subst: judge the real target too.
    if let Ok(c) = fs::canonicalize(p) {
        return match c.to_str().and_then(|s| normalize(s, true)) {
            None => true,
            Some(n) => hits(&n, set),
        };
    }
    false
}

/// What `is_protected_root` refuses, for the message a tool prints on refusal.
#[cfg(windows)]
pub const PROTECTED_SUMMARY: &str =
    "volume roots, Windows, Program Files, ProgramData, user profile and their ancestors";

/// True if a destructive tool must refuse `p`: any volume/share root in any spelling,
/// `%SystemRoot%`, `%ProgramFiles%`, `%ProgramFiles(x86)%`, `%ProgramData%`, `%USERPROFILE%`,
/// `C:\Users`, and any ancestor of those. Case-insensitive; `.`/`..` and trailing
/// separators are resolved; existing paths are also canonicalised.
#[cfg(windows)]
pub fn is_protected_root(p: &Path) -> bool {
    check(p, &make_set(&protected_paths()))
}

#[cfg(target_os = "linux")]
pub use linux::is_protected_root;

/// What `is_protected_root` refuses, for the message a tool prints on refusal.
#[cfg(target_os = "linux")]
pub const PROTECTED_SUMMARY: &str =
    "the filesystem root, /etc, /usr, /bin, /sbin, /lib*, /boot, /proc, /sys, /dev, /run, \
     /var, /opt, /srv, /root, /home, $HOME and their ancestors";

/// Protection for destructive tools on Linux.
///
/// A path is protected when, after lexical normalisation (and canonicalisation when it
/// exists, which also resolves symlinks), it is the filesystem root, one of the system
/// directories, a home directory, or an ancestor of one of those. Descendants of a
/// system directory (e.g. `/var/tmp`) are not protected. Anything that cannot be
/// normalised is treated as protected.
///
/// Mount points are not special-cased here: the tool refuses to descend into one while
/// deleting, so only a root the user typed explicitly can be a mount.
#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Component, Path, PathBuf};

    /// Absolute, no `.`/`..`, no trailing separator, resolved without touching the disk.
    fn lexical(p: &Path) -> Option<PathBuf> {
        let abs = std::path::absolute(p).ok()?;
        let mut out = PathBuf::from("/");
        for c in abs.components() {
            match c {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(s) => out.push(s),
                Component::Prefix(_) => return None,
            }
        }
        Some(out)
    }

    /// Immutable system trees: nothing below these is ever scratch space, so the whole
    /// subtree is refused (`/usr/bin`, `/etc/nginx`), like `C:\Windows` on Windows.
    fn system_trees() -> Vec<PathBuf> {
        [
            "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/boot",
            "/proc", "/sys", "/dev", "/run",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }

    /// Directories that hold user or service data: the directory itself and anything above
    /// it are refused, but their children (`/var/tmp`, `/opt/app/cache`) are prunable, the
    /// way `%ProgramFiles%\App` is on Windows.
    fn data_dirs() -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = ["/var", "/opt", "/srv", "/root", "/home"]
            .iter()
            .map(PathBuf::from)
            .collect();
        if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            if let Some(p) = lexical(Path::new(&h)) {
                v.push(p);
            }
        }
        v
    }

    pub fn is_protected_root(p: &Path) -> bool {
        if p.to_string_lossy().trim().is_empty() {
            return true;
        }
        let Some(abs) = lexical(p) else {
            return true;
        };
        if abs == Path::new("/") {
            return true;
        }
        let real = fs::canonicalize(p)
            .ok()
            .and_then(|c| lexical(&c))
            .unwrap_or(abs);
        system_trees().iter().any(|s| real.starts_with(s))
            || data_dirs().iter().any(|s| s.starts_with(&real))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::path::Path;

    fn prot(s: &str) -> bool {
        is_protected_root(Path::new(s))
    }

    #[test]
    fn root_and_system_trees_are_protected() {
        for s in [
            "/",
            "//",
            "/.",
            "/etc",
            "/etc/",
            "//etc",
            "/usr",
            "/usr/bin",
            "/usr/../etc",
            "/boot",
            "/boot/efi",
            "/opt",
            "/srv",
            "/root",
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/bin",
            "/lib64",
            "/home",
            "/var",
            "/etc/./passwd/..",
        ] {
            assert!(prot(s), "{s:?} should be protected");
        }
    }

    #[test]
    fn ordinary_dirs_and_data_children_are_prunable() {
        assert!(!prot("/tmp"));
        assert!(!prot("/tmp/x"));
        assert!(!prot("/var/tmp"));
        assert!(!prot("/var/log/journal"));
        assert!(!prot("/opt/app"));
        assert!(!prot("/workspace/proj"));
        assert!(!prot("/usrx"));
        assert!(!prot("/etcx/y"));
        assert!(!prot("some-relative-dir"));
        // A relative path climbing far enough lands on the filesystem root.
        assert!(prot(&(0..40).map(|_| "..").collect::<Vec<_>>().join("/")));
    }

    #[test]
    fn home_directories_are_protected() {
        if let Some(h) = std::env::var_os("HOME") {
            let home = Path::new(&h);
            assert!(is_protected_root(home), "HOME itself");
            if let Some(p) = home.parent() {
                assert!(is_protected_root(p), "an ancestor of HOME");
            }
            assert!(!is_protected_root(&home.join("scratch/x")), "inside HOME");
        }
    }

    #[test]
    fn empty_is_protected() {
        assert!(prot(""));
        assert!(prot("   "));
    }

    #[test]
    fn symlink_to_protected_dir_is_caught() {
        let base = std::env::temp_dir().join(format!("dusage-guard-lx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let link = base.join("lnk");
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        assert!(is_protected_root(&link), "symlink to /etc");
        let safe = base.join("safe");
        std::os::unix::fs::symlink(base.join("data"), &safe).unwrap();
        assert!(!is_protected_root(&safe));
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn test_set() -> Vec<Norm> {
        // Non-existent dirs on purpose: keeps the test independent of the machine.
        [
            r"C:\Windows",
            r"C:\Users\alice",
            r"C:\Users",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ]
        .iter()
        .filter_map(|s| normalize(s, true))
        .collect()
    }

    fn prot(s: &str) -> bool {
        check(Path::new(s), &test_set())
    }

    #[test]
    fn volume_root_spellings() {
        for s in [
            "C:",
            "c:",
            "C:\\",
            "c:/",
            "C:/",
            "C:\\.",
            "C:\\.\\",
            "C:\\\\",
            r"\\?\C:\\",
            r"\\?\C:",
            r"\\.\C:",
            r"\\.\C:\\",
            r"\??\C:\\",
            "D:",
            "D:\\",
            r"\\?\d:\\",
            "C:\\Users\\x\\..\\..",
            "C:\\Users\\x\\..\\..\\",
            "C:\\a\\..",
            "C:\\..",
            "C:\\..\\..",
            r"C:\Windows\..",
            "C:\\ ",
            "C:\\.. ",
        ] {
            assert!(prot(s), "{s:?} should be protected");
        }
    }

    #[test]
    fn share_roots() {
        for s in [
            r"\\server\share",
            r"\\server\share\\",
            r"\\SERVER\Share\",
            r"\\server",
            r"\\?\UNC\server\share",
            r"\\?\UNC\server\share\\",
            r"\\?\unc\server\share\x\..",
            r"\\.\UNC\server\share\",
            "//server/share",
            r"\\server\share\a\..",
        ] {
            assert!(prot(s), "{s:?} should be protected");
        }
        assert!(!prot(r"\\server\share\dir"));
        assert!(!prot(r"\\?\UNC\server\share\dir\sub"));
    }

    #[test]
    fn loopback_admin_shares_map_to_local_drive() {
        for s in [
            r"\\localhost\C$\Windows",
            r"\\LOCALHOST\c$\windows\",
            r"\\127.0.0.1\C$\Users",
            r"\\127.0.0.1\C$\Users\alice",
            r"\\[::1]\C$\Users",
            r"\\localhost\C$",
            r"\\localhost\C$\",
            r"\\?\UNC\localhost\C$\Windows",
            r"\\localhost\ADMIN$",
            r"\\localhost\ADMIN$\System32",
            r"\\localhost\IPC$",
            r"\\localhost\C$\Users\x\..",
        ] {
            assert!(prot(s), "{s:?} should be protected");
        }
        if let Ok(h) = std::env::var("COMPUTERNAME") {
            assert!(prot(&format!(r"\\{h}\C$\Users\alice")), "machine name");
        }
        assert!(!prot(r"\\localhost\C$\devcache\x"));
        assert!(!prot(r"\\localhost\D$\Projects\x"));
        assert!(!prot(r"\\localhost\C$\Windows\Temp"));
    }

    #[test]
    fn volume_guid_root() {
        assert!(prot(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\"));
        assert!(prot(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}"));
        assert!(!prot(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\x"));
    }

    #[test]
    fn system_dirs_case_insensitive() {
        for s in [
            r"C:\Windows",
            r"c:\WINDOWS\",
            r"c:/windows/",
            r"\\?\C:\Windows",
            r"C:\Windows\.",
            r"C:\Windows\System32\..",
            r"C:\Windows.",
            r"C:\Program Files",
            r"c:\program files (x86)",
            r"C:\ProgramData",
            r"C:\Users",
            r"C:\Users\",
            r"C:\Users\alice",
            r"C:\users\ALICE\",
            r"C:\Users\alice\Documents\..",
            r"C:\Users\x\..\alice",
        ] {
            assert!(prot(s), "{s:?} should be protected");
        }
    }

    #[test]
    fn not_protected() {
        for s in [
            r"C:\devcache\tool-tmp",
            r"C:\devcache",
            r"D:\work\x",
            r"D:\work",
            r"C:\Users\Other",
            r"C:\Users\alice\AppData",
            r"C:\Windows\Temp",
            r"C:\Program Files\App",
            r"C:\ProgramDataX",
            r"\\?\C:\devcache\tool-tmp",
            r"C:\a\b\..\c",
        ] {
            assert!(!prot(s), "{s:?} should not be protected");
        }
    }

    #[test]
    fn empty_is_protected() {
        assert!(prot(""));
        assert!(prot("  "));
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        // Climbing far enough from the cwd lands on the volume root.
        assert!(prot(r"..\..\..\..\..\..\..\..\..\..\..\..\..\..\.."));
        assert!(!prot("some-relative-dir"));
    }

    #[test]
    fn env_vars_read_at_call_time() {
        for k in ["SystemRoot", "ProgramFiles", "ProgramData", "USERPROFILE"] {
            if let Some(v) = std::env::var_os(k) {
                assert!(is_protected_root(Path::new(&v)), "{k} should be protected");
            }
        }
        assert!(is_protected_root(Path::new(r"C:\")));
        assert!(is_protected_root(Path::new(r"C:")));
        assert!(is_protected_root(Path::new(r"C:\Users")));
        assert!(!is_protected_root(Path::new(r"D:\work\x")));
        assert!(!is_protected_root(Path::new(r"C:\devcache\tool-tmp")));
        if let Some(v) = std::env::var_os("USERPROFILE") {
            // An ancestor of the profile is protected too.
            if let Some(parent) = Path::new(&v).parent() {
                assert!(is_protected_root(parent));
            }
        }
    }

    #[test]
    fn junction_to_protected_dir_is_caught() {
        let Some(win) = std::env::var_os("SystemRoot") else {
            return;
        };
        let base = std::env::temp_dir().join(format!("dusage-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let link = base.join("lnk");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&win)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            assert!(
                is_protected_root(&link),
                "junction to SystemRoot must be protected"
            );
            // Remove the junction itself (not the target) before cleaning up.
            let _ = fs::remove_dir(&link);
        } else {
            eprintln!("skipped: could not create junction");
        }
        // Only remove the (now empty) fixture dir; never recurse in case the junction remains.
        let _ = fs::remove_dir(&base);
    }
}
