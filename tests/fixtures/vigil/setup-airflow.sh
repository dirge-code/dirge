#!/usr/bin/env bash
# setup-airflow.sh — Configure Airflow for vigil e2e testing and create a failing DAG.
#
# Usage: ./setup-airflow.sh
#   Requires: Airflow running on localhost:8081 (podman-compose up -d)
#   Idempotent: overwrites the DAG file and re-triggers each run

set -euo pipefail

CONTAINER="vigil-airflow"
AIRFLOW_URL="http://localhost:8081"
AIRFLOW_USER="admin"
AIRFLOW_PASS="admin"

echo "=== Airflow e2e setup ==="

# 1. Fix auth: Airflow 2.10.5 standalone uses session auth by default, but the
#    plugin expects basic auth. Switch to basic_auth backend.
echo "  Configuring basic auth..."
podman exec "$CONTAINER" bash -c '
  sed -i "s|auth_backends = airflow.api.auth.backend.session|auth_backends = airflow.api.auth.backend.basic_auth|" /opt/airflow/airflow.cfg
' 2>/dev/null || true

# Restart to pick up auth change
echo "  Restarting Airflow..."
podman restart "$CONTAINER" > /dev/null 2>&1
sleep 15

# 2. Reset the admin password (airflow standalone may override the compose-created password)
echo "  Resetting admin password..."
podman exec "$CONTAINER" bash -c "
  airflow users reset-password -u admin -p $AIRFLOW_PASS 2>&1
" | tail -1

# Verify auth works
if ! curl -sf -u "$AIRFLOW_USER:$AIRFLOW_PASS" "$AIRFLOW_URL/api/v1/dags" > /dev/null 2>&1; then
  echo "  ERROR: basic auth not working after password reset. Check airflow.cfg auth_backends." >&2
  exit 1
fi
echo "  Auth verified (admin:$AIRFLOW_PASS)"

# 3. Create a DAG file that always fails
echo "  Creating failing_dag..."
podman exec "$CONTAINER" bash -c 'cat > /opt/airflow/dags/failing_dag.py << "DAGEOF"
from airflow import DAG
from airflow.operators.bash import BashOperator
from datetime import datetime

with DAG(
    dag_id="failing_dag",
    start_date=datetime(2026, 1, 1),
    schedule=None,
    catchup=False,
) as dag:
    BashOperator(task_id="will_fail", bash_command="exit 1")
DAGEOF
'

# 4. Unpause the DAG
echo "  Unpausing DAG..."
curl -sf -u "$AIRFLOW_USER:$AIRFLOW_PASS" -X PATCH "$AIRFLOW_URL/api/v1/dags/failing_dag" \
  -H 'Content-Type: application/json' \
  -d '{"is_paused":false}' > /dev/null

# 5. Trigger a DAG run
echo "  Triggering DAG run..."
curl -sf -u "$AIRFLOW_USER:$AIRFLOW_PASS" -X POST "$AIRFLOW_URL/api/v1/dags/failing_dag/dagRuns" \
  -H 'Content-Type: application/json' \
  -d '{}' > /dev/null

# 6. Wait for the DAG to run and fail (SequentialExecutor processes one task at a time)
echo "  Waiting for DAG run to fail..."
for i in $(seq 1 12); do
  sleep 5
  FAILED=$(curl -sf -u "$AIRFLOW_USER:$AIRFLOW_PASS" "$AIRFLOW_URL/api/v1/dags/~/dagRuns?state=failed" | python3 -c "import sys,json; print(json.load(sys.stdin).get('total_entries',0))" 2>/dev/null || echo "0")
  if [ "$FAILED" -gt 0 ]; then
    echo "  DAG run failed ($FAILED failed run(s))"
    break
  fi
done

if [ "${FAILED:-0}" -eq 0 ]; then
  # Fallback: trigger via CLI inside container
  echo "  Falling back to CLI trigger..."
  podman exec "$CONTAINER" bash -c 'airflow dags unpause failing_dag 2>&1; airflow dags trigger failing_dag 2>&1' | tail -3
  sleep 30
  FAILED=$(curl -sf -u "$AIRFLOW_USER:$AIRFLOW_PASS" "$AIRFLOW_URL/api/v1/dags/~/dagRuns?state=failed" | python3 -c "import sys,json; print(json.load(sys.stdin).get('total_entries',0))" 2>/dev/null || echo "0")
fi

echo "=== Airflow setup complete ==="
echo "DAG: failing_dag ($FAILED failed run(s))"
echo "Verify: curl -u admin:admin $AIRFLOW_URL/api/v1/dags/~/dagRuns?state=failed"
