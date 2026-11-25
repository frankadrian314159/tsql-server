# Stop any existing tsql-shard or tsql-cluster processes
Get-Process tsql-shard, tsql-cluster -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

# Certificates for the connections between nodes, made once
$certs = "$PSScriptRoot\data\certs"
if (-not (Test-Path "$certs\ca.pem")) {
    & "$PSScriptRoot\target\release\tsql-cluster.exe" --make-certs $certs control shard0 shard1 shard2
    if ($LASTEXITCODE -ne 0) { throw "Could not make the certificates; build with: cargo build --release --workspace" }
}
function Get-NodeFlags([string]$Name) { "--node-ca `"$certs\ca.pem`" --node-cert `"$certs\$Name.pem`" --node-key `"$certs\$Name.key`"" }

# Start 3 shard nodes
$s0 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 0 --listen 127.0.0.1:50051 $(Get-NodeFlags shard0)" -PassThru -NoNewWindow
$s1 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 1 --listen 127.0.0.1:50052 $(Get-NodeFlags shard1)" -PassThru -NoNewWindow
$s2 = Start-Process -FilePath "$PSScriptRoot\target\release\tsql-shard.exe" -ArgumentList "--id 2 --listen 127.0.0.1:50053 $(Get-NodeFlags shard2)" -PassThru -NoNewWindow

Start-Sleep -Seconds 1

# Start the cluster control node
& "$PSScriptRoot\target\release\tsql-cluster.exe" --config "$PSScriptRoot\cluster.json" --node-ca "$certs\ca.pem" --node-cert "$certs\control.pem" --node-key "$certs\control.key" --listen 1433 --sa-password Str0ng!pw
