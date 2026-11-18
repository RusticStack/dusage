// fastdel: parallel age/prefix filtered pruner (std-only, DESTRUCTIVE, no recycle bin).
// There is deliberately no confirmation prompt and no --yes flag; use --dry to preview.
use dusage::cmd;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    cmd::finish_prune(cmd::prune::run(raw));
}
