#!/bin/sh
# Autoscaler "up" command: make a shard node, wait until it answers, print its address (last line).
# Used as:  tsql-cluster ... --autoscale --scale-up-cmd /usr/local/bin/scale-up.sh --scale-down-cmd /usr/local/bin/scale-down.sh
set -e
N="tsql-shard-$(date +%s)"
gcloud compute instances create "$N" --zone "${ZONE:-us-central1-a}" --machine-type "${MACHINE:-n2-standard-16}" \
  --image-family "${IMAGE_FAMILY:-tsql-shard}" --boot-disk-size 100GB --boot-disk-type pd-ssd \
  --metadata "shard-id=${SHARD_ID:-new}" --quiet >&2
IP=$(gcloud compute instances describe "$N" --zone "${ZONE:-us-central1-a}" --format 'get(networkInterfaces[0].networkIP)')
# wait for the shard's metrics endpoint (it is up when it answers)
for i in $(seq 1 60); do curl -fs "http://$IP:9101/health" >/dev/null 2>&1 && break; sleep 5; done
echo "$IP:50051"
