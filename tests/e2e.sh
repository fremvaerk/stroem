#!/usr/bin/env bash
#
# Strøm Phase 1 E2E test
#
# Usage: ./tests/e2e.sh
#
# Starts docker-compose, triggers a workflow, verifies execution, and tears down.
# Requires: docker compose, curl, jq
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BASE_URL="http://localhost:8080"
# Detect docker compose command (v2 plugin vs standalone)
if docker compose version &>/dev/null; then
    COMPOSE="docker compose -f $PROJECT_DIR/docker-compose.yml"
else
    COMPOSE="docker-compose -f $PROJECT_DIR/docker-compose.yml"
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1"; exit 1; }
info() { echo -e "${YELLOW}----${NC}: $1"; }

cleanup() {
    info "Tearing down containers..."
    $COMPOSE down -v --remove-orphans 2>/dev/null || true
}

# Always clean up on exit
trap cleanup EXIT

# --- 0. Pre-flight checks ---
for cmd in docker curl jq; do
    command -v "$cmd" >/dev/null 2>&1 || fail "Required command '$cmd' not found"
done

# --- 1. Build and start services ---
info "Building images (this may take a few minutes on first run)..."
$COMPOSE build --quiet

info "Starting services..."
$COMPOSE up -d

info "Waiting for server to become healthy..."
MAX_WAIT=120
WAITED=0
until curl -sf "$BASE_URL/api/workspaces/default/tasks" >/dev/null 2>&1; do
    sleep 2
    WAITED=$((WAITED + 2))
    if [ "$WAITED" -ge "$MAX_WAIT" ]; then
        echo ""
        info "Server logs:"
        $COMPOSE logs server | tail -30
        fail "Server did not become healthy within ${MAX_WAIT}s"
    fi
    printf "."
done
echo ""
pass "Server is healthy (${WAITED}s)"

# Wait a moment for worker to register
sleep 3

# --- 2. List tasks ---
info "Listing tasks..."
TASKS=$(curl -sf "$BASE_URL/api/workspaces/default/tasks")
TASK_COUNT=$(echo "$TASKS" | jq 'length')

if [ "$TASK_COUNT" -lt 1 ]; then
    fail "Expected at least 1 task, got $TASK_COUNT"
fi
pass "Found $TASK_COUNT tasks"

# Check hello-world task exists
HAS_HELLO=$(echo "$TASKS" | jq '[.[] | select(.name == "hello-world")] | length')
if [ "$HAS_HELLO" -ne 1 ]; then
    fail "hello-world task not found"
fi
pass "hello-world task is registered"

# --- 3. Execute hello-world task ---
info "Triggering hello-world task..."
EXEC_RESP=$(curl -sf -X POST "$BASE_URL/api/workspaces/default/tasks/hello-world/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {"name": "E2E Test"}}')

JOB_ID=$(echo "$EXEC_RESP" | jq -r '.job_id')
if [ -z "$JOB_ID" ] || [ "$JOB_ID" = "null" ]; then
    fail "No job_id returned: $EXEC_RESP"
fi
pass "Job created: $JOB_ID"

# --- 4. Poll until job completes ---
info "Waiting for job to complete..."
MAX_POLL=60
POLLED=0
JOB_STATUS="pending"

while [ "$JOB_STATUS" != "completed" ] && [ "$JOB_STATUS" != "failed" ]; do
    sleep 2
    POLLED=$((POLLED + 2))
    if [ "$POLLED" -ge "$MAX_POLL" ]; then
        JOB_DETAIL=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID")
        echo "$JOB_DETAIL" | jq .
        fail "Job did not complete within ${MAX_POLL}s (status: $JOB_STATUS)"
    fi

    JOB_DETAIL=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID")
    JOB_STATUS=$(echo "$JOB_DETAIL" | jq -r '.status')
    printf "."
done
echo ""

if [ "$JOB_STATUS" != "completed" ]; then
    echo "$JOB_DETAIL" | jq .
    fail "Job failed (status: $JOB_STATUS)"
fi
pass "Job completed successfully (${POLLED}s)"

# --- 5. Verify steps ---
info "Verifying step statuses..."
STEPS=$(echo "$JOB_DETAIL" | jq '.steps')
STEP_COUNT=$(echo "$STEPS" | jq 'length')

if [ "$STEP_COUNT" -ne 2 ]; then
    fail "Expected 2 steps, got $STEP_COUNT"
fi
pass "Job has 2 steps"

# Check all steps completed
COMPLETED_STEPS=$(echo "$STEPS" | jq '[.[] | select(.status == "completed")] | length')
if [ "$COMPLETED_STEPS" -ne 2 ]; then
    echo "$STEPS" | jq .
    fail "Expected 2 completed steps, got $COMPLETED_STEPS"
fi
pass "All steps completed"

# --- 6. Verify logs ---
info "Checking hello-world job logs..."
LOGS_RESP=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID/logs")
LOGS=$(echo "$LOGS_RESP" | jq -r '.logs')

if [ -z "$LOGS" ] || [ "$LOGS" = "null" ]; then
    echo "Logs response:"
    echo "$LOGS_RESP" | jq .
    fail "No logs returned for hello-world job"
fi

# Verify step 1 output (greet: "echo Hello E2E Test")
if echo "$LOGS" | grep -q "Hello E2E Test"; then
    pass "Logs contain greet step output ('Hello E2E Test')"
else
    echo "Logs:"
    echo "$LOGS"
    fail "Logs missing greet step output ('Hello E2E Test')"
fi

# Verify step 2 output (shout: uppercased greeting from step 1 output)
# The greet step emits OUTPUT: {"greeting": "Hello E2E Test"}
# The shout step receives {{ say-hello.output.greeting }} = "Hello E2E Test"
# and uppercases it to "HELLO E2E TEST"
# This proves: job input -> step 1 -> OUTPUT parsing -> step 2 input templating
if echo "$LOGS" | grep -q "HELLO E2E TEST"; then
    pass "Logs contain shout step output ('HELLO E2E TEST') - proves step-to-step data flow"
else
    echo "Logs:"
    echo "$LOGS"
    fail "Logs missing shout step output ('HELLO E2E TEST') - step-to-step data flow broken"
fi

# --- 7. Test list jobs endpoint ---
info "Testing jobs list endpoint..."
JOBS_RESP=$(curl -sf "$BASE_URL/api/jobs?limit=10")
JOBS_COUNT=$(echo "$JOBS_RESP" | jq 'length')

if [ "$JOBS_COUNT" -lt 1 ]; then
    fail "Expected at least 1 job in list, got $JOBS_COUNT"
fi
pass "Jobs list returned $JOBS_COUNT job(s)"

# --- 8. Run deploy-pipeline task ---
info "Triggering deploy-pipeline task..."
EXEC_RESP2=$(curl -sf -X POST "$BASE_URL/api/workspaces/default/tasks/deploy-pipeline/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {"env": "test"}}')

JOB_ID2=$(echo "$EXEC_RESP2" | jq -r '.job_id')
if [ -z "$JOB_ID2" ] || [ "$JOB_ID2" = "null" ]; then
    fail "No job_id returned for deploy-pipeline: $EXEC_RESP2"
fi
pass "Deploy pipeline job created: $JOB_ID2"

info "Waiting for deploy-pipeline to complete..."
POLLED=0
JOB_STATUS2="pending"

while [ "$JOB_STATUS2" != "completed" ] && [ "$JOB_STATUS2" != "failed" ]; do
    sleep 2
    POLLED=$((POLLED + 2))
    if [ "$POLLED" -ge "$MAX_POLL" ]; then
        JOB_DETAIL2=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID2")
        echo "$JOB_DETAIL2" | jq .
        fail "Deploy pipeline did not complete within ${MAX_POLL}s"
    fi

    JOB_DETAIL2=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID2")
    JOB_STATUS2=$(echo "$JOB_DETAIL2" | jq -r '.status')
    printf "."
done
echo ""

if [ "$JOB_STATUS2" != "completed" ]; then
    echo "$JOB_DETAIL2" | jq .
    fail "Deploy pipeline failed (status: $JOB_STATUS2)"
fi
pass "Deploy pipeline completed (${POLLED}s)"

# --- 9. Verify deploy-pipeline logs ---
info "Checking deploy-pipeline job logs..."
LOGS_RESP2=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID2/logs")
LOGS2=$(echo "$LOGS_RESP2" | jq -r '.logs')

if [ -z "$LOGS2" ] || [ "$LOGS2" = "null" ]; then
    echo "Logs response:"
    echo "$LOGS_RESP2" | jq .
    fail "No logs returned for deploy-pipeline job"
fi

# Verify health check step ran
if echo "$LOGS2" | grep -q "Checking system status"; then
    pass "Deploy logs contain health-check output"
else
    echo "Logs:"
    echo "$LOGS2"
    fail "Deploy logs missing health-check output"
fi

# Verify deploy step ran (from actions/deploy.sh)
if echo "$LOGS2" | grep -q "Deployment complete"; then
    pass "Deploy logs contain deployment output"
else
    echo "Logs:"
    echo "$LOGS2"
    fail "Deploy logs missing deployment output"
fi

# Verify notify step ran AND received the correct input value
# The notify cmd is: "echo 'Notification: Deployment to {{ input.env }} completed...'"
# We passed env="test", so it should contain "Deployment to test"
if echo "$LOGS2" | grep -q "Deployment to test completed"; then
    pass "Deploy logs contain notification with correct env input ('test')"
else
    echo "Logs:"
    echo "$LOGS2"
    fail "Deploy logs missing notification with env input - input propagation broken"
fi

# --- 10. Test hooks with Python crash ---
info "Triggering python-crash-with-hook task (expects failure + hook)..."
EXEC_RESP3=$(curl -sf -X POST "$BASE_URL/api/workspaces/default/tasks/python-crash-with-hook/execute" \
    -H "Content-Type: application/json" \
    -d '{"input": {}}')

JOB_ID3=$(echo "$EXEC_RESP3" | jq -r '.job_id')
if [ -z "$JOB_ID3" ] || [ "$JOB_ID3" = "null" ]; then
    fail "No job_id returned for python-crash-with-hook: $EXEC_RESP3"
fi
pass "Python crash job created: $JOB_ID3"

info "Waiting for python-crash job to fail..."
POLLED=0
JOB_STATUS3="pending"

while [ "$JOB_STATUS3" != "completed" ] && [ "$JOB_STATUS3" != "failed" ]; do
    sleep 2
    POLLED=$((POLLED + 2))
    if [ "$POLLED" -ge "$MAX_POLL" ]; then
        JOB_DETAIL3=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID3")
        echo "$JOB_DETAIL3" | jq .
        fail "Python crash job did not reach terminal state within ${MAX_POLL}s (status: $JOB_STATUS3)"
    fi

    JOB_DETAIL3=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID3")
    JOB_STATUS3=$(echo "$JOB_DETAIL3" | jq -r '.status')
    printf "."
done
echo ""

if [ "$JOB_STATUS3" != "failed" ]; then
    echo "$JOB_DETAIL3" | jq .
    fail "Expected python-crash job to fail, got status: $JOB_STATUS3"
fi
pass "Python crash job failed as expected (${POLLED}s)"

# --- 11. Verify Python traceback in error message ---
info "Checking crash step error message..."
STEP_ERROR=$(echo "$JOB_DETAIL3" | jq -r '.steps[] | select(.step_name == "crash") | .error_message')

if echo "$STEP_ERROR" | grep -q "KeyError"; then
    pass "Error message contains Python KeyError exception"
else
    echo "Step error:"
    echo "$STEP_ERROR"
    fail "Error message missing Python KeyError exception"
fi

if echo "$STEP_ERROR" | grep -q "missing_key"; then
    pass "Error message contains the missing key name"
else
    echo "Step error:"
    echo "$STEP_ERROR"
    fail "Error message missing the key name 'missing_key'"
fi

# --- 12. Verify Python traceback in logs ---
info "Checking crash job logs for traceback..."
LOGS_RESP3=$(curl -sf "$BASE_URL/api/jobs/$JOB_ID3/logs")
LOGS3=$(echo "$LOGS_RESP3" | jq -r '.logs')

if echo "$LOGS3" | grep -q "Traceback"; then
    pass "Logs contain Python Traceback header"
else
    echo "Logs:"
    echo "$LOGS3"
    info "Note: traceback may be in error_message only (stderr captured by worker)"
fi

# --- 13. Verify hook job was created and completed ---
info "Waiting for hook job to appear and complete..."
POLLED=0
HOOK_FOUND=false

while [ "$POLLED" -lt "$MAX_POLL" ]; do
    sleep 2
    POLLED=$((POLLED + 2))

    ALL_JOBS=$(curl -sf "$BASE_URL/api/jobs?limit=50")
    HOOK_JOB=$(echo "$ALL_JOBS" | jq -r '[.[] | select(.source_type == "hook")] | first')

    if [ "$HOOK_JOB" != "null" ] && [ -n "$HOOK_JOB" ]; then
        HOOK_JOB_ID=$(echo "$HOOK_JOB" | jq -r '.job_id')
        HOOK_STATUS=$(echo "$HOOK_JOB" | jq -r '.status')

        if [ "$HOOK_STATUS" = "completed" ] || [ "$HOOK_STATUS" = "failed" ]; then
            HOOK_FOUND=true
            break
        fi
    fi
    printf "."
done
echo ""

if [ "$HOOK_FOUND" != "true" ]; then
    info "All jobs:"
    curl -sf "$BASE_URL/api/jobs?limit=50" | jq .
    fail "Hook job not found or did not reach terminal state within ${MAX_POLL}s"
fi
pass "Hook job $HOOK_JOB_ID reached status: $HOOK_STATUS"

# Verify hook job has correct source_type and source_id
HOOK_SOURCE_ID=$(echo "$HOOK_JOB" | jq -r '.source_id')
if echo "$HOOK_SOURCE_ID" | grep -q "on_error"; then
    pass "Hook job source_id contains 'on_error'"
else
    fail "Hook job source_id should contain 'on_error', got: $HOOK_SOURCE_ID"
fi

# --- 14. Verify hook job logs contain the error context ---
info "Checking hook job logs for error context..."
HOOK_LOGS_RESP=$(curl -sf "$BASE_URL/api/jobs/$HOOK_JOB_ID/logs")
HOOK_LOGS=$(echo "$HOOK_LOGS_RESP" | jq -r '.logs')

if echo "$HOOK_LOGS" | grep -q "HOOK_FIRED"; then
    pass "Hook job logs contain HOOK_FIRED marker"
else
    echo "Hook logs:"
    echo "$HOOK_LOGS"
    fail "Hook job logs missing HOOK_FIRED marker"
fi

if echo "$HOOK_LOGS" | grep -q "status=failed"; then
    pass "Hook job logs contain status=failed"
else
    echo "Hook logs:"
    echo "$HOOK_LOGS"
    fail "Hook job logs missing status=failed"
fi

if echo "$HOOK_LOGS" | grep -q "KeyError"; then
    pass "Hook job error_message contains Python KeyError (full traceback passed to hook)"
else
    echo "Hook logs:"
    echo "$HOOK_LOGS"
    fail "Hook job error_message missing Python KeyError - traceback not propagated to hook"
fi

# --- Summary ---
echo ""
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}  All E2E tests passed!${NC}"
echo -e "${GREEN}========================================${NC}"
