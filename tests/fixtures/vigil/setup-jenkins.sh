#!/usr/bin/env bash
# setup-jenkins.sh — Create a failing Jenkins job and trigger a build for e2e vigil testing.
#
# Usage: ./setup-jenkins.sh
#   Requires: Jenkins running on localhost:8080 (podman-compose up -d)
#   Idempotent: deletes and recreates 'test-pipeline' each run

set -euo pipefail

JENKINS_URL="http://localhost:8080"
JOB_NAME="test-pipeline"

echo "=== Jenkins e2e setup ==="

# Fetch CSRF crumb with session cookie (Jenkins requires both)
echo "  Fetching CSRF crumb..."
COOKIE_JAR=$(mktemp)
CRUMB_JSON=$(curl -sf -c "$COOKIE_JAR" "$JENKINS_URL/crumbIssuer/api/json")
CRUMB=$(echo "$CRUMB_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin)['crumb'])")
echo "  Crumb: $CRUMB"

# Delete existing job if present (ignore errors)
echo "  Cleaning up old job..."
curl -sf -X POST "$JENKINS_URL/job/$JOB_NAME/doDelete" \
  -b "$COOKIE_JAR" \
  -H "Jenkins-Crumb: $CRUMB" > /dev/null 2>&1 || true

# Create a freestyle job that runs 'exit 1'
echo "  Creating $JOB_NAME..."
curl -sf -X POST "$JENKINS_URL/createItem?name=$JOB_NAME" \
  -b "$COOKIE_JAR" \
  -H "Jenkins-Crumb: $CRUMB" \
  -H 'Content-Type: application/xml' \
  --data-binary "<project><builders><hudson.tasks.Shell><command>echo 'build started'; exit 1</command></hudson.tasks.Shell></builders></project>" > /dev/null

# Trigger a build
echo "  Triggering build..."
curl -sf -X POST "$JENKINS_URL/job/$JOB_NAME/build" \
  -b "$COOKIE_JAR" \
  -H "Jenkins-Crumb: $CRUMB" > /dev/null

# Wait for build to complete
sleep 8

# Verify the build failed
RESULT=$(curl -sf "$JENKINS_URL/job/$JOB_NAME/lastBuild/api/json" | python3 -c "import sys,json; print(json.load(sys.stdin).get('result','UNKNOWN'))")
echo "  Build result: $RESULT"

if [ "$RESULT" != "FAILURE" ]; then
  echo "ERROR: expected FAILURE, got $RESULT" >&2
  exit 1
fi

rm -f "$COOKIE_JAR"
echo "=== Jenkins setup complete ==="
echo "Job: $JENKINS_URL/job/$JOB_NAME/"
echo "Last build: #1 (FAILURE)"
