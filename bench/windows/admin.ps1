# The parts of the Windows benchmark that need an elevated token.
#
#   powershell -ExecutionPolicy Bypass -File bench/windows/admin.ps1 -Root D:\dusage-bench
#
# Run from an elevated shell (Start-Process -Verb RunAs). It does five things, in this
# order, and every one of them is contained in -Root or in a drive letter it creates itself:
#
#   1. Real-time protection A/B: the same scan and the same delete with Defender watching
#      and with -Root excluded. The exclusion is removed in a finally block whatever else
#      happens, and nothing here recommends turning protection off — it is measured only.
#   2. MFT against the batched backend and robocopy, on the volumes named in -Volumes.
#      One volume is the default because three whole-volume passes over a large volume take
#      longer than a person leaves an elevated window open; add 'E:' for a second data point.
#   3. The USN --watch path: 12 known changes in 4 groups under a running watch must print 4
#      updates, the journal's own record count for that window is read as an independent
#      oracle (bench/windows/usn_probe.py), and the last update's totals must equal a full
#      scan of the same tree.
#   4. A cold-cache pass: the standby list and working sets are purged between runs, which
#      is the only honest cold measurement on Windows and needs SeProfileSingleProcess.
#   5. FAT32 and ReFS volume behaviour for FileId, by attaching small VHDX files under
#      -Root and formatting them; the files are detached and deleted afterwards.
#
# Output: appends rows to bench/windows/admin-results.jsonl (merged into results.json by
# run.sh) and to bench/windows/checks.json (rendered by summary-checks.sh), plus a line per
# step in bench/windows/admin.log. Every row lands on disk as it is measured and each of the
# five sections is isolated, so a run that is closed or fails part-way still contributes
# whatever it finished.

param(
    [string]$Root = 'D:\dusage-bench',
    [string]$Bin = '',
    [string[]]$Volumes = @('D:'),
    [int]$Runs = 3,
    [switch]$SkipProtection,
    [switch]$SkipMft,
    [switch]$SkipWatch,
    [switch]$SkipCold,
    [switch]$SkipVolumes
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = (Resolve-Path (Join-Path $here '..\..')).ProviderPath
if (-not $Bin) { $Bin = Join-Path $repo 'target\release\dusage.exe' }
$example = Join-Path $repo 'target\release\examples\win_enum_perf.exe'
$delperf = Join-Path $repo 'target\release\examples\win_del_perf.exe'
$fastdel = Join-Path $repo 'target\release\fastdel.exe'
$rowsOut = Join-Path $repo 'bench\windows\admin-results.jsonl'
$checksOut = Join-Path $repo 'bench\windows\checks.json'

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Error 'admin.ps1 needs an elevated shell (Start-Process -Verb RunAs)'
    exit 2
}

$rows = New-Object System.Collections.ArrayList
$checks = New-Object System.Collections.ArrayList

# Rows and checks go to disk the moment they are measured. An elevated window can be
# closed, or a whole-volume scan can fail, at any point in a twenty-minute run, and a
# buffer that only lands at the end loses everything measured before it.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
function Add-Row($o) {
    $null = $rows.Add($o)
    [System.IO.File]::AppendAllText($rowsOut, (($o | ConvertTo-Json -Compress -Depth 6) + "`n"), $utf8NoBom)
}
function Add-Check($o) {
    $null = $checks.Add($o)
    [System.IO.File]::AppendAllText($checksOut, (($o | ConvertTo-Json -Compress -Depth 6) + "`n"), $utf8NoBom)
}

function Native([scriptblock]$sb) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { return (& $sb) } finally { $ErrorActionPreference = $old }
}

function Measure-Median([scriptblock]$sb, [int]$n) {
    $xs = @()
    for ($i = 0; $i -lt $n; $i++) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Native $sb | Out-Null
        $sw.Stop()
        $xs += $sw.Elapsed.TotalMilliseconds
        Start-Sleep -Milliseconds 100
    }
    $s = $xs | Sort-Object
    return [math]::Round($s[[int]($s.Count / 2)], 1)
}

# The standby-list purge: NtSetSystemInformation(SystemMemoryListInformation) with
# MemoryEmptyWorkingSets (2), MemoryFlushModifiedList (3) and MemoryPurgeStandbyList (4),
# which is what RAMMap's "Empty" actions do. Without it there is no cold cache on Windows.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Mem {
    [DllImport("ntdll.dll", SetLastError = true)]
    public static extern int NtSetSystemInformation(int cls, ref int info, int len);

    [DllImport("advapi32.dll", SetLastError = true)]
    public static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr token);

    [DllImport("advapi32.dll", SetLastError = true)]
    public static extern bool LookupPrivilegeValueW(string system, string name, out long luid);

    [DllImport("advapi32.dll", SetLastError = true)]
    public static extern bool AdjustTokenPrivileges(IntPtr token, bool disable, ref TP prev, int len, IntPtr prevlen, IntPtr retlen);

    [StructLayout(LayoutKind.Sequential)]
    public struct TP { public int Count; public long Luid; public int Attributes; }

    [DllImport("kernel32.dll")]
    public static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr h);

    public const uint TOKEN_ADJUST_PRIVILEGES = 0x20;
    public const uint TOKEN_QUERY = 0x8;
    public const uint SE_PRIVILEGE_ENABLED = 0x2;

    public static bool EnableProfilePrivilege() {
        IntPtr token;
        if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, out token)) return false;
        long luid;
        if (!LookupPrivilegeValueW(null, "SeProfileSingleProcessPrivilege", out luid)) { CloseHandle(token); return false; }
        TP tp = new TP();
        tp.Count = 1;
        tp.Luid = luid;
        tp.Attributes = (int)SE_PRIVILEGE_ENABLED;
        bool ok = AdjustTokenPrivileges(token, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
        CloseHandle(token);
        return ok;
    }

    public static int Purge(int command) {
        int v = command;
        return NtSetSystemInformation(0x50, ref v, 4);
    }
}
'@

# Returns whether the purge actually happened. Without SeProfileSingleProcess the calls are
# refused and the cache is untouched, so the caller must not label the timing cold.
function Purge-Cache {
    if (-not [Mem]::EnableProfilePrivilege()) { return $false }
    # 2 empty working sets, 3 flush the modified list, 4 purge the standby list.
    $codes = @()
    foreach ($cmd in 2, 3, 4) { $codes += [Mem]::Purge($cmd) }
    Start-Sleep -Milliseconds 400
    return (($codes | Where-Object { $_ -ne 0 }).Count -eq 0)
}

# Everything this run prints lands in a log beside the record, because an elevated window
# that closes takes its console with it. The transcript is a convenience; Write-Log is the
# record, so a transcript that cannot start never costs the run.
$logPath = Join-Path $repo 'bench\windows\admin.log'
function Write-Log([string]$m) {
    $line = "{0} {1}" -f (Get-Date -Format 'HH:mm:ss'), $m
    Write-Output $line
    try { [System.IO.File]::AppendAllText($logPath, $line + "`r`n", $utf8NoBom) } catch { }
}
try { Start-Transcript -Path (Join-Path $repo 'bench\windows\admin-transcript.log') -Force | Out-Null } catch { }
Write-Log ("admin: elevated={0} repo={1} root={2} bin={3}" -f $isAdmin, $repo, $Root, $Bin)
Write-Log ("admin: sections {0}" -f (($PSBoundParameters.Keys | Where-Object { $_ -like 'Skip*' }) -join ' '))

# ------------------------------------------------------ 1. real-time protection (question 3)
# Measure only. The exclusion is for -Root, it is removed before this script returns, and
# nothing in this script or the documents it feeds recommends disabling protection.
if (-not $SkipProtection) {
    $scanTree = Join-Path $Root 'tree1m'
    $delTree = Join-Path $Root 'srcA'
    $excluded = $false
    try {
        if (-not (Get-Command Add-MpPreference -ErrorAction SilentlyContinue)) {
            Add-Check ([ordered]@{ kind = 'protection'; operation = 'unavailable'; tree = ''; protection = 'unknown'; median_ms = $null; runs = 0; detail = 'Add-MpPreference is not present (no Windows Defender API on this box)' })
            throw 'skip'
        }
        foreach ($state in 'on', 'excluded') {
            if ($state -eq 'excluded') {
                Add-MpPreference -ExclusionPath $Root -ErrorAction Stop
                $excluded = $true
                Start-Sleep -Seconds 2
            }
            $scanMs = Measure-Median { & $Bin scan $scanTree --threads 16 } $Runs
            Add-Row ([ordered]@{ case = "protection-scan-$state"; section = 'protection'; cache = 'warm'; role = 'primary'; tool = 'dusage'; command = "dusage scan $scanTree --threads 16 (protection $state)"; median_ms = $scanMs; min_ms = $scanMs; max_ms = $scanMs; runs = $Runs })
            Add-Check ([ordered]@{ kind = 'protection'; operation = 'scan tree1m (1M files)'; tree = 'tree1m'; protection = $state; median_ms = $scanMs; runs = $Runs })

            # The copy is outside the clock; only the delete is timed.
            Remove-Item -LiteralPath (Join-Path $Root 'run') -Recurse -Force -ErrorAction SilentlyContinue
            New-Item -ItemType Directory -Force -Path (Join-Path $Root 'run') | Out-Null
            Native { robocopy (Join-Path $Root 'srcA') (Join-Path $Root 'run\payload') /MIR /NFL /NDL /NJH /NJS /NP } | Out-Null
            $sw = [Diagnostics.Stopwatch]::StartNew()
            Native { & $fastdel (Join-Path $Root 'run') --age -1 --threads 16 } | Out-Null
            $sw.Stop()
            $delMs = [math]::Round($sw.Elapsed.TotalMilliseconds, 1)
            Add-Row ([ordered]@{ case = "protection-delete-$state"; section = 'protection'; cache = 'warm'; role = 'primary'; tool = 'dusage'; command = "fastdel $Root\run --age -1 --threads 16 (protection $state)"; median_ms = $delMs; min_ms = $delMs; max_ms = $delMs; runs = 1 })
            Add-Check ([ordered]@{ kind = 'protection'; operation = 'delete 50k files'; tree = 'srcA'; protection = $state; median_ms = $delMs; runs = 1 })
        }
    }
    catch {
        if ("$_" -notmatch 'skip') { Write-Output ("admin: protection section failed: " + $_) }
    }
    finally {
        if ($excluded) {
            Remove-MpPreference -ExclusionPath $Root -ErrorAction SilentlyContinue
            Write-Log "admin: defender exclusion for $Root removed"
        }
    }
}

# ---------------------------------------------- 2. --mft against the batched backend (q11)
if (-not $SkipMft) {
    Write-Log 'section 2: --mft vs batched vs robocopy'
    try {
        foreach ($v in $Volumes) {
            $root = "${v}\"
            # Three whole-volume passes per volume, so the slowest steps announce themselves:
            # a window that looks hung gets closed, and a closed window ends the run.
            Write-Log "  $root batched scan ..."
            $batched = Native { & $Bin scan $root --backend batched --threads 16 --on-disk } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
            Write-Log "  $root --mft scan ..."
            $mft = Native { & $Bin scan $root --mft --on-disk } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
            Write-Log "  $root robocopy /L ..."
            $robocopy = Native { & robocopy $root NULL /L /E /XJ /BYTES /NFL /NDL /NJH /NP } | Where-Object { $_ -match '^ *Bytes' } | Select-Object -First 1
            $check = [ordered]@{ kind = 'mft'; volume = $root }
            if ($batched -match 'on disk ([\d.]+) GB .* files (\d+) \| dirs (\d+)') {
                $check.batched_disk_gb = [double]$Matches[1]; $check.batched_files = [int64]$Matches[2]
            }
            if ($mft -match 'on disk ([\d.]+) GB .* files (\d+) \| dirs (\d+)') {
                $check.mft_disk_gb = [double]$Matches[1]; $check.mft_files = [int64]$Matches[2]
            }
            if ($robocopy -match 'Bytes *: *([\d,]+)') { $check.robocopy_bytes = [int64]($Matches[1] -replace ',', '') }
            $check.batched_line = $batched
            $check.mft_line = $mft
            Add-Check $check
            Write-Log ("admin: MFT {0}: {1} / {2}" -f $root, $batched, $mft)
        }
    }
    catch { Write-Log ("section 2 failed: " + $_) }
}

# --------------------------------------------------------- 3. USN --watch on a real volume (q11)
#
# Ground truth, not an impression: a 38-file fixture (3 subdirectories) is scanned, then
# exactly twelve changes are made in four groups two seconds apart (4 creates at the root,
# 2 data extends plus 1 create in a subdirectory, 2 renames, 3 deletes). The watch polls
# every second, so each group lands in a poll of its own: four changed lines are the truthful
# answer, and bench/windows/usn_probe.py reads the same journal window as an independent
# oracle for how many records those twelve changes produced and why.
#
# The last update's totals are compared with a full scan of the same tree: a watch whose
# incremental rescan drifts shows up as totals_equal=false, which is the failure this
# section exists to catch. Run it from an elevated shell, or no journal is opened at all
# (error 5) and every tick is a full rescan instead.
if (-not $SkipWatch) {
    Write-Log 'section 3: USN --watch'
    try {
        $watchRoot = Join-Path $Root 'watch'
        $errLog = Join-Path $Root 'watch.err'
        $probe = Join-Path $repo 'bench\windows\usn_probe.py'
        $letter = $watchRoot.Substring(0, 1)

        # 38 files over three subdirectories: small enough that one report is a few lines,
        # deep enough that a change two levels down still maps to a changed top-level entry.
        $reset = {
            param([string]$dir)
            Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
            foreach ($d in 'a', 'b', 'c') { $null = New-Item -ItemType Directory -Force -Path (Join-Path $dir $d) }
            for ($i = 1; $i -le 20; $i++) { [System.IO.File]::WriteAllText((Join-Path $dir "f$i.txt"), ('x' * 100)) }
            foreach ($d in 'a', 'b', 'c') {
                for ($i = 1; $i -le 6; $i++) { [System.IO.File]::WriteAllText((Join-Path $dir "$d\$d$i.txt"), ('y' * 200)) }
            }
        }

        $changes = {
            param([string]$dir)
            Write-Log 'watch: group 1 (4 creates at the root)'
            for ($i = 1; $i -le 4; $i++) { [System.IO.File]::WriteAllText((Join-Path $dir "new$i.bin"), ('z' * 300)) }
            Start-Sleep -Seconds 2
            Write-Log 'watch: group 2 (2 extends, 1 create in c\)'
            Add-Content -LiteralPath (Join-Path $dir 'a\a1.txt') -Value ('m' * 50)
            Add-Content -LiteralPath (Join-Path $dir 'b\b1.txt') -Value ('m' * 50)
            [System.IO.File]::WriteAllText((Join-Path $dir 'c\inner.txt'), ('i' * 120))
            Start-Sleep -Seconds 2
            Write-Log 'watch: group 3 (2 renames)'
            [System.IO.File]::Move((Join-Path $dir 'f1.txt'), (Join-Path $dir 'f1r.txt'))
            [System.IO.File]::Move((Join-Path $dir 'a\a2.txt'), (Join-Path $dir 'a\a2r.txt'))
            Start-Sleep -Seconds 2
            Write-Log 'watch: group 4 (3 deletes)'
            foreach ($f in 'f2.txt', 'f3.txt', 'c\c3.txt') { Remove-Item -LiteralPath (Join-Path $dir $f) -Force }
        }

        $journalUsn = {
            param([string]$vol)
            if (-not (Test-Path $probe)) { return $null }
            $q = Native { & python $probe query --volume $vol 2>$null }
            if (-not $q) { return $null }
            return [int64](($q | ConvertFrom-Json).next_usn)
        }

        & $reset $watchRoot
        $usnBefore = & $journalUsn $letter
        $rep = Join-Path $Root 'watch.json'
        $p = Start-Process -FilePath $Bin -ArgumentList @('scan', $watchRoot, '--watch', '1', '--json') `
            -RedirectStandardOutput $rep -RedirectStandardError $errLog -PassThru -NoNewWindow
        Start-Sleep -Milliseconds 2500
        & $changes $watchRoot
        Start-Sleep -Seconds 4
        if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
        Start-Sleep -Milliseconds 300
        $usnAfter = & $journalUsn $letter

        $out = Get-Content -Raw -LiteralPath $rep -ErrorAction SilentlyContinue
        $err = Get-Content -Raw -LiteralPath $errLog -ErrorAction SilentlyContinue
        if (-not $out) { $out = '' }
        if (-not $err) { $err = '' }

        # One JSON document per update: the initial scan plus one per poll that saw a change.
        $docs = @(($out -split "`n") | Where-Object { $_.Trim().StartsWith('{') })
        $updates = $docs.Count
        $changedLines = ([regex]::Matches($err, 'changed:')).Count
        $paced = ([regex]::Matches($err, 'no change feed|full rescan')).Count

        # Independent oracle: the journal records this run wrote, and why.
        $journalRecords = $null
        $journalHist = ''
        if ($null -ne $usnBefore -and $null -ne $usnAfter -and $usnAfter -gt $usnBefore) {
            $st = Native { & python $probe stats --volume $letter --start "$usnBefore" --end "$usnAfter" --under $watchRoot 2>$null }
            if ($st) {
                $head = ($st | Select-Object -First 1)
                if ($head -match 'records=(\d+)') { $journalRecords = [int]$Matches[1] }
                $journalHist = (($st | Select-Object -Skip 1) -join '; ').Trim()
            }
        }

        # Same tree, full scan: the incremental totals must match exactly.
        $full = Native { & $Bin scan $watchRoot --json } | Out-String
        $totalsEqual = $false
        $watchTotal = ''
        $fullTotal = ''
        if ($docs.Count -gt 0 -and $full) {
            try {
                $w = $docs[-1] | ConvertFrom-Json
                $f = $full.Trim() | ConvertFrom-Json
                $watchTotal = ($w.total | ConvertTo-Json -Compress)
                $fullTotal = ($f.total | ConvertTo-Json -Compress)
                $topsEqual = (($w.tops | ConvertTo-Json -Compress -Depth 5) -eq ($f.tops | ConvertTo-Json -Compress -Depth 5))
                $totalsEqual = ($watchTotal -eq $fullTotal) -and $topsEqual
            }
            catch { Write-Log ("watch: totals compare failed: " + $_) }
        }

        # Twelve changes over four groups: four changed lines, and no fallback rescan at all.
        $truthful = ($changedLines -eq 4) -and $totalsEqual -and ($paced -eq 0)

        $errSample = if ($err.Length -gt 2000) { $err.Substring(0, 2000) } else { $err }
        Add-Row ([ordered]@{
                schema          = 1
                case            = 'watch'
                volume          = "${letter}:"
                files           = 38
                changes         = 12
                groups          = 4
                updates         = $updates
                changed_lines   = $changedLines
                fallback_lines  = $paced
                journal_usn     = "$usnBefore..$usnAfter"
                journal_records = $journalRecords
                journal_reasons = $journalHist
                watch_total     = $watchTotal
                full_total      = $fullTotal
                totals_equal    = $totalsEqual
                truthful        = $truthful
            })
        Add-Check ([ordered]@{
                kind            = 'watch'
                step            = 'scan --watch 1 over a 38-file tree; 12 changes in 4 groups while it ran'
                changes         = 12
                groups          = 4
                updates         = $updates
                changed_lines   = $changedLines
                fallback_lines  = $paced
                journal_records = $journalRecords
                journal_reasons = $journalHist
                totals_equal    = $totalsEqual
                truthful        = $truthful
                detail          = "updates=$updates changed_lines=$changedLines fallback=$paced journal_records=$journalRecords totals_equal=$totalsEqual truthful=$truthful"
                output          = (($errSample -replace '\s+', ' ').Trim())
                output_chars    = $err.Length
            })
        Write-Log ("admin: watch updates={0} changed_lines={1} journal_records={2} totals_equal={3} truthful={4}" -f $updates, $changedLines, $journalRecords, $totalsEqual, $truthful)
    }
    catch { Write-Log ("section 3 failed: " + $_) }
}

# ---------------------------------------------------------------- 4. cold cache (question 1)
if (-not $SkipCold) {
    Write-Log 'section 4: cold cache'
    try {
        if (-not (Purge-Cache)) {
            Add-Check ([ordered]@{ kind = 'cold'; case = 'unavailable'; warm_ms = $null; cold_ms = $null; runs = 0; detail = 'standby-list purge refused (no SeProfileSingleProcess), so no run here can be called cold' })
            throw 'cold cache needs an elevated token; no row written rather than a warm row labelled cold'
        }
        foreach ($target in @('tree1m', 'flat100k')) {
            $p = Join-Path $Root $target
            $warm = Measure-Median { & $Bin scan $p --threads 16 } $Runs
            $cold = @()
            for ($i = 0; $i -lt $Runs; $i++) {
                if (-not (Purge-Cache)) { throw 'the standby-list purge stopped working mid-section' }
                $sw = [Diagnostics.Stopwatch]::StartNew()
                Native { & $Bin scan $p --threads 16 } | Out-Null
                $sw.Stop()
                $cold += $sw.Elapsed.TotalMilliseconds
            }
            $coldSorted = $cold | Sort-Object
            $coldMs = [math]::Round($coldSorted[[int]($coldSorted.Count / 2)], 1)
            Add-Row ([ordered]@{ case = "scan-cold-$target"; section = 'enumeration'; cache = 'cold'; role = 'primary'; tool = 'dusage'; command = "dusage scan $p --threads 16 after a standby-list purge"; median_ms = $coldMs; min_ms = [math]::Round($coldSorted[0], 1); max_ms = [math]::Round($coldSorted[$coldSorted.Count - 1], 1); runs = $Runs })
            Add-Check ([ordered]@{ kind = 'cold'; case = $target; warm_ms = $warm; cold_ms = $coldMs; runs = $Runs })
            Write-Log ("admin: cold {0}: warm {1} ms, cold {2} ms" -f $target, $warm, $coldMs)
        }
    }
    catch { Write-Log ("section 4 failed: " + $_) }
}

# ------------------------------------- 5. FAT32 and ReFS FileId behaviour (question 6)
# Two small VHDX files under -Root, attached, formatted, probed and detached. The script
# never selects a physical disk, so nothing outside these two files can be touched.
if (-not $SkipVolumes) {
    Write-Log 'section 5: FAT32 and ReFS FileId'
    try {
        $vhdDir = Join-Path $Root 'vhds'
        New-Item -ItemType Directory -Force -Path $vhdDir | Out-Null
        foreach ($spec in @(@{ fs = 'FAT32'; size = 512; letter = 'W' }, @{ fs = 'ReFS'; size = 4096; letter = 'X' })) {
            $file = Join-Path $vhdDir ("bench_{0}.vhdx" -f $spec.fs.ToLower())
            $script = @(
                "create vdisk file=`"$file`" maximum=$($spec.size) type=expandable",
                "select vdisk file=`"$file`"",
                'attach vdisk',
                'create partition primary',
                "format fs=$($spec.fs) label=DBENCH quick",
                "assign letter=$($spec.letter)"
            ) -join "`r`n"
            $dp = $script | & diskpart.exe 2>&1
            Start-Sleep -Seconds 2
            $target = "$($spec.letter):\"
            $check = [ordered]@{ kind = 'fileid-volume'; volume = $target; filesystem = $spec.fs }
            if (Test-Path -LiteralPath $target) {
                $probe = Join-Path $target 'probe.txt'
                try {
                    [System.IO.File]::WriteAllText($probe, 'x')
                    $scan = Native { & $Bin scan $target --threads 1 } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
                    $check.dusage_scan = $scan
                    $check.file_id_supported = if ($scan -match 'files') { 'scan ok' } else { 'scan failed' }
                    $check.fsutil = (Native { & fsutil file queryFileID $probe 2>&1 }) -join ' '
                    $check.dusage_dedup = Native { & $Bin scan $target --threads 1 --dedup-hardlinks } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
                }
                catch { $check.error = $_.Exception.Message }
            }
            else { $check.error = 'volume did not mount: ' + ($dp -join ' ') }
            Add-Check $check
            # Detach and delete so nothing is left mounted.
            $script = @(
                "select vdisk file=`"$file`"",
                'detach vdisk'
            ) -join "`r`n"
            $script | & diskpart.exe 2>&1 | Out-Null
            Start-Sleep -Seconds 1
            Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
            Write-Log ("admin: {0} volume probed and removed" -f $spec.fs)
        }
    }
    catch { Write-Log ("section 5 failed: " + $_) }
}

# ---------------------------------------------------------------------------------- output
Write-Log ("admin: wrote {0} timing rows and {1} checks" -f $rows.Count, $checks.Count)
Write-Log 'admin: done'
try { Stop-Transcript | Out-Null } catch { }
