# EXAMPLE.md — reclaiming a full system drive

How the two tools are meant to be used together, from the kind of incident that
motivated them: a developer workstation whose system drive is nearly full and
still filling, because build outputs and tool temp folders grow faster than
anyone cleans them. Paths below are illustrative.

## 1. Find where the space went

```powershell
dusage C:\ --min-big 300M --top 100 --on-disk
```

On a drive with about 10 million files this took under 3 minutes with version
0.1, where a sequential PowerShell `Get-ChildItem` pass took 15-20 minutes. A
typical result:

| Top entry | What it usually is |
|---|---|
| a build or cache folder | toolchains, package caches, `target` folders from parallel builds |
| ↳ a tool temp folder | hundreds of thousands of per-session scratch folders nobody deletes |
| `Users` | `AppData` caches, editor and tool project caches |
| `Program Files` (+x86) | installed apps |
| `Windows` | normal, sometimes plus large crash dumps in `LiveKernelReports` |

Then drill into the biggest entry:

```powershell
dusage C:\cache --by-ext --top 30
```

## 2. Prune it, safely

Always preview with `--dry`, then run with a log:

```powershell
# old build outputs, any age
fastdel C:\cache --age -1 --prefix build-target,cargo-target --dry
fastdel C:\cache --age -1 --prefix build-target,cargo-target --log prune.log

# temp entries untouched for a day (positional form, still supported)
fastdel %TEMP% 1 "" 48

# per-session markers idle for more than 2 hours
fastdel C:\cache\tool-tmp --age 2h --prefix .session- --threads 96
```

## Lessons encoded in the tools

1. **Directory mtime is not recursive age.** On NTFS a folder's last-write time
   changes only when a direct child is created or deleted. That is exactly
   "idle since" for marker and session folders, which is what `fastdel --age`
   uses.
2. **Size concentrates.** Hundreds of thousands of small entries can hold little,
   while a few dozen folders hold most of the space. Look at the report before
   assuming a distribution.
3. **Negative durations panic** (`Duration::from_secs_f64(-x)`), which is why a
   negative age means "no age filter" instead of computing a cutoff.
4. **"Delete everything in this folder" is one typo away from disaster**, which is
   why protected roots are refused in every spelling. Always `--dry` first.
5. **Reparse points must never be followed** on Windows: `C:\` has junctions such
   as `Documents and Settings` that loop back into `Users`.
6. **Hardlinks inflate** naive byte sums (pnpm stores, git objects); use
   `--dedup-hardlinks` for an exact figure.
7. **Know the churn rate before pruning.** A temp folder that refills tens of GB
   per session beats any one-off cleanup; schedule `fastdel` instead.
