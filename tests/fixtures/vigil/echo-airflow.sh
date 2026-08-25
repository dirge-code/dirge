#!/bin/bash
# Simulate an Airflow DAG run completing and notifying vigil via harbinger.
echo '{"engine":"airflow","dag":"echo-dag","run_id":"scheduled__2026-01-01","status":"success"}' | nc -w1 127.0.0.1 9092
echo "airflow echo sent (port 9092)"
