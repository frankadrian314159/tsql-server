param(
    [string]$Bind = "0.0.0.0",
    [int]$Port = 1433
)

$ClusterDir = "C:\Users\frank\Projects\tsql-server\data\cluster"
Push-Location $ClusterDir

try {
    # Stop any existing processes
    Get-Process tsql-cluster, tsql-shard -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500

    Write-Host "Starting shard 0 on 127.0.0.1:50051 (s0)..."
    Start-Process -FilePath ".\bin\tsql-shard.exe" -ArgumentList "--id 0 --listen 127.0.0.1:50051 --data s0" -RedirectStandardOutput "shard0.log" -RedirectStandardError "shard0.err.log" -WindowStyle Hidden

    Write-Host "Starting shard 1 on 127.0.0.1:50052 (s1)..."
    Start-Process -FilePath ".\bin\tsql-shard.exe" -ArgumentList "--id 1 --listen 127.0.0.1:50052 --data s1" -RedirectStandardOutput "shard1.log" -RedirectStandardError "shard1.err.log" -WindowStyle Hidden

    Write-Host "Starting shard 2 on 127.0.0.1:50053 (s2)..."
    Start-Process -FilePath ".\bin\tsql-shard.exe" -ArgumentList "--id 2 --listen 127.0.0.1:50053 --data s2" -RedirectStandardOutput "shard2.log" -RedirectStandardError "shard2.err.log" -WindowStyle Hidden

    Start-Sleep -Seconds 1

    Write-Host "Starting control node on ${Bind}:${Port}..."
    Start-Process -FilePath ".\bin\tsql-cluster.exe" -ArgumentList "--config cluster.json --state cluster.state --listen $Port --bind $Bind" -RedirectStandardOutput "cluster.log" -RedirectStandardError "cluster.err.log" -WindowStyle Hidden

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
