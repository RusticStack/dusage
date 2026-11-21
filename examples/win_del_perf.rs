//! Delete strategy on a real NTFS volume: serial `std::fs::remove_dir_all` against the
//! handle-based serial walk, with the POSIX and classic dispositions, plus the error
//! categories each one ends with.
//!
//! `cargo run --release --example win_del_perf -- <root> <std|serial-posix|serial-classic>`
//!
//! One destructive run per invocation: the driver makes a fresh copy of the tree for
//! every row, so the clock never includes the copy. The parallel chunked path is
//! `fastdel` itself and is timed by the same driver.

use std::time::Instant;

#[cfg(not(windows))]
fn main() {
    eprintln!("win_del_perf drives the Windows delete paths only");
}

#[cfg(windows)]
fn main() {
    use dusage::model::ErrorCounts;
    use dusage::win::delete::remove_tree_with;
    let mut args = std::env::args().skip(1);
    let root: std::path::PathBuf = args
        .next()
        .expect("usage: win_del_perf <root> <mode>")
        .into();
    let mode = args.next().unwrap_or_else(|| "serial-posix".into());
    if !root.exists() {
        eprintln!("not a directory: {}", root.display());
        std::process::exit(2);
    }
    let (ms, deleted, errs) = match mode.as_str() {
        "std" => {
            let start = Instant::now();
            let mut errs = ErrorCounts::default();
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {}
                Err(e) => {
                    errs.record(&e);
                }
            }
            (start.elapsed().as_secs_f64() * 1000.0, 1u64, errs)
        }
        "serial-posix" | "serial-classic" => {
            let start = Instant::now();
            let (deleted, errs) = remove_tree_with(&root, mode == "serial-posix");
            (start.elapsed().as_secs_f64() * 1000.0, deleted, errs)
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };
    println!(
        "{{\"mode\":\"{mode}\",\"ms\":{ms:.1},\"deleted\":{deleted},\
         \"access_denied\":{},\"sharing_violation\":{},\"not_found\":{},\
         \"dir_not_empty\":{},\"other\":{},\"left\":{}}}",
        errs.access_denied,
        errs.sharing_violation,
        errs.not_found,
        errs.dir_not_empty,
        errs.other,
        root.exists() as u8
    );
}
