#!/usr/bin/env python3
"""Evict a tree's file data from the page cache without root.

`echo 3 > /proc/sys/vm/drop_caches` is how the cold-cache pass would really start, and
bench/run.sh tries it first. It needs root, so on an unprivileged box the harness falls
back to this: one `posix_fadvise(POSIX_FADV_DONTNEED)` per regular file, which drops that
file's clean data pages from the page cache exactly, the same way `vmtouch -e` does.

What it does not do is evict dentries or inodes — only the kernel's `drop_caches` shrinker
does that. A metadata walk (`scan`, `du`, `find`) therefore stays effectively warm here and
the harness labels it that way; a content read (`search`) genuinely comes back cold.

Usage: bench/evict.py PATH...   (directories are walked, symlinks are not followed)
Prints the number of files advised to stderr.
"""

import os
import sys


def evict_file(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


def main(argv):
    if len(argv) < 2:
        print("usage: evict.py PATH...", file=sys.stderr)
        return 2
    total = 0
    for target in argv[1:]:
        if os.path.isdir(target) and not os.path.islink(target):
            for dirpath, _dirnames, filenames in os.walk(target, followlinks=False):
                for name in filenames:
                    try:
                        if not os.path.islink(os.path.join(dirpath, name)):
                            evict_file(os.path.join(dirpath, name))
                            total += 1
                    except OSError:
                        pass
        else:
            try:
                evict_file(target)
                total += 1
            except OSError:
                pass
    print(f"evicted={total}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
