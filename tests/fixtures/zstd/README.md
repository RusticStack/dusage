# zstd fixtures

Inputs for the frame decoder in `src/archive_zstd.rs`. Every `.zst` here was produced by
the Zstandard CLI (v1.5.5) with the command in the table, and every test reads the pair
`<name>.zst` -> `<name>.bin`, so nothing at test time needs the `zstd` binary. Windows CI
runs the same tests against these committed bytes.

Regenerate the plaintexts, then the frames, from this directory:

```sh
python3 - <<'PY'
import random

random.seed(20261119)
words = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi "
         "rho sigma tau upsilon phi chi psi omega disk usage search prune archive zstd frame "
         "block literal sequence offset window").split()
lines, total, i = [], 0, 0
while total < 200 * 1024:
    line = f"{i:07d} " + " ".join(random.choice(words) for _ in range(random.randint(4, 18))) + "\n"
    lines.append(line)
    total += len(line)
    i += 1
open("text.bin", "wb").write("".join(lines).encode())

open("empty.bin", "wb").write(b"")
open("one.bin", "wb").write(b"x")

random.seed(4242)
open("random.bin", "wb").write(bytes(random.getrandbits(8) for _ in range(64 * 1024)))

open("repeat.bin", "wb").write(b"A" * 200000)

open("modes.bin", "wb").write(b"".join(b"%04d----" % (i % 10000) for i in range(20000)))

random.seed(3)
open("reuse.bin", "wb").write(b"".join(b"a" * 40 if random.random() < 0.9 else b"b" for _ in range(8000)))

open("rletables.bin", "wb").write((b"A" * 1000 + b"Z") * 100)

open("concat.bin", "wb").write(open("text.bin", "rb").read() + open("one.bin", "rb").read())
PY

zstd -q -f -3  empty.bin     -o empty.zst
zstd -q -f -3  one.bin       -o one.zst
zstd -q -f -3  text.bin      -o text3.zst
zstd -q -f -19 text.bin      -o text19.zst
zstd -q -f -19 text.bin      -o streams4.zst
zstd -q -f --no-check -3 text.bin -o nocheck.zst
zstd -q -f --content-size -3 text.bin -o contentsize.zst
zstd -q -f -3  random.bin    -o random.zst
zstd -q -f -3  repeat.bin    -o repeat.zst
zstd -q -f -1 -B1024 modes.bin -o modes.zst
zstd -q -f -12 -B1024 reuse.bin -o reuse.zst
zstd -q -f -1  rletables.bin -o rletables.zst
cat text.bin | zstd -q -3 > stream.zst
cat text.bin | zstd -q -3 --long=27 > long.zst
cat text3.zst one.zst > concat.zst
```

What each frame exercises, as the fixture set stands:

| frame | plaintext | covers |
| --- | --- | --- |
| `empty.zst` | `empty.bin` | a frame with nothing in it: a zero-length raw block, content size 0 |
| `one.zst` | `one.bin` | one byte, one raw block |
| `text3.zst` | `text.bin` | 200 KiB at `-3`: four-stream Huffman literals, a treeless literals block, FSE sequence tables in all three slots |
| `text19.zst` | `text.bin` | the same text at `-19`, where the encoder splits the window differently |
| `streams4.zst` | `text.bin` | four-stream Huffman with compressed weights, and one-stream blocks |
| `nocheck.zst` | `text.bin` | a frame with no content checksum |
| `contentsize.zst` | `text.bin` | a single-segment frame whose window is the frame content size |
| `stream.zst` | `text.bin` | a frame written from a pipe: a window descriptor and no content size |
| `long.zst` | `text.bin` | `--long=27`, a 128 MiB window, which is the decoder's cap |
| `random.zst` | `random.bin` | incompressible input: one 64 KiB raw block |
| `repeat.zst` | `repeat.bin` | 200000 repeated bytes: an RLE block |
| `modes.zst` | `modes.bin` | two blocks at `-B1024`, an offset table in RLE mode |
| `reuse.zst` | `reuse.bin` | six blocks: repeat-mode sequence tables |
| `rletables.zst` | `rletables.bin` | all three sequence tables in RLE mode |
| `concat.zst` | `concat.bin` | two frames back to back |

Skippable frames, a literals block in RLE mode, a named dictionary, a window above the cap
and every truncation are small enough to build inside the tests themselves, so they are not
fixtures.
