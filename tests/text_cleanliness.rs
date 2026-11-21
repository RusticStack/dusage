//! A control byte in a text file, where a backslash belongs, is invisible in review and
//! live in the artefact: it has cost this repository three defects in one slice — a
//! PowerShell transcript aimed at `windows<BEL>dmin.log`, a Git bash fallback pointing at
//! `Git<BS>ash.exe`, and `C:<FF>orge` in `AGENTS.md`, all from a path written through
//! something that interpreted `\a`, `\b` and `\f` as escapes. Every one of them read fine
//! in a diff and every one of them was found by accident.
//!
//! So this walks the repository's prose and harness scripts and fails on any byte below
//! `0x20` other than tab, newline and carriage return, naming the file, the line and the
//! byte. It covers the extensions whose contents are read as text with no escape
//! processing — Markdown, shell, PowerShell, TOML, JSON, YAML and plain text. Rust sources
//! are left out on purpose: the compiler and `rustfmt` reject or normalise an accidental
//! escape there, and one deliberate backspace lives in a regex test pattern.
//!
//! Fixtures are skipped: `tests/fixtures/` holds compressed and binary inputs on purpose.

use std::fs;
use std::path::{Path, PathBuf};

const TEXT_EXTENSIONS: [&str; 8] = ["md", "sh", "ps1", "toml", "json", "jsonl", "yml", "txt"];

fn walk(dir: &Path, visited: &mut usize, failures: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            failures.push(format!("{}: cannot be read: {e}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), ".git" | "target" | "fixtures") {
                continue;
            }
            walk(&path, visited, failures);
            continue;
        }
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !TEXT_EXTENSIONS.contains(&extension) {
            continue;
        }
        check(&path, visited, failures);
    }
}

fn check(path: &Path, visited: &mut usize, failures: &mut Vec<String>) {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            failures.push(format!("{}: cannot be read: {e}", path.display()));
            return;
        }
    };
    *visited += 1;
    let mut line = 1;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            line += 1;
            continue;
        }
        let stray = *byte < 0x20 && !matches!(*byte, b'\t' | b'\r');
        if stray {
            let column = bytes[..index]
                .iter()
                .rev()
                .take_while(|b| **b != b'\n')
                .count()
                + 1;
            failures.push(format!(
                "{}:{line}:{column}: control byte 0x{byte:02x} in a text file",
                path.display()
            ));
        }
    }
}

#[test]
fn text_files_carry_no_control_bytes() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut visited = 0;
    let mut failures = Vec::new();
    walk(&root, &mut visited, &mut failures);

    // A walker that visits nothing would pass this test while checking nothing.
    assert!(
        visited > 20,
        "the walk read only {visited} text files under {}; it is looking in the wrong place",
        root.display()
    );
    assert!(
        failures.is_empty(),
        "control bytes where characters belong:\n{}",
        failures.join("\n")
    );
}
