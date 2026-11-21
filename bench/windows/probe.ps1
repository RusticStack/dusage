# Per-file on-disk truth for one path, as one JSON object per line.
# Scratch probe: GetCompressedFileSizeW, FILE_STANDARD_INFO AllocationSize, FileIdInfo,
# the stream list, and the attributes/reparse tag, which are the four numbers the
# allocation-fidelity and FileId questions need side by side.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Paths)

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Probe {
    [StructLayout(LayoutKind.Sequential)]
    public struct StandardInfo {
        public long AllocationSize;
        public long EndOfFile;
        public uint NumberOfLinks;
        public byte DeletePending;
        public byte Directory;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct FileIdInfo {
        public ulong VolumeSerialNumber;
        public ulong HighPart;
        public ulong LowPart;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct StreamData {
        public long StreamSize;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 296)]
        public string cStreamName;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr CreateFileW(string name, uint access, uint share,
        IntPtr sec, uint disp, uint flags, IntPtr templ);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandleEx(IntPtr h, int cls, out StandardInfo info, uint size);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandleEx(IntPtr h, int cls, out FileIdInfo info, uint size);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool CloseHandle(IntPtr h);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern uint GetCompressedFileSizeW(string name, out uint high);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr FindFirstStreamW(string name, int level, out StreamData data, uint flags);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern bool FindNextStreamW(IntPtr h, out StreamData data);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FindClose(IntPtr h);
}
'@

$FILE_READ_ATTRIBUTES = 0x80
$FILE_SHARE_ALL = 7
$OPEN_EXISTING = 3
$FILE_FLAG_BACKUP_SEMANTICS = 0x02000000
$FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000
$FILE_STANDARD_INFO_CLASS = 1
$FILE_ID_INFO_CLASS = 18

foreach ($p in $Paths) {
    # A verbatim or device path is passed through untouched: .NET's normaliser rewrites the
    # prefix, and GetFullPath refuses a plain path over MAX_PATH, which is the case this
    # probe exists to compare against.
    if ($p.StartsWith('\?') -or $p.StartsWith('\.')) {
        $full = $p
    }
    else {
        $rp = Resolve-Path -LiteralPath $p -ErrorAction SilentlyContinue
        if ($rp) { $full = $rp.ProviderPath } else { $full = $p }
    }
    $obj = [ordered]@{
        path = $p
    }
    $h = [Probe]::CreateFileW($full, $FILE_READ_ATTRIBUTES, $FILE_SHARE_ALL, [IntPtr]::Zero,
        $OPEN_EXISTING, ($FILE_FLAG_BACKUP_SEMANTICS -bor $FILE_FLAG_OPEN_REPARSE_POINT), [IntPtr]::Zero)
    if ($h -eq [IntPtr](-1)) {
        $obj.error = "open " + [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    }
    else {
        $std = New-Object Probe+StandardInfo
        if ([Probe]::GetFileInformationByHandleEx($h, $FILE_STANDARD_INFO_CLASS, [ref]$std, 24)) {
            $obj.end_of_file = $std.EndOfFile
            $obj.allocation_size = $std.AllocationSize
            $obj.links = $std.NumberOfLinks
            $obj.is_dir = [bool]$std.Directory
        }
        $fid = New-Object Probe+FileIdInfo
        if ([Probe]::GetFileInformationByHandleEx($h, $FILE_ID_INFO_CLASS, [ref]$fid, 24)) {
            $obj.file_id = ("{0}-{1:x16}{2:x16}" -f $fid.VolumeSerialNumber, $fid.HighPart, $fid.LowPart)
            $obj.file_id_128 = ("{0:x16}{1:x16}" -f $fid.HighPart, $fid.LowPart)
        }
        [Probe]::CloseHandle($h) | Out-Null
    }
    [uint32]$high = 0
    [uint32]$low = [Probe]::GetCompressedFileSizeW($full, [ref]$high)
    if ($low -eq [uint32]::MaxValue -and $high -eq [uint32]::MaxValue) {
        $obj.compressed_size = $null
    }
    else {
        $obj.compressed_size = ([int64]$high -shl 32) -bor [int64]$low
    }
    $it = Get-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue
    if ($it) {
        $obj.attributes = $it.Attributes.ToString()
        $obj.link_type = $it.LinkType
        $obj.target = $it.Target
    }
    $sd = New-Object Probe+StreamData
    $fh = [Probe]::FindFirstStreamW($full, 0, [ref]$sd, 0)
    if ($fh -ne [IntPtr](-1)) {
        $streams = @()
        while ($true) {
            $streams += [ordered]@{ name = $sd.cStreamName; size = $sd.StreamSize }
            $sd = New-Object Probe+StreamData
            if (-not [Probe]::FindNextStreamW($fh, [ref]$sd)) { break }
        }
        [Probe]::FindClose($fh) | Out-Null
        $obj.streams = $streams
    }
    $obj | ConvertTo-Json -Compress -Depth 6
}
