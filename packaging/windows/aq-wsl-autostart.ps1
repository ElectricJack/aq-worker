<#
.SYNOPSIS
  E6a: start the WSL2 distro that hosts the AQ worker at Windows logon and keep
  its VM alive while the worker is enabled.

.DESCRIPTION
  Registers a per-user logon task named "AQ WSL worker keepalive". At logon it
  runs `wsl.exe -d <Distro> --exec sleep infinity` with no window. That boots
  the VM, which starts systemd, linger, aq-sandbox-podman.service and
  aq-node@<profile>.service (or the container worker aq-worker@<profile>).
  Holding the session open also stops WSL from
  shutting down the idle distro or VM. A second trigger re-fires the task every
  minute with IgnoreNew, so a keepalive that ended (wsl --shutdown, a WSL
  update) is back within one or two minutes.

  If Task Scheduler refuses the registration (some policies need elevation for
  logon triggers), it falls back to a hidden launcher in the user's Startup
  folder.

  The worker container runs on Docker Desktop's engine, so Docker Desktop must
  also be up after logon. Its own "Start Docker Desktop when you sign in"
  setting may be off. The script therefore registers a second logon task,
  "AQ Docker Desktop at logon", that starts Docker Desktop. Docker Desktop is
  single-instance, so this does nothing if it is already running. Pass
  -NoDockerDesktop to skip it, for example when the distro runs its own
  dockerd. Until the engine is up, aq-worker@<profile>.service retries every
  10 seconds.

  Run it from Windows PowerShell, or from WSL:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File aq-wsl-autostart.ps1 -Distro Ubuntu
  To remove it, pass -Uninstall. To see what is installed, pass -Status.
#>
param(
  [string]$Distro = "Ubuntu",
  [switch]$Uninstall,
  [switch]$Status,
  [switch]$StartNow,
  [switch]$NoDockerDesktop
)
$ErrorActionPreference = "Stop"
$TaskName = "AQ WSL worker keepalive"
$DockerTaskName = "AQ Docker Desktop at logon"
$DockerDesktop = Join-Path $env:ProgramFiles "Docker\Docker\Docker Desktop.exe"
$Wsl = Join-Path $env:WINDIR "System32\wsl.exe"
$Conhost = Join-Path $env:WINDIR "System32\conhost.exe"
$StartupDir = [Environment]::GetFolderPath("Startup")
$Launcher = Join-Path $StartupDir "aq-wsl-keepalive.vbs"

if ([string]::IsNullOrWhiteSpace($Distro) -or $Distro -match '["\x00-\x1f]') {
  throw "Use a WSL distribution name without quotes or control characters."
}

function Get-AqTask { Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue }

if ($Status) {
  $t = Get-AqTask
  if ($t) { "task: $($t.State)"; ($t | Get-ScheduledTaskInfo | Select-Object LastRunTime, LastTaskResult | Format-List | Out-String).Trim() } else { "task: not registered" }
  $d = Get-ScheduledTask -TaskName $DockerTaskName -ErrorAction SilentlyContinue
  "docker desktop task: $(if ($d) { $d.State } else { 'not registered' })"
  "startup launcher: $(if (Test-Path $Launcher) { $Launcher } else { 'absent' })"
  exit 0
}

if ($Uninstall) {
  if (Get-AqTask) { Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false; "removed task '$TaskName'" }
  if (Get-ScheduledTask -TaskName $DockerTaskName -ErrorAction SilentlyContinue) { Unregister-ScheduledTask -TaskName $DockerTaskName -Confirm:$false; "removed task '$DockerTaskName'" }
  if (Test-Path $Launcher) { Remove-Item $Launcher; "removed $Launcher" }
  "the running keepalive (if any) ends with: wsl.exe -d $Distro --exec pkill -x -f 'sleep infinity'"
  exit 0
}

# `conhost --headless` keeps wsl.exe windowless; the task's own lifetime keeps the VM up.
$action = New-ScheduledTaskAction -Execute $Conhost -Argument "--headless `"$Wsl`" -d `"$Distro`" --exec sleep infinity"
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$dockerTrigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
# A second trigger re-fires every minute, starting now and continuing
# indefinitely (it keeps going after reboots too). IgnoreNew drops the extra
# runs while the keepalive is alive, so a keepalive that ended (wsl --shutdown,
# a WSL update, a killed session) is back within one or two minutes. Restart-on-failure
# can't cover this: conhost exits 0 whatever wsl.exe returns. A logon trigger's
# own repetition only starts at the next logon.
$repeat = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval (New-TimeSpan -Minutes 1)
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
  -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
  -MultipleInstances IgnoreNew -StartWhenAvailable
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger @($trigger, $repeat) -Settings $settings `
    -Principal $principal -Description "Keeps WSL distro $Distro running for the AQ worker (E6a)" -Force | Out-Null
  "registered task '$TaskName' (logon trigger, distro $Distro)"
  if ($StartNow) { Start-ScheduledTask -TaskName $TaskName; "started task" }
  if (-not $NoDockerDesktop) {
    if (Test-Path $DockerDesktop) {
      $dsettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -StartWhenAvailable
      Register-ScheduledTask -TaskName $DockerTaskName -Action (New-ScheduledTaskAction -Execute $DockerDesktop) `
        -Trigger $dockerTrigger -Settings $dsettings -Principal $principal `
        -Description "Starts Docker Desktop (the AQ worker container's engine) at logon (E6a)" -Force | Out-Null
      "registered task '$DockerTaskName'"
    } else { "Docker Desktop not found at $DockerDesktop; make sure the distro's container engine starts at boot" }
  }
} catch {
  "Task Scheduler refused registration ($($_.Exception.Message)); using the Startup folder instead"
  $vbs = 'CreateObject("WScript.Shell").Run """' + $Wsl + '"" -d ""' + $Distro + '"" --exec sleep infinity", 0, False'
  Set-Content -Path $Launcher -Value $vbs -Encoding Unicode
  "wrote $Launcher"
  if ($StartNow) { Start-Process -WindowStyle Hidden -FilePath $Wsl -ArgumentList "-d `"$Distro`" --exec sleep infinity"; "started keepalive" }
}
