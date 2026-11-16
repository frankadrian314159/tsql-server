#!/bin/sh
# Autoscaler "down" command: delete the node of the shard that was drained. $TSQL_SHARD_ADDR is its host:port.
# Deleting an instance makes GCE stop it with an ACPI shutdown, which systemd turns into SIGTERM for tsql-shard (a graceful stop).
set -e
IP="${TSQL_SHARD_ADDR%%:*}"
N=$(gcloud compute instances list --filter "networkInterfaces[0].networkIP=$IP" --format 'value(name)')
[ -n "$N" ] && gcloud compute instances delete "$N" --zone "${ZONE:-us-central1-a}" --quiet
