<#
.SYNOPSIS
  Spec 0007: write the Windows host CPU, memory and disk load to a file so a
  WSL2 worker can report the load of the whole machine, not only of the WSL VM.

.DESCRIPTION
  Every 5 seconds the probe writes C:\ProgramData\agent-q\host-load.json
  atomically (a temporary file, then a move). The worker reads it through
  /mnt/c/ProgramData/agent-q, or through a read-only mount in the container,
  and uses it when it is at most 30 seconds old. Without a fresh file the
  worker reports the WSL VM load and says the probe is stale or missing.

  -Install registers a per-user logon task named "AQ host load probe" that runs
  a copy of this script (no elevation needed). -Uninstall removes the task and
  the copy. -Status prints the task state and the age of host-load.json.
  -Once writes one sample and exits. -StartNow starts the task after -Install.

  The probe uses Win32_PerfFormattedData_PerfOS_Processor, not Get-Counter,
  because counter names are localized.

  Run from WSL:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File aq-host-load.ps1 -Install -StartNow
  Run from Windows PowerShell:
    powershell -NoProfile -ExecutionPolicy Bypass -File aq-host-load.ps1 -Install -StartNow
#>
param(
  [switch]$Install,
  [switch]$Uninstall,
  [switch]$Status,
  [switch]$Once,
  [switch]$StartNow,
  [string]$OutDir = (Join-Path $env:ProgramData "agent-q")
)
$ErrorActionPreference = "Stop"
$TaskName = "AQ host load probe"
$Out = Join-Path $OutDir "host-load.json"
# The script copy lives in the user profile so another local user cannot replace it.
$CopyDir = Join-Path $env:LOCALAPPDATA "agent-q"
$Copy = Join-Path $CopyDir "aq-host-load.ps1"

function Write-Sample {
  $cpu  = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'").PercentProcessorTime
  $os   = Get-CimInstance Win32_OperatingSystem
  $cpus = (Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
  $disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='$($env:SystemDrive)'"
  $total = [uint64]$os.TotalVisibleMemorySize * 1024; $free = [uint64]$os.FreePhysicalMemory * 1024
  $s = [ordered]@{ schema = "aq-host-load-1"; observed_at_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();
    logical_cpus = [int]$cpus; cpu_busy_bps = [int][Math]::Min(10000, [Math]::Max(0, [int]$cpu * 100));
    memory_total_bytes = $total; memory_used_bytes = $total - $free;
    disk_total_bytes = [uint64]$disk.Size; disk_free_bytes = [uint64]$disk.FreeSpace }
  New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
  $tmp = "$Out.tmp"; [IO.File]::WriteAllText($tmp, ($s | ConvertTo-Json -Compress))
  # File.Replace is atomic on an existing destination; Move-Item -Force is not on PowerShell 5.1.
  if (Test-Path $Out) { [IO.File]::Replace($tmp, $Out, $null) } else { Move-Item $tmp $Out }
}

if ($Once) { Write-Sample; exit 0 }

if ($Status) {
  $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
  if ($task) { "task: $($task.State)" } else { "task: not installed" }
  if (Test-Path $Out) {
    $age = [int]((Get-Date) - (Get-Item $Out).LastWriteTime).TotalSeconds
    "host-load.json: $age s old"
  } else { "host-load.json: missing" }
  exit 0
}

if ($Uninstall) {
  if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
  }
  Remove-Item -Force -ErrorAction SilentlyContinue $Copy
  exit 0
}

if ($Install) {
  New-Item -ItemType Directory -Force -Path $OutDir, $CopyDir | Out-Null
  Copy-Item -Force $PSCommandPath $Copy
  $trigger  = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
  $action   = New-ScheduledTaskAction -Execute "powershell.exe" -Argument "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$Copy`" -OutDir `"$OutDir`""
  $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
  $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName $TaskName -Trigger $trigger -Action $action -Settings $settings -Principal $principal -Force | Out-Null
  if ($StartNow) { Start-ScheduledTask -TaskName $TaskName }
  exit 0
}

# Default (the task's action): sample every 5 seconds until the task ends.
while ($true) { try { Write-Sample } catch { }; Start-Sleep -Seconds 5 }
