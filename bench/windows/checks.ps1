# Behaviour checks that answer the roadmap's non-timing questions on this machine.
#
#   powershell -ExecutionPolicy Bypass -File bench/windows/checks.ps1 -Root D:\dusage-bench
#
# Writes bench/windows/checks.json: one JSON object per line, each with a "kind" that
# bench/windows/summary-checks.sh renders as a table. Nothing here is timed; the numbers
# are sizes, file ids and booleans read from the same Win32 calls dusage uses, so a claim in
# the docs can point at the check that produced it.
#
# The checks that need an elevated token are in bench/windows/admin.ps1 and append their
# rows to the same file.

param(
    [string]$Root = 'D:\dusage-bench',
    [string]$Bin = '',
    [string[]]$OvercountRoots = @(
        "$env:LOCALAPPDATA\pnpm\store",
        'C:\projects',
        "$env:USERPROFILE\.forge"
    ),
    [string]$Out = '',
    [switch]$Append
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Out) {
    $Out = Join-Path (Resolve-Path (Join-Path $here '..\..')).ProviderPath 'bench\windows\checks.json'
}
if (-not $Bin) {
    $repo = (Resolve-Path (Join-Path $here '..\..')).ProviderPath
    $Bin = Join-Path $repo 'target\release\dusage.exe'
}

$rows = New-Object System.Collections.ArrayList
function Add-Row($o) { $null = $rows.Add($o) }

# A native program that writes to stderr (dusage prints its banner there) is a terminating
# error while ErrorActionPreference is Stop, so native calls run with it relaxed.
function Native([scriptblock]$sb) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { return (& $sb) } finally { $ErrorActionPreference = $old }
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Chk {
    [StructLayout(LayoutKind.Sequential)]
    public struct StandardInfo { public long AllocationSize; public long EndOfFile; public uint Links; public byte DeletePending; public byte Directory; }
    [StructLayout(LayoutKind.Sequential)]
    public struct FileIdInfo { public ulong Serial; public ulong High; public ulong Low; }
    [StructLayout(LayoutKind.Sequential)]
    public struct DispositionInfoEx { public uint Flags; }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr sec, uint disp, uint flags, IntPtr templ);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandleEx(IntPtr h, int cls, out StandardInfo info, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandleEx(IntPtr h, int cls, out FileIdInfo info, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool SetFileInformationByHandle(IntPtr h, int cls, ref DispositionInfoEx info, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern uint GetCompressedFileSizeW(string name, out uint high);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr FindFirstStreamW(string name, int level, out StreamData data, uint flags);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FindNextStreamW(IntPtr h, out StreamData data);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct StreamData { public long StreamSize; [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 296)] public string cStreamName; }
}
'@

$READ_ATTR = [uint32]0x80
$WRITE_ATTR = 0x100
$DELETE = [uint32]0x10000
$SHARE_ALL = [uint32]7
$OPEN_EXISTING = 3
$FLAG_BACKUP = 0x02000000
$FLAG_REPARSE = 0x00200000
$STD_INFO = 1
$ID_INFO = 18
$DISP_EX = 21

function Open-Path($p, $access) {
    return [Chk]::CreateFileW($p, $access, $SHARE_ALL, [IntPtr]::Zero, $OPEN_EXISTING,
        ($FLAG_BACKUP -bor $FLAG_REPARSE), [IntPtr]::Zero)
}

# ---------------------------------------------------------------- allocation fidelity (q5)
$fid = Join-Path $Root 'fid'
foreach ($name in 'resident.bin', 'plain.bin', 'comp.bin', 'sparse.bin', 'ads.txt', 'hl_a.txt', 'hl_b.txt', 'junction', 'sub') {
    $p = Join-Path $fid $name
    if (-not (Test-Path -LiteralPath $p)) { continue }
    $row = [ordered]@{ kind = 'fidelity'; name = $name }
    $h = Open-Path $p $READ_ATTR
    if ($h -ne [IntPtr](-1)) {
        $std = New-Object Chk+StandardInfo
        if ([Chk]::GetFileInformationByHandleEx($h, $STD_INFO, [ref]$std, 24)) {
            $row.end_of_file = $std.EndOfFile
            $row.allocation_size = $std.AllocationSize
            $row.links = $std.Links
        }
        $id = New-Object Chk+FileIdInfo
        if ([Chk]::GetFileInformationByHandleEx($h, $ID_INFO, [ref]$id, 24)) {
            $row.file_id = ('{0:x16}{1:x16}' -f $id.High, $id.Low)
        }
        [Chk]::CloseHandle($h) | Out-Null
    }
    [uint32]$hi = 0
    [uint32]$lo = [Chk]::GetCompressedFileSizeW($p, [ref]$hi)
    $row.compressed_size = if ($lo -eq [uint32]::MaxValue -and $hi -eq [uint32]::MaxValue) { $null } else { ([int64]$hi -shl 32) -bor [int64]$lo }
    $sd = New-Object Chk+StreamData
    $fh = [Chk]::FindFirstStreamW($p, 0, [ref]$sd, 0)
    if ($fh -ne [IntPtr](-1)) {
        $names = @()
        while ($true) {
            $names += ('{0}={1}' -f $sd.cStreamName, $sd.StreamSize)
            $sd = New-Object Chk+StreamData
            if (-not [Chk]::FindNextStreamW($fh, [ref]$sd)) { break }
        }
        $row.streams = ($names -join ' ')
    }
    $it = Get-Item -LiteralPath $p -Force
    $row.attributes = $it.Attributes.ToString()
    Add-Row $row
}

# ------------------------------------------------------------------- file id semantics (q6)
$hl = [ordered]@{ kind = 'fileid' }
$hl.name = 'hardlink pair hl_a.txt / hl_b.txt'
$a = (Get-Item -LiteralPath (Join-Path $fid 'hl_a.txt') -Force)
$b = (Get-Item -LiteralPath (Join-Path $fid 'hl_b.txt') -Force)
$hl.links = $a.LinkType
$hl.same_file_id = $null
$ha = Open-Path $a.FullName $READ_ATTR
$hb = Open-Path $b.FullName $READ_ATTR
if ($ha -ne [IntPtr](-1) -and $hb -ne [IntPtr](-1)) {
    $ia = New-Object Chk+FileIdInfo
    $ib = New-Object Chk+FileIdInfo
    $okA = [Chk]::GetFileInformationByHandleEx($ha, $ID_INFO, [ref]$ia, 24)
    $okB = [Chk]::GetFileInformationByHandleEx($hb, $ID_INFO, [ref]$ib, 24)
    if ($okA -and $okB) {
        $hl.same_file_id = (($ia.Serial -eq $ib.Serial) -and ($ia.High -eq $ib.High) -and ($ia.Low -eq $ib.Low))
        $hl.file_id = ('{0:x16}{1:x16}' -f $ia.High, $ia.Low)
    }
}
if ($ha -ne [IntPtr](-1)) { [Chk]::CloseHandle($ha) | Out-Null }
if ($hb -ne [IntPtr](-1)) { [Chk]::CloseHandle($hb) | Out-Null }

# What `fsutil file queryFileID` needs, on this build: the syntax is checked with a real
# file and the result compared with the handle-derived id above.
$fsutil = Native { & fsutil file queryFileID (Join-Path $fid 'hl_a.txt') 2>$null }
$hl.fsutil_syntax = if ($LASTEXITCODE -eq 0) { 'ok' } else { 'error ' + ($fsutil -join ' ') }
$hl.fsutil_output = ($fsutil -join ' ').Trim()
Add-Row $hl

# Every local volume's file id support: NTFS is expected; ReFS/FAT are what the roadmap asks.
foreach ($v in Get-Volume | Where-Object { $_.DriveLetter }) {
    $probe = $v.DriveLetter + ':\'
    $row = [ordered]@{ kind = 'fileid-volume'; volume = $probe; filesystem = $v.FileSystemType }
    $h = Open-Path $probe $READ_ATTR
    if ($h -eq [IntPtr](-1)) {
        $row.error = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    }
    else {
        $id = New-Object Chk+FileIdInfo
        $row.file_id_supported = [bool][Chk]::GetFileInformationByHandleEx($h, $ID_INFO, [ref]$id, 24)
        if ($row.file_id_supported) { $row.file_id = ('{0:x16}{1:x16}' -f $id.High, $id.Low) }
        [Chk]::CloseHandle($h) | Out-Null
    }
    Add-Row $row
}

# ----------------------------------------------------------------------- reparse points (q7)
$j = Join-Path $fid 'junction'
if (Test-Path -LiteralPath $j) {
    $m = [System.IO.Directory]::GetFileSystemEntries($fid) | Where-Object { $_ -match 'junction$' } | Select-Object -First 1
    $it = Get-Item -LiteralPath $j -Force
    $row = [ordered]@{
        kind       = 'reparse'
        path       = 'fid\junction'
        link_type  = $it.LinkType
        is_dir     = [bool]$it.PSIsContainer
        attributes = $it.Attributes.ToString()
    }
    $h = Open-Path $j $READ_ATTR
    if ($h -ne [IntPtr](-1)) {
        $std = New-Object Chk+StandardInfo
        if ([Chk]::GetFileInformationByHandleEx($h, $STD_INFO, [ref]$std, 24)) { $row.directory_flag = [bool]$std.Directory }
        [Chk]::CloseHandle($h) | Out-Null
    }
    Add-Row $row

    # What dusage does with it: the junction's target holds one file, so a scan that
    # recursed into the link would report two copies of it.
    $scanLine = Native { & $Bin scan $fid --threads 1 2>$null } | Where-Object { $_ -match 'files \d+' } | Select-Object -First 1
    $row2 = [ordered]@{ kind = 'reparse-scan'; path = 'fid'; scan_line = $scanLine }
    if ($scanLine -match 'files (\d+)') { $row2.files = [int64]$Matches[1] }
    if ($scanLine -match 'dirs (\d+)') { $row2.dirs = [int64]$Matches[1] }
    $f = Native { & $Bin find '*' $fid 2>$null }
    $row2.find_paths = ($f -join ' ')
    $row2.find_mentions_target_twice = ([regex]::Matches(($f -join ' '), 'inner\.txt').Count -gt 1)
    Add-Row $row2
}

# --------------------------------------------------------------------------- long paths (q10)
$long = Join-Path $Root 'long'
if (Test-Path -LiteralPath $long) {
    $bs = [char]92
    $seg = 's' * 120
    $deepDir = $long + $bs + $seg + '0' + $bs + $seg + '1' + $bs + $seg + '2'
    $deep = $deepDir + $bs + 'f2.txt'
    $verb = $bs + $bs + '?' + $bs + $deep
    $row = [ordered]@{ kind = 'longpath'; plain_len = $deep.Length }
    $h = Open-Path $deep $READ_ATTR
    $row.plain_open_ok = ($h -ne [IntPtr](-1))
    if ($h -ne [IntPtr](-1)) { [Chk]::CloseHandle($h) | Out-Null }
    if (-not $row.plain_open_ok) { $row.plain_open_error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
    $h = Open-Path $verb $READ_ATTR
    $row.verbatim_open_ok = ($h -ne [IntPtr](-1))
    if ($h -ne [IntPtr](-1)) { [Chk]::CloseHandle($h) | Out-Null }
    if (-not $row.verbatim_open_ok) { $row.verbatim_open_error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
    $row.long_paths_enabled = ((Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -ErrorAction SilentlyContinue).LongPathsEnabled -eq 1)
    Add-Row $row
}

# -------------------------------------------------------------- hardlink overcount (q9)
foreach ($r in $OvercountRoots) {
    if (-not (Test-Path -LiteralPath $r)) { continue }
    $naive = Native { & $Bin scan $r --threads 16 --on-disk 2>$null } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
    $dedup = Native { & $Bin scan $r --threads 16 --on-disk --dedup-hardlinks 2>$null } | Where-Object { $_ -match 'TOTAL' } | Select-Object -First 1
    $row = [ordered]@{ kind = 'overcount'; root = $r; naive = $naive; dedup = $dedup }
    if ($naive -match 'on disk ([\d.]+) GB .* files (\d+)') { $row.naive_disk_gb = [double]$Matches[1]; $row.naive_files = [int64]$Matches[2] }
    if ($dedup -match 'on disk ([\d.]+) GB .* files (\d+)') { $row.dedup_disk_gb = [double]$Matches[1]; $row.dedup_files = [int64]$Matches[2] }
    if ($row.naive_disk_gb -and $row.dedup_disk_gb -and $row.dedup_disk_gb -gt 0) {
        $row.overcount_pct = [math]::Round(100.0 * ($row.naive_disk_gb - $row.dedup_disk_gb) / $row.dedup_disk_gb, 1)
    }
    Add-Row $row
}

# ------------------------------------------------------------------------ elevation (q8)
foreach ($dev in '\\.\C:', '\\.\D:') {
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $row = [ordered]@{ kind = 'elevation'; device = $dev; elevated = $isAdmin }
    $h = [Chk]::CreateFileW($dev, [uint32]2147483648, $SHARE_ALL, [IntPtr]::Zero, $OPEN_EXISTING, 0, [IntPtr]::Zero)
    $row.open_ok = ($h -ne [IntPtr](-1))
    if ($h -eq [IntPtr](-1)) { $row.open_error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
    else { [Chk]::CloseHandle($h) | Out-Null }
    Add-Row $row
}

# ------------------------------------------------------- POSIX delete semantics (question 4)
$posix = Join-Path $Root 'run'
New-Item -ItemType Directory -Force -Path $posix | Out-Null
$target = Join-Path $posix 'posix_probe.txt'
[System.IO.File]::WriteAllText($target, 'x')
$row = [ordered]@{ kind = 'posix-delete'; path = $target }
$h = [Chk]::CreateFileW($target, ($DELETE -bor $READ_ATTR), $SHARE_ALL, [IntPtr]::Zero, $OPEN_EXISTING, 0, [IntPtr]::Zero)
if ($h -eq [IntPtr](-1)) {
    $row.error = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
}
else {
    $dis = New-Object Chk+DispositionInfoEx
    $dis.Flags = 0x1 -bor 0x2 -bor 0x10
    $row.set_info_ok = [Chk]::SetFileInformationByHandle($h, $DISP_EX, [ref]$dis, 4)
    if (-not $row.set_info_ok) { $row.set_info_error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
    # What the name looks like between the disposition call and the last close: a fresh open
    # answers for it, so the answer is the error code, not a boolean.
    $h2 = Open-Path $target $READ_ATTR
    if ($h2 -eq [IntPtr](-1)) {
        $row.state_while_handle_open = 'open failed ' + [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    }
    else {
        $row.state_while_handle_open = 'still openable'
        [Chk]::CloseHandle($h2) | Out-Null
    }
    [Chk]::CloseHandle($h) | Out-Null
    $row.name_gone_after_close = -not (Test-Path -LiteralPath $target -ErrorAction SilentlyContinue)
}
Add-Row $row

# ---------------------------------------------------------------------------------- output
$writer = if ($Append) { 'AppendAllLines' } else { 'WriteAllLines' }
$lines = $rows | ForEach-Object { $_ | ConvertTo-Json -Compress -Depth 6 }
[System.IO.File]::$writer($Out, [string[]]$lines)
Write-Output ("wrote {0} checks to {1}" -f $rows.Count, $Out)
foreach ($l in $lines) { Write-Output $l }
