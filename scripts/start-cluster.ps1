param(
    [string]$Bind = "0.0.0.0",
    [int]$Port = 1433,
    [string]$SaPassword = ""
)

$ClusterDir = "C:\Users\frank\Projects\tsql-server\data\cluster"
Push-Location $ClusterDir

function Start-DetachedProcess {
    param(
        [string]$CommandLine,
        [string]$WorkingDir
    )
    $res = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
        CommandLine = "cmd.exe /c `"$CommandLine`""
        CurrentDirectory = $WorkingDir
    }
    return $res.ProcessId
}

try {
    # Stop any existing processes
    Get-Process tsql-cluster, tsql-shard -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500

    Write-Host "Starting shard 0 on 127.0.0.1:50051 (s0)..."
    $null = Start-DetachedProcess -CommandLine "bin\tsql-shard.exe --id 0 --listen 127.0.0.1:50051 --data s0 > shard0.log 2> shard0.err.log" -WorkingDir $ClusterDir

    Write-Host "Starting shard 1 on 127.0.0.1:50052 (s1)..."
    $null = Start-DetachedProcess -CommandLine "bin\tsql-shard.exe --id 1 --listen 127.0.0.1:50052 --data s1 > shard1.log 2> shard1.err.log" -WorkingDir $ClusterDir

    Write-Host "Starting shard 2 on 127.0.0.1:50053 (s2)..."
    $null = Start-DetachedProcess -CommandLine "bin\tsql-shard.exe --id 2 --listen 127.0.0.1:50053 --data s2 > shard2.log 2> shard2.err.log" -WorkingDir $ClusterDir

    Start-Sleep -Seconds 1

    Write-Host "Starting control node on ${Bind}:${Port}..."
    $pwArg = if ($SaPassword) { "--sa-password `"$SaPassword`"" } else { "" }
    $null = Start-DetachedProcess -CommandLine "bin\tsql-cluster.exe --config cluster.json --state cluster.state --listen $Port --bind $Bind $pwArg > cluster.log 2> cluster.err.log" -WorkingDir $ClusterDir

    Start-Sleep -Seconds 2

    $procs = Get-Process tsql-cluster, tsql-shard -ErrorAction SilentlyContinue
    if ($procs) {
        Write-Host "Cluster is running:" -ForegroundColor Green
        $procs | Select-Object Id, ProcessName
    } else {
        Write-Warning "Failed to start cluster processes. Check cluster.err.log and shard*.err.log"
    }
} finally {
    Pop-Location
}
