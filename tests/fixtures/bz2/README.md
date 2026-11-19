# bzip2 fixtures

Inputs for the decoder tests in `src/archive_bz2.rs`. Every `.bz2` here was produced by the
system `bzip2` (1.0.8) from the plaintext beside it, and each plaintext is either a checked-in
file the test loads (`mixed_200k.txt`, `big_block.txt`, `two_streams.txt`) or a value the test
hard-codes (`x` for `one.bz2`, empty for `empty.bz2`).

To regenerate, from this directory:

```sh
# plaintext: 200 KiB of mixed content, and one 900k block of repetitive text
python3 - <<'PY'
import random

r = random.Random(20261119)
words = ["alpha", "needle", "gamma", "delta", "disk", "usage", "search", "block",
         "bzip", "stream", "huffman", "crc", "needle", "selector", "table"]
out = bytearray()
while len(out) < 204800:
    line = " ".join(r.choice(words) for _ in range(r.randint(2, 14)))
    if r.random() < 0.10:
        line = line.upper()
    if r.random() < 0.15:
        line = "\t" + line
    if r.random() < 0.20:
        line += " " + str(r.randint(0, 900000))
    if r.random() < 0.05:
        line += " \u00e9\u00e8\u00fc\u00df"
    out += line.encode("utf-8")
    out += b"\r\n" if r.random() < 0.02 else b"\n"
    if r.random() < 0.04:
        out += b"\n"
    if r.random() < 0.01:
        out += bytes([r.choice(words).encode()[0]]) * r.randint(4, 600) + b"\n"
open("mixed_200k.txt", "wb").write(bytes(out[: out.rfind(b"\n") + 1]))

line = b"needle alpha gamma delta 0123456789 the quick brown fox jumps\n"
big = bytearray(b"A" * 2000 + b"\n")
while len(big) < 900000:
    big += line
open("big_block.txt", "wb").write(bytes(big[:900000]))
PY

printf 'x' > one.txt
: > empty.txt
printf 'first stream: alpha needle\n' > first.txt
printf 'second stream: beta needle\n' > second.txt
cat first.txt second.txt > two_streams.txt

# fixtures
bzip2 -9 -c one.txt > one.bz2
bzip2 -9 -c empty.txt > empty.bz2
bzip2 -9 -c mixed_200k.txt > mixed_200k.bz2
bzip2 -1 -c mixed_200k.txt > mixed_200k_level1.bz2        # three 100 KB blocks in one member
bzip2 -9 -c big_block.txt > big_block.bz2                 # one full 900 KB block
cat <(bzip2 -9 -c first.txt) <(bzip2 -9 -c second.txt) > two_streams.bz2

# one bit flipped a third of the way into the first block's Huffman data
python3 -c "s = bytearray(open('mixed_200k.bz2','rb').read()); s[len(s)//3] ^= 0x40; open('corrupt.bz2','wb').write(bytes(s))"

rm -f one.txt empty.txt first.txt second.txt
```

`corrupt.bz2` is the only fixture without a plaintext: it must fail to decode. The truncated
case needs no fixture, the tests cut `mixed_200k.bz2` and `two_streams.bz2` themselves.
