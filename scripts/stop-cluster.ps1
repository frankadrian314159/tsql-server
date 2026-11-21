$procs = Get-Process tsql-cluster, tsql-shard -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Stop-Process -Force
    Write-Host "Stopped cluster processes: $($procs.Count)" -ForegroundColor Green
} else {
    Write-Host "No cluster processes were running." -ForegroundColor Yellow
}
