---
name: dusage
description: Find what is using disk space on Windows and free it safely with the dusage and fastdel CLIs. Use when the user asks what is filling a drive or folder, why a disk is full, or to clean up caches, build output, temp or agent folders on Windows. Scans are read-only; deletion only happens after the user approves an exact list.
argument-hint: "[drive or folder, e.g. C:\\]"
---

# dusage: find and reclaim disk space on Windows

`dusage` reports what uses space under a folder or drive and never writes anything.
`fastdel` permanently deletes a folder's children that match a name prefix and an age.
Both are single-file Windows exes from https://github.com/RusticStack/dusage. They do not
run on Linux or macOS. On a non-Windows machine, tell the user and stop.

## 1. Make sure the tools are available

```powershell
dusage -V; fastdel -V
```

If either is missing, download the latest release into a user folder (no admin needed),
verify it, and use that path:

```powershell
$dir = "$env:LOCALAPPDATA\Programs\dusage"; New-Item -ItemType Directory -Force $dir | Out-Null
$rel = Invoke-RestMethod https://api.github.com/repos/RusticStack/dusage/releases/latest
foreach ($n in 'dusage.exe','fastdel.exe','SHA256SUMS.txt') {
  $a = $rel.assets | Where-Object name -eq $n
  Invoke-WebRequest $a.browser_download_url -OutFile "$dir\$n"
}
$sums = Get-Content "$dir\SHA256SUMS.txt"
foreach ($n in 'dusage.exe','fastdel.exe') {
  $h = (Get-FileHash "$dir\$n" -Algorithm SHA256).Hash.ToLower()
  if (-not ($sums -match "^$h\s+\*?$n$")) { throw "checksum mismatch for $n" }
}
$env:PATH = "$dir;$env:PATH"
```

Do not change the user's permanent PATH without asking.

## 2. Find where the space goes

Start broad and drill into the biggest entry each time. `dusage` lists only the direct
children of the root, so drilling down means scanning again one level deeper.

```powershell
Get-PSDrive -PSProvider FileSystem          # used and free space per drive
dusage C:\ --on-disk --by-ext --top 40      # whole drive: top entries, big files, extensions
dusage C:\some\big\folder --top 25          # drill into the largest entry
dusage C:\some\folder --dedup-hardlinks     # space actually freed (pnpm, .git share files)
dusage C:\some\folder --json                # machine-readable, for summing groups
```

Speed tips:
- In an elevated shell, add `--mft` for whole drives or trees with millions of files. It
  reads the NTFS master file table: about 10 s for a drive instead of a minute or more. It
  always reads the whole volume's table, so skip it for small folders. Without admin it
  fails with "needs an elevated (admin) shell". Fall back to the default backend then.
- Run independent scans of different folders in parallel.
- Totals at a volume root under `--mft` include NTFS metadata (`$MFT`, `$LogFile`) and
  `pagefile.sys`. Those are not reclaimable.

Folder age: a directory's modified time only changes when a direct child is created or
deleted. To see whether a tree is still in use, check `LastWriteTime` of the folder and a few
children. Treat this as a hint and confirm with the user.

## 3. Report, then ask

Do not delete anything during the investigation. Present one table sorted by size, with:
- path (or a group, e.g. `build-*`, 26 folders),
- size (use `--dedup-hardlinks` figures where they differ),
- last modified,
- what it is and whether it regenerates: build output, package cache, temp, logs, user data,
  or unknown,
- the risk of removing it.

Then ask the user which items to clean up. Name concrete items and sizes, and recommend
the safest wins first: build output, caches that regenerate, and old temp folders. Never
assume approval, and never treat an earlier approval as covering new items.

Never propose deleting user documents, downloads, desktop files, game saves, databases of
running apps, credentials (`.ssh`, keys), or anything under Windows, Program Files or
ProgramData. Name those as large items for the user to handle.

## 4. Clean up only what was approved

Prefer the owning tool when one exists. It knows what is safe:
`cargo clean`, `pnpm store prune`, `npm cache clean --force`, `uv cache clean`,
`pip cache purge`, `go clean -cache`, `docker system prune`, or Windows Disk Cleanup.

For whole folders of approved items, use `fastdel`, always as a dry run first:

```powershell
fastdel C:\dev\cache --age 7d --dry                            # entries idle for 7+ days
fastdel C:\dev --age -1 --prefix build-,target- --dry   # by name, any age
```

Show the user the dry-run plan. Run the real command only after they confirm that exact
plan, and always with a log:

```powershell
fastdel C:\dev --age -1 --prefix build- --log "$env:TEMP\fastdel.log"
```

Rules:
- Deletion is permanent. There is no Recycle Bin and no undo.
- `fastdel` deletes the children of the root, never the root itself. It refuses drive roots,
  Windows, Program Files, ProgramData and user-profile roots in every spelling. Do not try
  to work around a refusal; report it.
- `--prefix` is case-sensitive. `--age` takes `30m`, `2h`, `7d`, `1w`, or `-1` for any age.
- Junctions and symlinks are removed as links. Their targets are never touched.
- Exit code 1 means some entries failed (locked or in use). Report the summary. Never retry
  in a loop, and never kill processes to unlock files without asking.
- Never delete the working directory or temp folder of the session you are running in.

Afterwards, run `Get-PSDrive` again and report the space recovered per item.
