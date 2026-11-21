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
#   3. The USN --watch path: a watch is started, files are created and deleted under it, and
#      the update it prints is compared with a full rescan so a silent full rescan shows up.
#   4. A cold-cache pass: the standby list and working sets are purged between runs, which
#      is the only honest cold measurement on Windows and needs SeProfileSingleProcess.
#   5. FAT32 and ReFS volume behaviour for FileId, by attaching small VHDX files under
#      -Root and formatting them; the files are detached and deleted afterwards.
#
# Output: appends rows to bench/windows/admin-results.jsonl (merged into results.json by
# run.sh) and to bench/windows/checks.json (rendered by summary-checks.sh).

param(
    [string]$Root = 'D:\dusage-bench',
    [string]$Bin = '',
    [string[]]$Volumes = @('D:', 'E:'),
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

function Add-Row($o) { $null = $rows.Add($o) }
function Add-Check($o) { $null = $checks.Add($o) }

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

function Purge-Cache {
    [Mem]::EnableProfilePrivilege() | Out-Null
    # 2 empty working sets, 3 flush the modified list, 4 purge the standby list.
    foreach ($cmd in 2, 3, 4) { [Mem]::Purge($cmd) | Out-Null }
    Start-Sleep -Milliseconds 400
}

# Everything this run prints lands in a log beside the record, because an elevated window
# that closes takes its console with it.
$logPath = Join-Path $repo 'bench\windowsdmin.log'
Start-Transcript -Path $logPath -Force | Out-Null
Write-Output 'admin: elevated, starting'

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
            Write-Output "admin: defender exclusion for $Root removed"
        }
    }
}

# ---------------------------------------------- 2. --mft against the batched backend (q11)
if (-not $SkipMft) {
    foreach ($v in $Volumes) {
        $root = "${v}\"
        $batched = Native { & $Bin scan $root --backend batched --threads 16 --on-disk } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
        $mft = Native { & $Bin scan $root --mft --on-disk } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
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
        Write-Output ("admin: MFT {0}: {1} / {2}" -f $root, $batched, $mft)
    }
}

# --------------------------------------------------------- 3. USN --watch on a real volume (q11)
if (-not $SkipWatch) {
    $watchRoot = Join-Path $Root 'watch'
    Remove-Item -LiteralPath $watchRoot -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $watchRoot | Out-Null
    for ($i = 0; $i -lt 50; $i++) { [System.IO.File]::WriteAllText((Join-Path $watchRoot "seed$i.txt"), 'x') }
    $log = Join-Path $Root 'watch.log'
    $errLog = Join-Path $Root 'watch.err'
    $bash = (Get-Command bash.exe -ErrorAction SilentlyContinue).Source
    if (-not $bash) { $bash = 'C:\Program Files\Gitinash.exe' }
    $binBash = $Bin.Replace('', '/')
    $rootBash = $watchRoot.Replace('', '/')
    $logBash = $log.Replace('', '/')
    $errBash = $errLog.Replace('', '/')
    # The watch runs for twelve seconds; two files are created and one is deleted inside
    # that window, then the watch is stopped and its whole output is read back.
    $script = @"
"$binBash" scan "$rootBash" --watch 2 > "$logBash" 2> "$errBash" &
WPID=`$!
sleep 4
printf 'y' > "$rootBash/created_a.txt"
printf 'y' > "$rootBash/created_b.txt"
sleep 2
rm -f "$rootBash/created_a.txt"
sleep 6
kill `$WPID 2>/dev/null
wait `$WPID 2>/dev/null
exit 0
"@
    $script | Out-File -Encoding ascii -FilePath (Join-Path $Root 'watch.sh')
    Native { & $bash (Join-Path $Root 'watch.sh') } | Out-Null
    $out = Get-Content -Raw -LiteralPath $log -ErrorAction SilentlyContinue
    $err = Get-Content -Raw -LiteralPath $errLog -ErrorAction SilentlyContinue
    $sawCreated = ($out -match 'created_b')
    $sawDeleted = ($out -match 'created_a')
    $updates = ([regex]::Matches($out, 'elapsed')).Count
    Add-Check ([ordered]@{
            kind   = 'watch'
            step   = 'scan --watch 2 over a 50-file directory; two files created and one deleted while it ran'
            detail = "updates=$updates saw_created_b=$sawCreated saw_created_a=$sawDeleted full_rescan=$($out -match 'full')"
            output = (($out -replace '\s+', ' ').Trim())
            stderr = (($err -replace '\s+', ' ').Trim())
        })
    Write-Output ("admin: watch updates={0} created_seen={1} deleted_seen={2}" -f $updates, $sawCreated, $sawDeleted)
}

# ---------------------------------------------------------------- 4. cold cache (question 1)
if (-not $SkipCold) {
    foreach ($target in @('tree1m', 'flat100k')) {
        $p = Join-Path $Root $target
        $warm = Measure-Median { & $Bin scan $p --threads 16 } $Runs
        $cold = @()
        for ($i = 0; $i -lt $Runs; $i++) {
            Purge-Cache
            $sw = [Diagnostics.Stopwatch]::StartNew()
            Native { & $Bin scan $p --threads 16 } | Out-Null
            $sw.Stop()
            $cold += $sw.Elapsed.TotalMilliseconds
        }
        $coldSorted = $cold | Sort-Object
        $coldMs = [math]::Round($coldSorted[[int]($coldSorted.Count / 2)], 1)
        Add-Row ([ordered]@{ case = "scan-cold-$target"; section = 'enumeration'; cache = 'cold'; role = 'primary'; tool = 'dusage'; command = "dusage scan $p --threads 16 after a standby-list purge"; median_ms = $coldMs; min_ms = [math]::Round($coldSorted[0], 1); max_ms = [math]::Round($coldSorted[$coldSorted.Count - 1], 1); runs = $Runs })
        Add-Check ([ordered]@{ kind = 'cold'; case = $target; warm_ms = $warm; cold_ms = $coldMs; runs = $Runs })
        Write-Output ("admin: cold {0}: warm {1} ms, cold {2} ms" -f $target, $warm, $coldMs)
    }
}

# ------------------------------------- 5. FAT32 and ReFS FileId behaviour (question 6)
# Two small VHDX files under -Root, attached, formatted, probed and detached. The script
# never selects a physical disk, so nothing outside these two files can be touched.
if (-not $SkipVolumes) {
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
        Write-Output ("admin: {0} volume probed and removed" -f $spec.fs)
    }
}

# ---------------------------------------------------------------------------------- output
$lines = $rows | ForEach-Object { $_ | ConvertTo-Json -Compress -Depth 6 }
if ($lines) { [System.IO.File]::WriteAllLines($rowsOut, [string[]]$lines) }
if ($checks.Count -gt 0) {
    $existing = if (Test-Path $checksOut) { Get-Content -LiteralPath $checksOut } else { @() }
    $newLines = $checks | ForEach-Object { $_ | ConvertTo-Json -Compress -Depth 6 }
    [System.IO.File]::WriteAllLines($checksOut, [string[]]($existing + $newLines))
}
Write-Output ("admin: wrote {0} timing rows and {1} checks" -f $rows.Count, $checks.Count)
Stop-Transcript | Out-Null
