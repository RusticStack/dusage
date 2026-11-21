//! `--hyperlink-format`: the OSC 8 terminal hyperlink a printed path or line number is
//! wrapped in. The format is ripgrep's: a URL with `{path}`, `{host}`, `{line}`, `{column}`
//! and `{wslprefix}` in it, or the name of a built-in one.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Path,
    Host,
    Line,
    Column,
    WslPrefix,
}

/// A parsed format, with the two values that are the same for every link already looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkFormat {
    parts: Vec<Part>,
    host: String,
    wsl_prefix: String,
}

/// The built-in formats, by the names ripgrep gives them.
const ALIASES: &[(&str, &str)] = &[
    ("cursor", "cursor://file{path}:{line}:{column}"),
    #[cfg(not(windows))]
    ("default", "file://{host}{path}"),
    #[cfg(windows)]
    ("default", "file://{path}"),
    ("file", "file://{host}{path}"),
    ("grep+", "grep+://{path}:{line}"),
    ("kitty", "file://{host}{path}#{line}"),
    (
        "macvim",
        "mvim://open?url=file://{path}&line={line}&column={column}",
    ),
    ("none", ""),
    (
        "textmate",
        "txmt://open?url=file://{path}&line={line}&column={column}",
    ),
    ("vscode", "vscode://file{path}:{line}:{column}"),
    (
        "vscode-insiders",
        "vscode-insiders://file{path}:{line}:{column}",
    ),
    ("vscodium", "vscodium://file{path}:{line}:{column}"),
];

impl HyperlinkFormat {
    /// Parse a format or an alias. `Ok(None)` is the empty format, which prints no links.
    pub fn parse(text: &str) -> Result<Option<HyperlinkFormat>, String> {
        let text = match ALIASES.iter().find(|(name, _)| *name == text) {
            Some((_, format)) => format,
            None => text,
        };
        if text.is_empty() {
            return Ok(None);
        }
        let scheme = text.split(':').next().unwrap_or("");
        let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.');
        if !text.contains(':')
            || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            || !scheme.chars().all(valid)
        {
            return Err(format!(
                "hyperlink format {text:?} must start with a URL scheme, like file://"
            ));
        }
        let mut parts = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_string()));
            }
            let close = rest[open..]
                .find('}')
                .ok_or_else(|| format!("hyperlink format {text:?} has an unclosed '{{'"))?;
            parts.push(match &rest[open + 1..open + close] {
                "path" => Part::Path,
                "host" => Part::Host,
                "line" => Part::Line,
                "column" => Part::Column,
                "wslprefix" => Part::WslPrefix,
                other => {
                    return Err(format!(
                        "hyperlink format {text:?} has an unknown variable {{{other}}} (path, host, line, column, wslprefix)"
                    ))
                }
            });
            rest = &rest[open + close + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        if !parts.contains(&Part::Path) {
            return Err(format!("hyperlink format {text:?} needs a {{path}}"));
        }
        Ok(Some(HyperlinkFormat {
            parts,
            host: host_name(),
            wsl_prefix: std::env::var("WSL_DISTRO_NAME")
                .map(|distro| format!("wsl$/{distro}"))
                .unwrap_or_default(),
        }))
    }

    /// The URL for `path`, or `None` when the path cannot be made absolute. `line` and
    /// `column` default to 1, as they do in ripgrep, for a link that is about the whole file.
    pub fn url(&self, path: &str, line: Option<u64>, column: Option<u64>) -> Option<String> {
        let abs = std::path::absolute(Path::new(path)).ok()?;
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Path => encode_path(&abs, &mut out),
                Part::Host => out.push_str(&self.host),
                Part::Line => out.push_str(&line.unwrap_or(1).to_string()),
                Part::Column => out.push_str(&column.unwrap_or(1).to_string()),
                Part::WslPrefix => out.push_str(&self.wsl_prefix),
            }
        }
        Some(out)
    }
}

/// The machine's name without a system call: std has none for it and FFI stays in the
/// platform modules. An unknown host is empty, which `file://{host}{path}` reads as local.
fn host_name() -> String {
    let named = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
    named("COMPUTERNAME")
        .or_else(|| named("HOSTNAME"))
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

/// An absolute path as a URL path: `/` separators, a leading `/` before a drive letter, no
/// verbatim prefix, and every byte outside the unreserved set percent-encoded.
fn encode_path(path: &Path, out: &mut String) {
    let text = path.to_string_lossy();
    let mut text = text.as_ref();
    if cfg!(windows) {
        text = text.strip_prefix(r"\\?\").unwrap_or(text);
        if text.as_bytes().get(1) == Some(&b':') {
            out.push('/');
        } else if let Some(unc) = text.strip_prefix(r"\\") {
            // A UNC path becomes `//server/share`, which after `file:` names the host.
            out.push_str("//");
            text = unc;
        }
    }
    for &b in text.as_bytes() {
        match b {
            b'\\' if cfg!(windows) => out.push('/'),
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'/' | b':' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_validation() {
        assert_eq!(HyperlinkFormat::parse(""), Ok(None));
        assert_eq!(HyperlinkFormat::parse("none"), Ok(None));
        for (name, _) in ALIASES.iter().filter(|(name, _)| *name != "none") {
            assert!(HyperlinkFormat::parse(name).unwrap().is_some(), "{name}");
        }
        for (bad, why) in [
            ("{path}", "scheme"),
            ("1a://{path}", "scheme"),
            ("file://{host}", "{path}"),
            ("file://{path", "unclosed"),
            ("file://{path}{nope}", "unknown variable"),
        ] {
            let err = HyperlinkFormat::parse(bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn url_fills_every_variable() {
        let f = HyperlinkFormat {
            host: "box".to_string(),
            wsl_prefix: "wsl$/Ubuntu".to_string(),
            ..HyperlinkFormat::parse("x://{host}/{wslprefix}{path}?l={line}&c={column}")
                .unwrap()
                .unwrap()
        };
        let abs = if cfg!(windows) {
            r"C:\a dir\é.txt"
        } else {
            "/a dir/é.txt"
        };
        let lead = if cfg!(windows) { "/C:" } else { "" };
        assert_eq!(
            f.url(abs, Some(7), Some(3)).unwrap(),
            format!("x://box/wsl$/Ubuntu{lead}/a%20dir/%C3%A9.txt?l=7&c=3")
        );
        assert!(f.url(abs, None, None).unwrap().ends_with("?l=1&c=1"));
        // A relative path is resolved against the working directory.
        let cwd = std::env::current_dir().unwrap();
        let mut want = String::new();
        encode_path(&cwd.join("rel.txt"), &mut want);
        let f = HyperlinkFormat::parse("grep+://{path}:{line}")
            .unwrap()
            .unwrap();
        let sub = if cfg!(windows) {
            r".\rel.txt"
        } else {
            "./rel.txt"
        };
        assert_eq!(
            f.url(sub, Some(2), None).unwrap(),
            format!("grep+://{want}:2")
        );
    }
}
