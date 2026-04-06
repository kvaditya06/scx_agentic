#!/bin/bash
# Agentic Kernel Test Runner
# Usage: ./test_milestone.sh [1|2|3|4|5|all]
set -e

BINARY="./target/debug/scx_agentic"
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

pass() { echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { echo -e "  ${RED}FAIL${NC}: $1"; }
warn() { echo -e "  ${YELLOW}WARN${NC}: $1"; }
info() { echo -e "  ${BOLD}INFO${NC}: $1"; }

check_prereqs() {
    if [ ! -f "$BINARY" ]; then
        echo "Binary not found. Building..."
        cargo build -p scx_agentic
    fi

    if [ "$(cat /sys/kernel/sched_ext/state 2>/dev/null)" = "enabled" ]; then
        echo "Another sched_ext scheduler is already loaded. Abort."
        exit 1
    fi

    if [ "$(whoami)" != "root" ]; then
        echo "Must be run as root."
        exit 1
    fi
}

cleanup() {
    # Kill any leftover scheduler or stress processes
    pkill -f "scx_agentic" 2>/dev/null || true
    pkill -f "stress-ng" 2>/dev/null || true
    sleep 1
}

milestone_1() {
    echo -e "\n${BOLD}=== MILESTONE 1: Does It Even Load? ===${NC}\n"
    local LOG="/tmp/m1_load.log"

    timeout 10 $BINARY --verbose 2>&1 | tee $LOG || true
    echo ""

    if grep -qi "market scheduler" $LOG; then
        pass "Scheduler loaded into kernel"
    else
        fail "Scheduler failed to load"
        return 1
    fi

    if grep -qi "unregister\|exit" $LOG; then
        pass "Clean exit"
    else
        warn "Exit status unclear (may have been killed by timeout — that's OK)"
    fi

    info "Log: $LOG"
}

milestone_2() {
    echo -e "\n${BOLD}=== MILESTONE 2: Can It Schedule Work? ===${NC}\n"
    local LOG="/tmp/m2_sched.log"

    $BINARY --verbose 2>$LOG &
    local SCHED_PID=$!
    sleep 2

    if ! kill -0 $SCHED_PID 2>/dev/null; then
        fail "Scheduler died during startup"
        return 1
    fi
    pass "Scheduler running (PID $SCHED_PID)"

    # CPU work
    info "Running stress-ng --cpu 2 for 15s..."
    stress-ng --cpu 2 --timeout 15s --metrics-brief 2>&1 | tee /tmp/m2_stress.log &
    local STRESS_PID=$!

    # I/O work
    dd if=/dev/urandom of=/dev/null bs=1M count=50 2>/tmp/m2_dd.log &

    # Check latency mid-run
    sleep 5
    local T1=$(date +%s%N)
    ls /tmp > /dev/null
    local T2=$(date +%s%N)
    local LAT=$(( (T2 - T1) / 1000000 ))

    wait $STRESS_PID 2>/dev/null
    wait 2>/dev/null

    kill $SCHED_PID 2>/dev/null
    wait $SCHED_PID 2>/dev/null

    echo ""
    if [ -s /tmp/m2_stress.log ]; then
        pass "stress-ng completed"
    else
        fail "stress-ng produced no output"
    fi

    if [ "$LAT" -lt 100 ]; then
        pass "Interactive latency: ${LAT}ms"
    elif [ "$LAT" -lt 1000 ]; then
        warn "Interactive latency: ${LAT}ms (acceptable under load)"
    else
        fail "Interactive latency: ${LAT}ms (too slow)"
    fi

    info "Log: $LOG"
}

milestone_3() {
    echo -e "\n${BOLD}=== MILESTONE 3: Is The Market Working? ===${NC}\n"
    local LOG="/tmp/m3_market.log"

    $BINARY --verbose 2>$LOG &
    local SCHED_PID=$!
    sleep 2

    # Diverse workload
    for i in $(seq 1 15); do
        (echo "scale=100; 4*a(1)" | bc -l > /dev/null 2>&1) &
    done
    stress-ng --cpu 2 --timeout 15s 2>/dev/null &
    dd if=/dev/zero of=/dev/null bs=4k count=100000 2>/dev/null &

    sleep 18
    kill $SCHED_PID 2>/dev/null
    wait 2>/dev/null
    sleep 1

    echo ""
    # Token supply
    if grep -q "TOKEN SUPPLY VIOLATION" $LOG; then
        fail "Token supply violation detected"
    else
        pass "Token supply conserved"
    fi

    # Circuit breaker
    local TRIPS=$(grep -c "Circuit breaker TRIPPED" $LOG 2>/dev/null || echo "0")
    local RELS=$(grep -c "Circuit breaker released" $LOG 2>/dev/null || echo "0")
    if [ "$TRIPS" = "0" ]; then
        pass "Circuit breaker: no trips needed (stable market)"
    elif [ "$TRIPS" = "$RELS" ]; then
        pass "Circuit breaker: $TRIPS trip(s), all released"
    else
        warn "Circuit breaker: $TRIPS trips, $RELS releases"
    fi

    # Central Bank
    if grep -q "Central Bank daemon started" $LOG; then
        pass "Central Bank running"
    else
        fail "Central Bank not started"
    fi

    info "Log: $LOG"
}

milestone_4() {
    echo -e "\n${BOLD}=== MILESTONE 4: Stress Test (Break It) ===${NC}\n"
    local LOG="/tmp/m4_stress.log"

    $BINARY --verbose 2>$LOG &
    local SCHED_PID=$!
    sleep 2

    info "Phase A: Normal load..."
    stress-ng --cpu 1 --timeout 5s 2>/dev/null
    sleep 1

    info "Phase B: Overload (50 workers)..."
    for i in $(seq 1 50); do
        stress-ng --cpu 1 --timeout 10s 2>/dev/null &
    done

    sleep 3
    local T1=$(date +%s%N)
    ls /tmp > /dev/null
    local T2=$(date +%s%N)
    local LAT=$(( (T2 - T1) / 1000000 ))

    sleep 12
    wait 2>/dev/null

    info "Phase C: Recovery..."
    stress-ng --cpu 1 --timeout 5s --metrics-brief 2>&1 > /tmp/m4_recovery.log || true
    sleep 1

    kill $SCHED_PID 2>/dev/null
    wait $SCHED_PID 2>/dev/null
    sleep 1

    echo ""
    local TRIPS=$(grep -c "Circuit breaker TRIPPED" $LOG 2>/dev/null || echo "0")
    if [ "$TRIPS" -gt "0" ]; then
        pass "Circuit breaker activated under stress ($TRIPS trips)"
    else
        info "Circuit breaker didn't trip (threshold may be high for this load)"
    fi

    if grep -q "TOKEN SUPPLY VIOLATION" $LOG; then
        fail "Token supply violation during stress"
    else
        pass "Token supply survived stress test"
    fi

    if [ -s /tmp/m4_recovery.log ]; then
        pass "System recovered post-storm"
    else
        fail "Recovery workload failed"
    fi

    if [ "$LAT" -lt 1000 ]; then
        pass "Responsive during overload (${LAT}ms)"
    else
        warn "High latency during overload (${LAT}ms)"
    fi

    info "Log: $LOG"
}

milestone_5() {
    echo -e "\n${BOLD}=== MILESTONE 5: Sustained 60s Run ===${NC}\n"
    local LOG="/tmp/m5_sustained.log"

    $BINARY --verbose 2>$LOG &
    local SCHED_PID=$!
    sleep 2

    # Mixed sustained workload
    stress-ng --cpu 1 --timeout 55s 2>/dev/null &

    (for i in $(seq 1 10); do
        dd if=/dev/urandom of=/dev/null bs=1M count=10 2>/dev/null
        sleep 5
    done) &

    (for i in $(seq 1 100); do
        echo "scale=10; 4*a(1)" | bc -l > /dev/null 2>&1
        sleep 0.5
    done) &

    # Latency checks
    local MAX_LAT=0
    for i in $(seq 1 6); do
        sleep 10
        local T1=$(date +%s%N)
        ls /tmp > /dev/null
        local T2=$(date +%s%N)
        local LAT=$(( (T2 - T1) / 1000000 ))
        echo "  [${i}0s] Latency: ${LAT}ms"
        if [ "$LAT" -gt "$MAX_LAT" ]; then MAX_LAT=$LAT; fi
    done

    kill $SCHED_PID 2>/dev/null
    wait 2>/dev/null
    sleep 1

    echo ""
    local LINES=$(wc -l < $LOG)
    info "Log lines: $LINES"

    if grep -q "TOKEN SUPPLY VIOLATION" $LOG; then
        local V=$(grep -c "TOKEN SUPPLY VIOLATION" $LOG)
        fail "Token supply: $V violation(s)"
    else
        pass "Token supply: conserved for 60s"
    fi

    local TRIPS=$(grep -c "Circuit breaker TRIPPED" $LOG 2>/dev/null || echo "0")
    if [ "$TRIPS" -lt "5" ]; then
        pass "Market stability: $TRIPS circuit breaker trip(s)"
    else
        warn "Market stability: $TRIPS trips (market may be oversensitive)"
    fi

    if grep -q "Central Bank daemon started" $LOG; then
        pass "Central Bank: ran throughout"
    else
        fail "Central Bank: missing"
    fi

    if [ "$MAX_LAT" -lt "100" ]; then
        pass "Worst latency: ${MAX_LAT}ms"
    elif [ "$MAX_LAT" -lt "500" ]; then
        warn "Worst latency: ${MAX_LAT}ms"
    else
        fail "Worst latency: ${MAX_LAT}ms"
    fi

    info "Full log: $LOG"
}

# --- Main ---

check_prereqs

case "${1:-all}" in
    1) milestone_1 ;;
    2) cleanup; milestone_2 ;;
    3) cleanup; milestone_3 ;;
    4) cleanup; milestone_4 ;;
    5) cleanup; milestone_5 ;;
    all)
        milestone_1
        cleanup
        milestone_2
        cleanup
        milestone_3
        cleanup
        milestone_4
        cleanup
        milestone_5
        cleanup
        echo ""
        echo -e "${BOLD}=== ALL MILESTONES COMPLETE ===${NC}"
        echo "Logs in /tmp/m[1-5]_*.log"
        ;;
    *)
        echo "Usage: $0 [1|2|3|4|5|all]"
        exit 1
        ;;
esac
