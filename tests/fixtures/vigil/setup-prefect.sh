#!/usr/bin/env bash
# setup-prefect.sh — Create a failed Prefect flow run for vigil e2e testing.
#
# Usage: ./setup-prefect.sh
#   Requires: Prefect running on localhost:4200 (podman-compose up -d)
#   Idempotent: creates a new flow run each invocation

set -euo pipefail

PREFECT_URL="http://localhost:4200"
FLOW_NAME="failing-flow"

echo "=== Prefect e2e setup ==="

# 1. Create a flow (idempotent — existing flow is reused)
echo "  Creating flow..."
FLOW_ID=$(curl -sf -X POST -H 'Content-Type: application/json' "$PREFECT_URL/api/flows/" \
  -d "{\"name\":\"$FLOW_NAME\"}" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "  Flow ID: $FLOW_ID"

# 2. Create a flow run in SCHEDULED state
echo "  Creating flow run..."
RUN_JSON=$(curl -sf -X POST "$PREFECT_URL/api/flow_runs/" \
  -H 'Content-Type: application/json' \
  -d "{
    \"flow_id\": \"$FLOW_ID\",
    \"name\": \"failing-run-$(date +%s)\",
    \"state\": {\"type\": \"SCHEDULED\"}
  }")
RUN_ID=$(echo "$RUN_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "  Run ID: $RUN_ID"

# 3. Transition to FAILED (set_state returns {state: {type: "FAILED"}, status: "ACCEPT"})
echo "  Setting state to FAILED..."
RESPONSE=$(curl -sf -X POST "$PREFECT_URL/api/flow_runs/$RUN_ID/set_state" \
  -H 'Content-Type: application/json' \
  -d '{"state": {"type": "FAILED", "name": "Failed", "message": "simulated failure for vigil e2e test"}}')
STATUS=$(echo "$RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','REJECTED'))" 2>/dev/null || echo "REJECTED")

if [ "$STATUS" != "ACCEPT" ]; then
  echo "  ERROR: state transition rejected (status=$STATUS)" >&2
  echo "  Response: $RESPONSE" >&2
  exit 1
fi
echo "  State transition accepted"

# 4. Verify the flow run appears in the FAILED filter
echo "  Verifying failed flow runs..."
FAILED_COUNT=$(curl -sf -X POST -H 'Content-Type: application/json' \
  -d '{"flow_runs":{"state":{"type":{"any_":["FAILED","CRASHED"]}}}}' \
  "$PREFECT_URL/api/flow_runs/filter" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))")

echo "=== Prefect setup complete ==="
echo "Flow: $FLOW_NAME ($FLOW_ID)"
echo "Run:  $RUN_ID (FAILED)"
echo "$FAILED_COUNT failed flow run(s) detected by filter"
