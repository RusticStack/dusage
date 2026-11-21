# Build the Windows benchmark fixtures under -Root.
#
#   powershell -ExecutionPolicy Bypass -File bench/windows/fixtures.ps1 -Root D:\dusage-bench
#
# Everything here is created by this script inside -Root and nothing outside it is touched.
# The trees are:
#
#   tree1m     1000 directories x 1000 zero-length files      (1,001,000 entries)
#   flat100k   one directory, 100 000 zero-length files       (one huge index)
#   tree1k     a wide fan-out at two sizes, for the per-directory cost
#   srcA       500 directories x 40 files                     (delete fixture, regenerated)
#   srcB       100 directories x 50 files of 128 KiB          (delete fixture, data-bearing)
#   text       300 directories x 20 files of 32 KiB of text   (search fixture)
#   fid        compressed / sparse / resident / ADS / hardlink / junction fixtures
#   long       a path over MAX_PATH, written through the verbatim prefix
#
# The file loops run in C# through Add-Type: a PowerShell loop over a million files costs
# minutes, and the fixtures are meant to be cheap to rebuild.

param(
    [string]$Root = 'D:\dusage-bench',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;

public static class Fixture {
    public static long ZeroTree(string root, int dirs, int per) {
        long n = 0;
        for (int d = 0; d < dirs; d++) {
            string dp = Path.Combine(root, "d" + d.ToString("D5"));
            Directory.CreateDirectory(dp);
            for (int i = 0; i < per; i++) {
                string p = Path.Combine(dp, "f" + i.ToString("D6") + ".dat");
                using (File.Create(p, 1, FileOptions.None, null)) { }
                n++;
            }
        }
        return n;
    }

    public static long ZeroFlat(string root, int n) {
        Directory.CreateDirectory(root);
        for (int i = 0; i < n; i++) {
            using (File.Create(Path.Combine(root, "f" + i.ToString("D6") + ".dat"), 1, FileOptions.None, null)) { }
        }
        return n;
    }

    public static long SizedTree(string root, int dirs, int per, int size) {
        byte[] buf = new byte[size];
        var rng = new Random(1234);
        rng.NextBytes(buf);
        long n = 0;
        for (int d = 0; d < dirs; d++) {
            string dp = Path.Combine(root, "s" + d.ToString("D4"));
            Directory.CreateDirectory(dp);
            for (int i = 0; i < per; i++) {
                File.WriteAllBytes(Path.Combine(dp, "f" + i.ToString("D4") + ".bin"), buf);
                n++;
            }
        }
        return n;
    }

    // Source-like text, deterministic, with a rare token so a search case is meaningful.
    public static long TextTree(string root, int dirs, int per, int size) {
        var sb = new StringBuilder(size);
        var rng = new Random(99);
        string[] words = { "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta" };
        while (sb.Length < size - 64) {
            sb.Append(words[rng.Next(words.Length)]);
            sb.Append(' ');
            if (rng.Next(12) == 0) sb.Append("asyncpreemptoff ");
            if (rng.Next(9) == 0) sb.Append('\n');
        }
        string body = sb.ToString();
        long n = 0;
        for (int d = 0; d < dirs; d++) {
            string dp = Path.Combine(root, "t" + d.ToString("D4"));
            Directory.CreateDirectory(dp);
            for (int i = 0; i < per; i++) {
                File.WriteAllText(Path.Combine(dp, "f" + i.ToString("D4") + ".txt"), body);
                n++;
            }
        }
        return n;
    }
}
'@

# A fixture is present when its sentinel exists: counting a million entries to decide is
# slower than building the tree, so the last entry each builder writes is the marker.
function Ensure([string]$Name, [string]$Sentinel, [scriptblock]$Build) {
    $p = Join-Path $Root $Name
    if (-not $Force -and (Test-Path (Join-Path $p $Sentinel))) {
        Write-Output ("fixture {0}: present" -f $Name)
        return
    }
    if (Test-Path $p) { Remove-Item -LiteralPath $p -Recurse -Force }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $Build $p
    $sw.Stop()
    Write-Output ("fixture {0}: built in {1:N0} ms" -f $Name, $sw.Elapsed.TotalMilliseconds)
}

New-Item -ItemType Directory -Force -Path $Root | Out-Null

Ensure 'tree1m' 'd00999' { param($p) [Fixture]::ZeroTree($p, 1000, 1000) | Out-Null }
Ensure 'flat100k' 'f099999.dat' { param($p) [Fixture]::ZeroFlat($p, 100000) | Out-Null }
Ensure 'tree1k_d10' 'd00999' { param($p) [Fixture]::ZeroTree($p, 1000, 10) | Out-Null }
Ensure 'tree1k_d100' 'd00999' { param($p) [Fixture]::ZeroTree($p, 1000, 100) | Out-Null }
Ensure 'srcA' 'd00499' { param($p) [Fixture]::ZeroTree($p, 500, 40) | Out-Null }
Ensure 'srcB' 's0099' { param($p) [Fixture]::SizedTree($p, 100, 50, 131072) | Out-Null }
Ensure 'text' 't0299' { param($p) [Fixture]::TextTree($p, 300, 20, 32768) | Out-Null }

# Allocation-fidelity fixture: one row per on-disk case the roadmap asks about.
$fid = Join-Path $Root 'fid'
if ($Force -or -not (Test-Path (Join-Path $fid 'ads.txt'))) {
    if (Test-Path $fid) { Remove-Item -LiteralPath $fid -Recurse -Force }
    New-Item -ItemType Directory -Force -Path (Join-Path $fid 'sub') | Out-Null
    [System.IO.File]::WriteAllBytes((Join-Path $fid 'resident.bin'), [byte[]](1))
    [System.IO.File]::WriteAllBytes((Join-Path $fid 'plain.bin'), (New-Object byte[] 1048576))
    Copy-Item (Join-Path $fid 'plain.bin') (Join-Path $fid 'comp.bin')
    & compact.exe /c (Join-Path $fid 'comp.bin') | Out-Null
    # A sparse file must be written then declared sparse, then its range freed.
    $sp = Join-Path $fid 'sparse.bin'
    [System.IO.File]::WriteAllBytes($sp, (New-Object byte[] 1048576))
    & fsutil.exe sparse setflag $sp | Out-Null
    & fsutil.exe sparse setrange $sp 0 1048576 | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $fid 'ads.txt'), 'base')
    $ads = Join-Path $fid 'ads.txt:hidden'
    [System.IO.File]::WriteAllBytes($ads, (New-Object byte[] 1048576))
    [System.IO.File]::WriteAllText((Join-Path $fid 'hl_a.txt'), 'hardlink content')
    & cmd.exe /c mklink /H (Join-Path $fid 'hl_b.txt') (Join-Path $fid 'hl_a.txt') | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $fid 'sub\inner.txt'), 'inner')
    & cmd.exe /c mklink /J (Join-Path $fid 'junction') (Join-Path $fid 'sub') | Out-Null
    Write-Output 'fixture fid: built'
}

# A tree whose deepest path is over MAX_PATH (393 characters), written verbatim.
$long = Join-Path $Root 'long'
if ($Force -or -not (Test-Path $long)) {
    if (Test-Path $long) { Remove-Item -LiteralPath $long -Recurse -Force }
    [System.IO.Directory]::CreateDirectory($long) | Out-Null
    $bs = [char]92
    $seg = 's' * 120
    $cur = $long
    for ($i = 0; $i -lt 3; $i++) {
        $cur = $cur + $bs + $seg + $i
        [System.IO.Directory]::CreateDirectory($cur) | Out-Null
        [System.IO.File]::WriteAllText(($cur + $bs + 'f' + $i + '.txt'), 'x')
    }
    Write-Output ('fixture long: deepest plain path ' + ($cur.Length) + ' characters')
}

Write-Output ''
Write-Output "fixtures under ${Root}: " + ((Get-ChildItem -LiteralPath $Root -Directory | Select-Object -ExpandProperty Name) -join ', ')
