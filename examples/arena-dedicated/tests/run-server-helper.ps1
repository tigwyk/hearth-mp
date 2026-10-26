# Runs the Godot HearthServer helper against the v0.5 fleet e2e fixture.
#
# Prerequisites: the fleet gateway is running in examples/arena-dedicated with
# `.dev.vars` present. The sample's compile-time allocator is the container
# mode, so the fleet fixture must start the gateway in webhook mode:
#
#   cd examples/arena-dedicated
#   npx wrangler@4 dev --port 8790 --persist-to .wrangler/fleet-state `
#     --enable-containers=false `
#     --var HEARTH_ALLOCATOR_MODE:webhook `
#     --var HEARTH_ALLOCATOR_URL:http://127.0.0.1:8791/allocate `
#     --var HEARTH_ALLOW_INSECURE_ENDPOINTS:1
#
# Windows and Linux: `wrangler dev` cannot run Cloudflare Containers locally on
# Windows (it needs WSL), so `--enable-containers=false` is required there even
# though the fleet path never starts a container. On Linux the flag is optional
# but harmless; keep the same command so the fixture line is identical on both
# hosts. `HEARTH_ALLOCATOR_MODE=webhook` + the two allocator vars are required on
# both hosts for the fleet path.
#
# Then:
#
#   pwsh -File examples/arena-dedicated/tests/run-server-helper.ps1
#
# The script starts `node e2e-fleet.mjs --print-session` (one fresh lobby
# session with two players; it exits without ending the session), exports the
# session env + the two tickets, imports the godot-client project, and runs
# tests/server_helper.gd twice: first without the session env (from_env() must
# be null) and then with it (redeem / report / heartbeat).
param(
    [string]$GodotPath = 'C:\Program Files\Godot\Godot.exe',
    [string]$Url = 'http://127.0.0.1:8790'
)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$dedicated = (Resolve-Path (Join-Path $here '..')).Path
$project = Join-Path $dedicated 'godot-client'
$base = $Url.TrimEnd('/')

function Invoke-Godot([string[]]$GodotArguments) {
    $stdout = Join-Path $env:TEMP ("hearth-godot-" + [guid]::NewGuid().ToString('N') + '.out')
    $stderr = $stdout + '.err'
    $process = Start-Process -FilePath $GodotPath -ArgumentList $GodotArguments -Wait -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    Get-Content $stdout -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
    if ((Get-Item $stderr -ErrorAction SilentlyContinue).Length -gt 0) {
        Get-Content $stderr | ForEach-Object { Write-Host $_ }
    }
    Remove-Item $stdout, $stderr -ErrorAction SilentlyContinue
    return $process.ExitCode
}

# --- gateway reachable? ------------------------------------------------------
try {
    $schema = Invoke-WebRequest -Uri "$base/schema/arena" -UseBasicParsing -TimeoutSec 5
    if ($schema.StatusCode -ne 200) {
        throw "schema route answered $($schema.StatusCode)"
    }
}
catch {
    Write-Error ("the fleet gateway is not reachable at $base. Start it first: " +
        "cd examples/arena-dedicated; npx wrangler dev --port 8790 --persist-to .wrangler/state " +
        "($($_.Exception.Message))")
    exit 1
}

# --- allocator secret must match the room's .dev.vars ------------------------
$devVarsPath = Join-Path $dedicated '.dev.vars'
if (-not (Test-Path $devVarsPath)) {
    Write-Error "examples/arena-dedicated/.dev.vars is missing; copy .dev.vars.example and fill both secrets"
    exit 1
}
$devVars = @{}
Get-Content $devVarsPath | ForEach-Object {
    if ($_ -match '^\s*([A-Za-z0-9_]+)\s*=\s*(.+?)\s*$') {
        $devVars[$Matches[1]] = $Matches[2]
    }
}
$env:HEARTH_ALLOCATOR_SECRET = $devVars['HEARTH_ALLOCATOR_SECRET']
$env:HEARTH_URL = $base

# --- one fresh session fixture ----------------------------------------------
Write-Host "== creating a fleet session fixture (e2e-fleet.mjs --print-session)"
$lines = & node (Join-Path $dedicated 'e2e-fleet.mjs') '--print-session' 2>&1
$fixtureExit = $LASTEXITCODE
$lines | ForEach-Object { Write-Host $_ }
if ($fixtureExit -ne 0) {
    Write-Error "e2e-fleet.mjs --print-session failed (exit $fixtureExit)"
    exit 1
}
$jsonLine = @($lines | Where-Object { "$_".TrimStart().StartsWith('{') }) | Select-Object -Last 1
if (-not $jsonLine) {
    Write-Error "e2e-fleet.mjs --print-session printed no fixture JSON"
    exit 1
}
$fixture = $jsonLine | ConvertFrom-Json
Write-Host "== fixture session $($fixture.sid) for room $($fixture.room)"

# --- import the Godot project once -----------------------------------------
Write-Host "== syncing the addon and importing the godot-client project"
& (Join-Path $project 'scripts\sync-addon.ps1')
$importExit = Invoke-Godot @('--headless', '--import', '--path', $project)
if ($importExit -ne 0) {
    Write-Error "godot --headless --import failed (exit $importExit)"
    exit 1
}

$sessionVariables = @('HEARTH_SID', 'HEARTH_SESSION_SECRET', 'HEARTH_REPORT_URL', 'HEARTH_TICKETS')
$saved = @{}
foreach ($name in $sessionVariables) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name)
    Remove-Item "Env:$name" -ErrorAction SilentlyContinue
}

$nullCaseExit = Invoke-Godot @('--headless', '--path', $project, '-s', 'tests/server_helper.gd')
if ($nullCaseExit -ne 0) {
    Write-Error "server_helper failed without the session env (exit $nullCaseExit)"
    exit 1
}

$env:HEARTH_SID = $fixture.sid
$env:HEARTH_SESSION_SECRET = $fixture.secret
$env:HEARTH_REPORT_URL = $fixture.report_url
$env:HEARTH_TICKETS = ConvertTo-Json -InputObject @($fixture.tickets) -Compress
$sessionCaseExit = Invoke-Godot @('--headless', '--path', $project, '-s', 'tests/server_helper.gd')

foreach ($name in $sessionVariables) {
    if ($null -eq $saved[$name]) {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    else {
        Set-Item "Env:$name" $saved[$name]
    }
}

if ($sessionCaseExit -ne 0) {
    Write-Error "server_helper failed with the session env (exit $sessionCaseExit)"
    exit 1
}
Write-Host "server_helper runner: ok (null case 0, session case 0)"
exit 0
