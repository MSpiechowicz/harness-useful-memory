# Harness Memory installer for Windows.
#
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/MSpiechowicz/harness-useful-memory/main/install.ps1 | iex"
#
# Installs harness-memory.exe in %LOCALAPPDATA%\Programs\harness-memory and puts that folder on your user PATH: a single
# binary that is the project memory MCP server of your coding agents. Register it with each harness afterwards (the
# steps are printed at the end, and are in the README).
#
# Running servers are never stopped. A harness keeps harness-memory.exe running while a session is open and Windows
# will not overwrite a running exe, but it can rename one: the old file moves aside first, and the running server keeps
# using it until its harness starts it again.
#
# Environment overrides:
#   HARNESS_MEMORY_VERSION         release tag to install (default: latest), e.g. v0.1.0
#   HARNESS_MEMORY_NO_MODIFY_PATH  set to 1 to leave your user PATH alone
$ErrorActionPreference = "Stop"

$Repo = "MSpiechowicz/harness-useful-memory"
$Bin = "harness-memory"
$Asset = "$Bin-x86_64-pc-windows-msvc.exe"
$Version = if ($env:HARNESS_MEMORY_VERSION) { $env:HARNESS_MEMORY_VERSION } else { "latest" }
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$Bin"

if (-not [Environment]::Is64BitOperatingSystem) { throw "Harness Memory requires 64-bit Windows." }

# True when the Path value $PathValue (raw, %VARS% not expanded) already lists $Dir.
function Test-OnPath([string]$PathValue, [string]$Dir) {
  if (-not $PathValue) { return $false }
  $want = $Dir.TrimEnd('\')
  foreach ($entry in ($PathValue -split ";")) {
    if ([Environment]::ExpandEnvironmentVariables($entry).TrimEnd('\') -ieq $want) { return $true }
  }
  return $false
}

# Tells running programs (Explorer, so new terminals) that the environment changed. Best effort: without it a new
# terminal can still miss the PATH change until the next sign-in.
function Send-EnvironmentChange {
  try {
    $Sig = '[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)] public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);'
    if (-not ('HarnessMemory.Native' -as [type])) {
      Add-Type -Namespace HarnessMemory -Name Native -MemberDefinition $Sig -ErrorAction Stop
    }
    $Result = [UIntPtr]::Zero
    # HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG, 5 seconds.
    [void][HarnessMemory.Native]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, "Environment", 2, 5000, [ref]$Result)
  } catch {
    Write-Verbose "Could not broadcast the environment change: $($_.Exception.Message)"
  }
}

# Release tags are vMAJOR.MINOR.PATCH: accept the number with or without the v.
if ($Version -match '^\d') { $Version = "v$Version" }
# The same rule as install.sh: vMAJOR.MINOR.PATCH with an optional -prerelease or +build. \z, not $, so a trailing newline fails.
if ($Version -ne "latest" -and $Version -cnotmatch '^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?\z') { throw "Unexpected version: $Version" }
$Base = if ($Version -eq "latest") { "https://github.com/$Repo/releases/latest/download" } else { "https://github.com/$Repo/releases/download/$Version" }

# The version comes from the environment. Dot segments (also percent-encoded) would walk out of the releases prefix.
$Url = "$Base/$Asset"
$Prefix = if ($Version -eq "latest") { "https://github.com/$Repo/releases/latest/download/" } else { "https://github.com/$Repo/releases/download/" }
if ($Url -match '\.\.|%2e' -or -not $Url.StartsWith($Prefix, [StringComparison]::Ordinal)) {
  throw "Unexpected download URL: $Url"
}

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
  # Add TLS 1.2 to what is allowed, keeping the machine's other protocols (Windows PowerShell 5.1 may default to old ones).
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $Download = Join-Path $Tmp $Asset
  Write-Host "==> Downloading $Asset ($Version)" -ForegroundColor Cyan
  try {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Download
  } catch {
    throw "Download failed: $Url"
  }

  # Nothing is installed without a checksum to compare against.
  $Sums = Join-Path $Tmp "checksums.txt"
  try {
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/checksums.txt" -OutFile $Sums
  } catch {
    throw "This release has no checksums.txt, so $Asset can't be verified. Nothing was installed."
  }
  $line = Get-Content $Sums | Where-Object { $_ -match "\s\*?$([regex]::Escape($Asset))$" } | Select-Object -First 1
  if (-not $line) { throw "checksums.txt has no entry for $Asset" }
  $expected = ($line -split "\s+")[0].ToLower()
  $actual = (Get-FileHash -Algorithm SHA256 $Download).Hash.ToLower()
  if ($expected -ne $actual) { throw "Checksum mismatch for $Asset" }
  Write-Host "==> Checksum verified" -ForegroundColor Cyan

  $Target = Join-Path $InstallDir "$Bin.exe"
  $Aside = $null
  $Installed = $false
  # The user PATH is read raw from the registry. [Environment]::GetEnvironmentVariable would expand %VARS% and writing
  # that back as a plain string would flatten the user's entries (and turn REG_EXPAND_SZ into REG_SZ).
  $EnvKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
  if (-not $EnvKey) { $EnvKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey("Environment") }
  $PathBefore = $EnvKey.GetValue("Path", $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
  $PathExisted = $null -ne $PathBefore
  $KindBefore = [Microsoft.Win32.RegistryValueKind]::ExpandString
  if ($PathExisted -and $EnvKey.GetValueKind("Path") -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
    $KindBefore = [Microsoft.Win32.RegistryValueKind]::String
  }
  $PathChanged = $false
  try {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    # Leftovers of earlier upgrades. One still locked by a running older server stays.
    Get-ChildItem -Path $InstallDir -Filter "$Bin.exe.old*" -ErrorAction SilentlyContinue |
      Remove-Item -Force -ErrorAction SilentlyContinue

    if (Test-Path -LiteralPath $Target) {
      $Aside = "$Target.old"
      $N = 1
      while (Test-Path -LiteralPath $Aside) {
        $Aside = "$Target.old.$N"
        $N++
      }
      Move-Item -Force -LiteralPath $Target -Destination $Aside
    }
    Move-Item -Force -LiteralPath $Download -Destination $Target
    $Installed = $true
    Write-Host "==> Installed $Bin to $Target" -ForegroundColor Cyan

    if ($env:HARNESS_MEMORY_NO_MODIFY_PATH -eq "1") {
      if (-not (Test-OnPath $PathBefore $InstallDir)) {
        Write-Warning "$InstallDir is not on your PATH. Add it yourself to use $Bin by name."
      }
    } elseif (-not (Test-OnPath $PathBefore $InstallDir)) {
      $NewPath = if ($PathBefore) { $PathBefore.TrimEnd(";") + ";$InstallDir" } else { $InstallDir }
      $EnvKey.SetValue("Path", $NewPath, $KindBefore)
      $PathChanged = $true
      Send-EnvironmentChange
      Write-Host "==> Added $InstallDir to your PATH (open a new terminal to use it)" -ForegroundColor Cyan
    }
  } catch {
    $Failure = $_.Exception.Message

    # Roll back: the previous PATH (raw value and kind) and the previous exe come back, or the new exe goes when there
    # was none before, so a failed run leaves the machine as it was.
    if ($PathChanged) {
      try {
        if ($PathExisted) { $EnvKey.SetValue("Path", $PathBefore, $KindBefore) } else { $EnvKey.DeleteValue("Path", $false) }
        Send-EnvironmentChange
      } catch {
        Write-Warning "Couldn't restore your user PATH: $($_.Exception.Message)"
      }
    }
    if ($Aside -and (Test-Path -LiteralPath $Aside)) {
      if (Test-Path -LiteralPath $Target) { Remove-Item -Force -LiteralPath $Target -ErrorAction SilentlyContinue }
      Move-Item -Force -LiteralPath $Aside -Destination $Target -ErrorAction SilentlyContinue
    } elseif (-not $Aside -and $Installed -and (Test-Path -LiteralPath $Target)) {
      Remove-Item -Force -LiteralPath $Target -ErrorAction SilentlyContinue
    }
    throw "Couldn't install ${Bin}: $Failure. Nothing was changed."
  } finally {
    $EnvKey.Close()
  }

  # A fresh binary that cannot run is better reported now than by the first harness.
  try {
    $Reported = & $Target --version
    Write-Host "==> $Reported" -ForegroundColor Cyan
  } catch {
    Write-Warning "$Target did not run. Try it yourself: & '$Target' --version"
  }

  Write-Host ""
  Write-Host "$Bin is installed. Register it with each coding agent you use (once per harness):"
  Write-Host ""
  Write-Host "  Claude Code   claude mcp add --scope user $Bin -- $Bin mcp"
  Write-Host "  Codex         codex mcp add $Bin -- $Bin mcp"
  Write-Host "  OMP, PI, OpenCode: add the entry from the README, section 'Register with your harness'"
  Write-Host ""
  Write-Host "README: https://github.com/$Repo#register-with-your-harness"
  Write-Host "Harness Desktop registers it for you. Re-run this installer to update."
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
