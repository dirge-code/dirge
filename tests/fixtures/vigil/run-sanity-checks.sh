#!/usr/bin/env bash
# run-sanity-checks.sh — Verify all engine containers, failure fixtures, and
# harbinger ports are ready for vigil e2e testing.
#
# Usage: ./run-sanity-checks.sh
#   Requires: podman-compose up -d (engines running) and setup-*.sh run
#   Exit 0 = everything ready. Exit 1 = something is wrong.

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

PASS="${GREEN}PASS${NC}"
FAIL="${RED}FAIL${NC}"
WARN="${YELLOW}WARN${NC}"

errors=0
warnings=0

check() {
  local label="$1"; shift
  local result="$1"; shift
  if [ "$result" = "0" ]; then
    printf "  ${PASS}  %s\n" "$label"
  elif [ "$result" = "WARN" ]; then
    printf "  ${WARN}  %s\n" "$label"
    warnings=$((warnings + 1))
  else
    printf "  ${FAIL}  %s\n" "$label"
    errors=$((errors + 1))
  fi
}

echo "=== Vigil Sanity Check ==="
echo ""

# ── Containers ──────────────────────────────────────────────────────────────

echo "--- Containers ---"

for svc in vigil-jenkins vigil-prefect vigil-airflow; do
  if podman ps --filter "name=$svc" --format '{{.Status}}' 2>/dev/null | grep -q '^Up'; then
    check "$svc" 0
  else
    check "$svc" 1
  fi
done
echo ""

# ── Engine APIs ─────────────────────────────────────────────────────────────

echo "--- Engine APIs ---"

# Jenkins
if curl -sf --max-time 5 'http://localhost:8080/api/json' > /dev/null 2>&1; then
  check "Jenkins API (8080)" 0
else
  check "Jenkins API (8080)" 1
fi

# Prefect
if curl -sf --max-time 5 'http://localhost:4200/api/health' > /dev/null 2>&1; then
  check "Prefect API (4200)" 0
else
  check "Prefect API (4200)" 1
fi

# Airflow
if curl -sf --max-time 5 -u admin:admin 'http://localhost:8081/api/v1/dags' > /dev/null 2>&1; then
  check "Airflow API (8081)" 0
else
  check "Airflow API (8081)" 1
fi
echo ""

# ── Failure fixtures ────────────────────────────────────────────────────────

echo "--- Failure Fixtures ---"

# Jenkins: check test-pipeline job exists and last build failed
JENKINS_JOB=$(curl -sf --globoff --max-time 5 \
  'http://localhost:8080/api/json?tree=jobs[name,color]' \
  2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); jobs=[j for j in d.get('jobs',[]) if j['name']=='test-pipeline']; print(jobs[0]['color'] if jobs else 'NOT_FOUND')" 2>/dev/null || echo "ERROR")

case "$JENKINS_JOB" in
  red*) check "Jenkins: test-pipeline FAILURE" 0 ;;
  NOT_FOUND) check "Jenkins: test-pipeline job not found" 1 ;;
  *) check "Jenkins: test-pipeline (color=$JENKINS_JOB)" 1 ;;
esac

# Prefect: check for failed flow runs
PREFECT_FAILED=$(curl -sf --max-time 5 \
  -X POST -H 'Content-Type: application/json' \
  -d '{"flow_runs":{"state":{"type":{"any_":["FAILED","CRASHED"]}}}}' \
  'http://localhost:4200/api/flow_runs/filter' \
  2>/dev/null | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")

if [ "$PREFECT_FAILED" -gt 0 ]; then
  check "Prefect: $PREFECT_FAILED failed flow run(s)" 0
else
  check "Prefect: no failed flow runs" 1
fi

# Airflow: check for failed DAG runs
AIRFLOW_FAILED=$(curl -sf --max-time 5 \
  -u admin:admin \
  'http://localhost:8081/api/v1/dags/~/dagRuns?state=failed' \
  2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('total_entries',0))" 2>/dev/null || echo "0")

if [ "$AIRFLOW_FAILED" -gt 0 ]; then
  check "Airflow: $AIRFLOW_FAILED failed DAG run(s)" 0
else
  check "Airflow: no failed DAG runs" 1
fi
echo ""

# ── Harbinger ports ─────────────────────────────────────────────────────────

echo "--- Harbinger Ports ---"

if nc -z -w2 127.0.0.1 9090 2>/dev/null; then
  check "Harbinger template (9090)" 0
else
  check "Harbinger template (9090)" 1
fi

if nc -z -w2 127.0.0.1 9091 2>/dev/null; then
  check "Harbinger commands (9091)" 0
else
  check "Harbinger commands (9091)" 1
fi

# Probe template mode
if nc -z -w2 127.0.0.1 9090 2>/dev/null; then
  echo '{"message":"sanity-check-probe"}' | nc -w2 127.0.0.1 9090 > /dev/null 2>&1 && true
  check "  template probe sent" 0
else
  check "  template probe (skipped, port down)" "WARN"
fi

# Probe commands mode
if nc -z -w2 127.0.0.1 9091 2>/dev/null; then
  echo '{"command":"ping"}' | nc -w2 127.0.0.1 9091 > /dev/null 2>&1 && true
  check "  commands ping sent" 0
else
  check "  commands ping (skipped, port down)" "WARN"
fi
echo ""

# ── Cross-session messaging ─────────────────────────────────────────────────

echo "--- Cross-Session Messaging ---"

# Send a realistic cross-session event to the template harbinger port.
# If a dirge --vigil instance is running with harbinger on 9090, it will
# receive this payload and fire an observance turn.
CROSS_MSG_DATA='{"sender":"sanity-checker","event":"coord-test","message":"Cross-session messaging test from run-sanity-checks.sh. If you see this, sessions can message each other via harbinger."}'
CROSS_OK=0
echo "$CROSS_MSG_DATA" | nc -w2 127.0.0.1 9090 > /dev/null 2>&1 || CROSS_OK=1

if [ "$CROSS_OK" -eq 0 ]; then
  check "Cross-session event sent to 9090" 0
else
  check "Cross-session event (send failed, is dirge --vigil running on 9090?)" "WARN"
fi
echo ""

# ── Summary ─────────────────────────────────────────────────────────────────

echo "=== Summary ==="

if [ "$errors" -gt 0 ]; then
  echo "  Errors:   $errors"
else
  echo "  Errors:   0"
fi
echo "  Warnings: $warnings"
echo ""

if [ "$errors" -gt 0 ]; then
  echo "Some checks failed. Run the setup scripts:"
  echo "  ./tests/fixtures/vigil/setup-jenkins.sh"
  echo "  ./tests/fixtures/vigil/setup-prefect.sh"
  echo "  ./tests/fixtures/vigil/setup-airflow.sh"
  echo ""
  exit 1
fi

echo "All checks passed. Engines are ready for vigil e2e testing."
echo ""
echo "Next: start dirge --vigil and run:"
echo "  /plugins load all"
echo "  /poll-jenkins"
echo "  /poll-prefect"
echo "  /poll-airflow"
echo "  /vigil status"
