[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('CurrentUser', 'AllUsers')]
    [string]$Scope
)

$ErrorActionPreference = 'Stop'
if ($Scope -eq 'AllUsers') {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Removing the all-users Curator Server service requires elevation.'
    }
    $service = Get-Service -Name 'CuratorServer' -ErrorAction SilentlyContinue
    if ($service) {
        if ($service.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
            & sc.exe stop CuratorServer | Out-Null
            if ($LASTEXITCODE -notin @(0, 1062)) {
                throw 'Could not stop the Curator Server Windows service.'
            }
            $service.Refresh()
            $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, (New-TimeSpan -Seconds 30))
        }
        & sc.exe delete CuratorServer
        if ($LASTEXITCODE -ne 0) { throw 'Could not remove the Curator Server Windows service.' }
    }
} else {
    Stop-ScheduledTask -TaskName 'Curator Server (Current User)' -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName 'Curator Server (Current User)' -Confirm:$false -ErrorAction SilentlyContinue
}

# Deliberately preserve %LocalAppData%/Curator and %ProgramData%/Curator.
# Uninstalling a service must never erase a library, archive, backup, or
# managed P-HAR environment.
