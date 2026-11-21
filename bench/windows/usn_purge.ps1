# Purge behaviour of the USN --watch feed (roadmap item 9, last unmeasured line).
#
#   powershell -ExecutionPolicy Bypass -File bench/windows/usn_purge.ps1 [-Vhd <path>] [-Flood 20000]
#
# Run from an elevated shell: it creates its own VHD, formats it NTFS, assigns a free drive
# letter, and detaches and deletes the VHD when it is done. Nothing outside that volume is
# written, and no prune runs against a real path.
#
# The question is the one path item 9 could not measure on a system volume: what happens when
# the records a slow watch has not read are purged out of the journal before it gets to them.
# C:'s journal is 32 MB and nothing this harness does writes fast enough to wrap it, so the
# experiment runs on a volume whose journal is the 1 MB a small NTFS volume gets at format
# time. Two cases, same volume, same journal, same total volume of change:
#
#   burst    one 20000-file burst, --watch 10. The unread backlog is several times the journal
#            and a checkpoint (every ~5 s) trims it before the next poll, so the read starts
#            from a USN that no longer exists. Expect ERROR_JOURNAL_ENTRY_DELETED (1181), the
#            watch reporting "change journal reset (...)", a full rescan, and totals that
#            still equal a full scan.
#   paced    the same 20000 files written 2000 per second under --watch 1, so no interval ever
#            holds more than a fraction of the journal. Expect no reset at all.
#
# That pair is what separates "the journal purged" from "the volume is slow", and it is why
# the trigger is journal size against write rate rather than volume size.
#
# Journal geometry is fixed at format time and `fsutil usn createjournal m=` does not change
# it on this build (both calls return 0 and the geometry stays put); this script logs that
# rather than assuming it.

param(
    [string]$Vhd = (Join-Path $env:TEMP 'dusage_usn_purge\purge.vhd'),
    [string]$Bin = '',
    [string]$Letter = '',
    [int]$Flood = 20000
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = (Resolve-Path (Join-Path $here '..\..')).ProviderPath
if (-not $Bin) { $Bin = Join-Path $repo 'target\release\dusage.exe' }
$outRows = Join-Path $repo 'bench\windows\purge-results.jsonl'
$dir = [System.IO.Path]::GetDirectoryName($Vhd)
$log = Join-Path $dir 'purge.log'

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Error 'usn_purge.ps1 needs an elevated shell (Start-Process -Verb RunAs)'
    exit 2
}

function W([string]$m) {
    Add-Content -Path $log -Value $m -Encoding utf8
    [Console]::Out.WriteLine($m)
}

New-Item -ItemType Directory -Force -Path $dir | Out-Null
if (-not $Letter) {
    $used = [System.IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name.Substring(0, 1).ToUpper() }
    foreach ($c in 'W', 'X', 'Y', 'Z') { if ($used -notcontains $c) { $Letter = $c; break } }
}
if (-not $Letter) { Write-Error 'no free drive letter for the scratch volume'; exit 2 }
$vol = "${Letter}:"
$fixture = "$vol\pg"
$dp = Join-Path $dir 'dp.txt'

function Journal-Field([string]$name) {
    $q = (& fsutil usn queryjournal $vol | Out-String)
    $line = ($q -split "`r?`n" | Select-String -Pattern $name).Line
    if (-not $line) { return '' }
    return ($line -replace '.*: *', '').Trim()
}

function Journal-State([string]$tag) {
    W "STEP: journal state $tag"
    (& fsutil usn queryjournal $vol | Out-String).TrimEnd() -split "`r?`n" |
        Select-String -Pattern 'Usn Journal ID|First Usn|Next Usn|Maximum Size|Allocation Delta' |
        ForEach-Object { W "  $($_.Line.Trim())" }
}

function Reset-Fixture {
    Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $fixture | Out-Null
    for ($i = 1; $i -le 20; $i++) { [System.IO.File]::WriteAllText("$fixture\seed$i.txt", ('x' * 100)) }
}

function Watch-Start([string]$rep, [string]$err, [int]$interval) {
    Remove-Item $rep, $err -ErrorAction SilentlyContinue
    $env:DUSAGE_WATCH_TRACE = '1'
    return Start-Process -FilePath $Bin -ArgumentList @('scan', $fixture, '--watch', "$interval", '--json') `
        -RedirectStandardOutput $rep -RedirectStandardError $err -PassThru -NoNewWindow
}

function Compare-Totals([string]$rep) {
    $docs = @(((Get-Content -Raw -LiteralPath $rep) -split "`n") | Where-Object { $_.Trim().StartsWith('{') })
    $watchTotal = ''
    $fullTotal = ''
    $equal = $false
    $full = & $Bin scan $fixture --json | Out-String
    if ($docs.Count -gt 0 -and $full) {
        $w = $docs[-1] | ConvertFrom-Json
        $f = $full.Trim() | ConvertFrom-Json
        $watchTotal = ($w.total | ConvertTo-Json -Compress)
        $fullTotal = ($f.total | ConvertTo-Json -Compress)
        $tops = (($w.tops | ConvertTo-Json -Compress -Depth 5) -eq ($f.tops | ConvertTo-Json -Compress -Depth 5))
        $equal = ($watchTotal -eq $fullTotal) -and $tops
    }
    return @{ updates = $docs.Count; watch = $watchTotal; full = $fullTotal; equal = $equal }
}

function Count-Matches([string]$path, [string]$pattern) {
    if (-not (Test-Path $path)) { return 0 }
    return ([regex]::Matches((Get-Content -Raw -LiteralPath $path), $pattern)).Count
}

# The reset reason in a locale-free form: the numeric Win32 error, not the OS message text,
# which differs per Windows display language.
function Reset-Codes([string]$path) {
    if (-not (Test-Path $path)) { return '' }
    $codes = [regex]::Matches((Get-Content -Raw -LiteralPath $path), 'change journal reset \(.*?os error (\d+)') |
        ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
    return ($codes -join ',')
}

function Save-Row([hashtable]$row) {
    [System.IO.File]::AppendAllText($outRows, (($row | ConvertTo-Json -Compress -Depth 6) + "`n"),
        (New-Object System.Text.UTF8Encoding($false)))
}

# ---- case: burst -------------------------------------------------------------------------
function Run-Burst {
    $tag = 'burst'
    Reset-Fixture
    $rep = Join-Path $dir "$tag.out"
    $err = Join-Path $dir "$tag.err"
    $firstBefore = Journal-Field 'First Usn'
    $nextBefore = Journal-Field 'Next Usn'
    $p = Watch-Start $rep $err 10
    Start-Sleep -Milliseconds 3000

    $sw = [Diagnostics.Stopwatch]::StartNew()
    for ($i = 1; $i -le $Flood; $i++) { [System.IO.File]::WriteAllText("$fixture\flood-{0:d5}.bin" -f $i, 'f') }
    $sw.Stop()
    W "STEP: $tag wrote $Flood files in $($sw.ElapsedMilliseconds) ms"
    Start-Sleep -Seconds 12
    $firstAfter = Journal-Field 'First Usn'
    $nextAfter = Journal-Field 'Next Usn'
    [System.IO.File]::WriteAllText("$fixture\after-burst.txt", 'a')
    Start-Sleep -Seconds 12
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 500

    $t = Compare-Totals $rep
    $row = @{
        schema = 1; case = "purge-$tag"; volume = $vol; vhd_mb = 512
        journal_max = (Journal-Field 'Maximum Size'); flood_files = $Flood; flood_ms = $sw.ElapsedMilliseconds
        first_usn = "$firstBefore..$firstAfter"; next_usn = "$nextBefore..$nextAfter"
        updates = $t.updates
        changed_lines = (Count-Matches $err 'changed:')
        reset_lines = (Count-Matches $err 'change journal reset')
        reset_codes = (Reset-Codes $err)
        nofeed_lines = (Count-Matches $err 'no change feed')
        watch_total = $t.watch; full_total = $t.full; totals_equal = $t.equal
    }
    Save-Row $row
    W ("MEAS: $tag journal_max=$($row.journal_max) flood=$Flood ms=$($sw.ElapsedMilliseconds) updates=$($row.updates) changed=$($row.changed_lines) reset=$($row.reset_lines) totals_equal=$($t.equal)")
    W "---- $tag stderr (first 24) ----"
    (Get-Content $err | Select-Object -First 24) | ForEach-Object { W "  $_" }
    return $row
}

# ---- case: paced -------------------------------------------------------------------------
function Run-Paced {
    $tag = 'paced'
    Reset-Fixture
    $rep = Join-Path $dir "$tag.out"
    $err = Join-Path $dir "$tag.err"
    $p = Watch-Start $rep $err 1
    Start-Sleep -Milliseconds 3000

    $sw = [Diagnostics.Stopwatch]::StartNew()
    $per = [Math]::Max(1, [int]($Flood / 10))
    $n = 0
    for ($round = 1; $round -le 10; $round++) {
        for ($i = 1; $i -le $per; $i++) {
            $n++
            [System.IO.File]::WriteAllText("$fixture\flood-{0:d5}.bin" -f $n, 'f')
        }
        Start-Sleep -Milliseconds 1100
    }
    $sw.Stop()
    W "STEP: $tag wrote $n files in $($sw.ElapsedMilliseconds) ms across 10 paced rounds"
    Start-Sleep -Seconds 3
    [System.IO.File]::WriteAllText("$fixture\after-paced.txt", 'a')
    Start-Sleep -Seconds 3
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    Start-Sleep -Milliseconds 500

    $t = Compare-Totals $rep
    $row = @{
        schema = 1; case = "purge-$tag"; volume = $vol; vhd_mb = 512
        journal_max = (Journal-Field 'Maximum Size'); flood_files = $n; flood_ms = $sw.ElapsedMilliseconds
        updates = $t.updates
        changed_lines = (Count-Matches $err 'changed:')
        reset_lines = (Count-Matches $err 'change journal reset')
        reset_codes = (Reset-Codes $err)
        nofeed_lines = (Count-Matches $err 'no change feed')
        watch_total = $t.watch; full_total = $t.full; totals_equal = $t.equal
    }
    Save-Row $row
    W ("MEAS: $tag journal_max=$($row.journal_max) flood=$n ms=$($sw.ElapsedMilliseconds) updates=$($row.updates) changed=$($row.changed_lines) reset=$($row.reset_lines) totals_equal=$($t.equal)")
    W "---- $tag stderr (first 12) ----"
    (Get-Content $err | Select-Object -First 12) | ForEach-Object { W "  $_" }
    return $row
}

W "STEP: host=$env:COMPUTERNAME started=$(Get-Date -Format o) volume=$vol vhd=$Vhd"
try {
    if (Test-Path $Vhd) { Remove-Item $Vhd -Force }
    @(
        "create vdisk file=`"$Vhd`" maximum=512 type=fixed",
        "select vdisk file=`"$Vhd`"",
        'attach vdisk',
        'create partition primary',
        'format fs=ntfs quick label=USNPURGE',
        "assign letter=$Letter"
    ) | Set-Content -Path $dp -Encoding ascii
    (& diskpart /s $dp | Out-String) | Out-Null
    Start-Sleep -Seconds 3
    if (-not (Test-Path "$vol\")) { Write-Error "volume $vol did not appear"; exit 1 }
    W "MEAS: scratch volume $vol size=$([System.IO.DriveInfo]::new($vol).TotalSize) format=$([System.IO.DriveInfo]::new($vol).DriveFormat)"

    # Documented rather than assumed: does createjournal change the geometry?
    $before = Journal-Field 'Maximum Size'
    $mk = (& fsutil usn createjournal m=0x4000000 a=0x800000 W: 2>&1 | Out-String).Trim()
    Start-Sleep -Milliseconds 500
    $after = Journal-Field 'Maximum Size'
    W "MEAS: createjournal m=0x4000000 a=0x800000 exit=$LASTEXITCODE out=$(($mk -replace '\s+', ' ').Trim()) max_before='$before' max_after='$after'"
    Journal-State 'start'

    $burst = Run-Burst
    $paced = Run-Paced

    W "MEAS: verdict burst_resets=$($burst.reset_lines) paced_resets=$($paced.reset_lines) burst_totals_equal=$($burst.totals_equal) paced_totals_equal=$($paced.totals_equal) paced_updates=$($paced.updates) burst_updates=$($burst.updates)"
}
finally {
    @(
        "select vdisk file=`"$Vhd`"",
        'detach vdisk'
    ) | Set-Content -Path $dp -Encoding ascii
    & diskpart /s $dp | Out-Null
    Remove-Item $Vhd -Force -ErrorAction SilentlyContinue
    W "STEP: vhd detached and removed=$(-not (Test-Path $Vhd))"
    W "STEP: done $(Get-Date -Format o)"
}
