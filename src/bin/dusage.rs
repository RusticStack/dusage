// dusage: disk usage, content search and cleanup in one binary.
use dusage::cmd;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    cmd::finish(cmd::dispatch(raw));
}
