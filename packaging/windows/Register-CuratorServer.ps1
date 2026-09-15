[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('CurrentUser', 'AllUsers')]
    [string]$Scope,

    [Parameter(Mandatory = $true)]
    [string]$ServerPath,

    [switch]$EnablePhar
)

$ErrorActionPreference = 'Stop'
$resolvedServer = (Resolve-Path -LiteralPath $ServerPath).Path
if (-not (Test-Path -LiteralPath $resolvedServer -PathType Leaf)) {
    throw "Curator Server executable was not found: $ServerPath"
}

if ($Scope -eq 'AllUsers') {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'All-users Curator Server installation requires elevation.'
    }
    if (Get-Service -Name 'CuratorServer' -ErrorAction SilentlyContinue) {
        throw 'The Curator Server Windows service already exists. Uninstall or stop it before registering another server.'
    }
    $binPath = '"{0}" --background --install-scope all-users' -f $resolvedServer
    & sc.exe create CuratorServer binPath= $binPath start= auto DisplayName= 'Curator Server'
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the Curator Server Windows service.' }
    & sc.exe description CuratorServer 'Curator Server local and Tailnet media backend'
    $scopeArgument = 'all-users'
    $startServer = {
        & sc.exe start CuratorServer
        if ($LASTEXITCODE -ne 0) { throw 'Curator Server service was created but did not start.' }
    }
} else {
    $taskName = 'Curator Server (Current User)'
    $taskAction = New-ScheduledTaskAction -Execute $resolvedServer -Argument '--background --install-scope current-user'
    $taskTrigger = New-ScheduledTaskTrigger -AtLogOn
    $taskSettings = New-ScheduledTaskSettingsSet -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Days 0)
    Register-ScheduledTask -TaskName $taskName -Action $taskAction -Trigger $taskTrigger -Settings $taskSettings -Description 'Curator Server local and Tailnet backend' -Force | Out-Null
    $scopeArgument = 'current-user'
    $startServer = { Start-ScheduledTask -TaskName $taskName }
}

# This records only consent/intent before the service/task first launches.
# The Server then evaluates managed P-HAR setup after it starts, so model
# downloads cannot break the OS installation transaction.
if ($EnablePhar) {
    & $resolvedServer phar-intent --enabled true --install-scope $scopeArgument
    if ($LASTEXITCODE -ne 0) { throw 'Curator Server was installed, but P-HAR intent could not be recorded.' }
}

& $startServer
