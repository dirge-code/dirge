#!/bin/bash
# Simulate a Prefect flow run completing and notifying vigil via harbinger.
# This represents what a Prefect task would do as its last step.
echo '{"engine":"prefect","flow":"echo-flow","run_id":"run-001","status":"completed"}' | nc -w1 127.0.0.1 9092
echo "prefect echo sent (port 9092)"
