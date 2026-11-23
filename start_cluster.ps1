# Stop any existing tsql-shard or tsql-cluster processes
Get-Process tsql-shard, tsql-cluster -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

# Start 3 shard nodes
$s0 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 0 --listen 127.0.0.1:50051" -PassThru -NoNewWindow
$s1 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 1 --listen 127.0.0.1:50052" -PassThru -NoNewWindow
$s2 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 2 --listen 127.0.0.1:50053" -PassThru -NoNewWindow

Start-Sleep -Seconds 1

# Start the cluster control node
& "$PSScriptRoot\target\release\tsql-cluster.exe" --config "$PSScriptRoot\cluster.json" --listen 1433 --sa-password Str0ng!pw
