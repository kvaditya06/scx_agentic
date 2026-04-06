# Agentic Kernel — Testing Playbook

> **Who this is for:** You don't need to be a systems engineer. Each test tells you
> exactly what to run, what to look for, and what "good" vs "bad" looks like.
> If something says PASS, the market is working. If it says FAIL, something needs fixing.

---

## How This Works

The Agentic scheduler replaces Linux's default "be fair to everyone" approach with a
**token economy** where processes bid for CPU time. Think of it like an auction house
that runs thousands of times per second.

We test it in **5 milestones**, each building on the last. You can stop at any
milestone — they're designed to catch problems early before going deeper.

**Safety net:** The kernel has a 5-second watchdog. If our scheduler freezes,
Linux automatically kicks it out and goes back to normal. Your machine won't brick.

---

## Before You Start

```bash
# Make sure the binary is built and fresh
cargo build -p scx_agentic

# Confirm no other sched_ext scheduler is running
cat /sys/kernel/sched_ext/state
# Should say: "disabled"
# If it says "enabled", find and kill the other scheduler first

# Confirm you're root
whoami
# Should say: root
```

---

## Milestone 1: "Does It Even Load?"

**What we're testing:** Can the scheduler load into the kernel without crashing?
This is the bare minimum — if this fails, nothing else matters.

**Time:** ~15 seconds

```bash
# Run the scheduler for 10 seconds, then auto-kill it
sudo timeout 10 ./target/debug/scx_agentic --verbose 2>&1 | tee /tmp/m1_load.log

# Check: did it load?
echo "=== LOAD CHECK ==="
grep -i "agentic" /tmp/m1_load.log | head -5

# Check: did it exit cleanly (not crash)?
echo "=== EXIT CHECK ==="
tail -3 /tmp/m1_load.log
```

### What PASS looks like:
- You see a line like `Agentic version 0.1.0 - market scheduler, 2 CPUs, 2000000 total tokens`
- The log ends with `Unregister Agentic scheduler` or a clean exit
- Your terminal is responsive (you can type)

### What FAIL looks like:
- Error about BPF loading or verification (means the C code has a bug)
- `SIGKILL` or no output (means it crashed hard)
- Terminal hangs for 5+ seconds then recovers (watchdog saved you — there's a dispatch bug)

---

## Milestone 2: "Can It Actually Schedule Work?"

**What we're testing:** With real CPU work happening, does the scheduler keep things
moving? Or do tasks starve and the system freeze?

**Time:** ~35 seconds

```bash
# Start the scheduler in the background, logging to file
sudo ./target/debug/scx_agentic --verbose 2>/tmp/m2_sched.log &
SCHED_PID=$!
sleep 2  # Let it initialize

# Now throw some work at it
echo "=== Starting workload ==="

# CPU-bound work: calculate primes (2 workers for 20 seconds)
stress-ng --cpu 2 --timeout 20s --metrics-brief 2>&1 | tee /tmp/m2_stress.log &
STRESS_PID=$!

# I/O work: write 100MB to /dev/null (tests mixed workloads)
dd if=/dev/urandom of=/dev/null bs=1M count=100 2>/tmp/m2_dd.log &

# Interactive work: a simple command that should respond quickly
sleep 5
TIME_START=$(date +%s%N)
echo "hello" > /dev/null
TIME_END=$(date +%s%N)
LATENCY=$(( (TIME_END - TIME_START) / 1000000 ))
echo "=== Interactive latency: ${LATENCY}ms ==="

# Wait for stress test to finish
wait $STRESS_PID 2>/dev/null

# Kill the scheduler
sudo kill $SCHED_PID 2>/dev/null
wait $SCHED_PID 2>/dev/null
sleep 1

# Results
echo ""
echo "=== DISPATCH CHECK ==="
grep -c "dispatch\|market" /tmp/m2_sched.log || echo "0 dispatches found"

echo "=== STRESS RESULTS ==="
cat /tmp/m2_stress.log

echo "=== DD RESULTS ==="
cat /tmp/m2_dd.log
```

### What PASS looks like:
- `stress-ng` completes without errors
- `dd` finishes (shows bytes copied)
- Interactive latency is under 100ms (ideally under 10ms)
- The scheduler log shows dispatch activity

### What FAIL looks like:
- `stress-ng` hangs or reports errors
- Interactive latency is >500ms (the market is starving interactive work)
- The scheduler log shows "TOKEN SUPPLY VIOLATION" (conservation bug)
- System becomes unresponsive during the test

---

## Milestone 3: "Is The Market Actually Working?"

**What we're testing:** Are tokens being spent? Are bids being made? Is the
treasury system functioning, not just passing tasks through unchanged?

**Time:** ~25 seconds

```bash
# Run with verbose logging, capture everything
sudo timeout 20 ./target/debug/scx_agentic --verbose 2>&1 | tee /tmp/m3_market.log &
SCHED_PID=$!
sleep 2

# Create diverse workload to exercise the market
# "Interactive" work (short bursts)
for i in $(seq 1 20); do
    (echo "scale=100; 4*a(1)" | bc -l > /dev/null 2>&1) &
done

# "Batch" work (sustained CPU)
stress-ng --cpu 2 --timeout 15s 2>/dev/null &

# "I/O" work
dd if=/dev/zero of=/dev/null bs=4k count=100000 2>/dev/null &

# Wait
sleep 18
sudo kill $SCHED_PID 2>/dev/null
wait 2>/dev/null
sleep 1

echo "========================================="
echo "  MARKET HEALTH REPORT"
echo "========================================="
echo ""

# Check 1: Token supply integrity
echo "--- Token Supply ---"
if grep -q "TOKEN SUPPLY VIOLATION" /tmp/m3_market.log; then
    echo "FAIL: Token supply is leaking!"
    grep "TOKEN SUPPLY VIOLATION" /tmp/m3_market.log | tail -3
else
    echo "PASS: Token supply is conserved"
fi
echo ""

# Check 2: Circuit breaker behavior
echo "--- Circuit Breaker ---"
TRIPS=$(grep -c "Circuit breaker TRIPPED" /tmp/m3_market.log || echo "0")
RELEASES=$(grep -c "Circuit breaker released" /tmp/m3_market.log || echo "0")
echo "Trips: $TRIPS | Releases: $RELEASES"
if [ "$TRIPS" = "$RELEASES" ] || [ "$TRIPS" = "0" ]; then
    echo "PASS: Circuit breaker is balanced (or never needed)"
else
    echo "WARN: Trips and releases don't match — check timing"
fi
echo ""

# Check 3: Central Bank is alive
echo "--- Central Bank ---"
if grep -q "Central Bank daemon started" /tmp/m3_market.log; then
    echo "PASS: Central Bank is running"
else
    echo "FAIL: Central Bank didn't start"
fi
echo ""

# Check 4: Haircut protocol
echo "--- Haircut Protocol ---"
HAIRCUTS=$(grep -c "Haircut complete" /tmp/m3_market.log || echo "0")
echo "Haircuts executed: $HAIRCUTS"
echo "(Haircuts only happen if circuit breaker trips — 0 is fine under normal load)"
echo ""

# Check 5: Exogenous event detection
echo "--- Exogenous Events ---"
EXOG=$(grep -c "Exogenous event" /tmp/m3_market.log || echo "0")
echo "Exogenous events detected: $EXOG"
echo ""

# Overall
echo "--- Summary ---"
LINES=$(wc -l < /tmp/m3_market.log)
echo "Total log lines: $LINES"
echo "Full log at: /tmp/m3_market.log"
```

### What PASS looks like:
- Token supply conserved (no violations)
- Circuit breaker balanced (trips == releases, or 0 trips)
- Central Bank running
- The system stayed responsive throughout

### What FAIL looks like:
- Token supply violations (means tokens are being created or destroyed somewhere)
- Circuit breaker fires repeatedly without releasing (dispatch stuck)
- Central Bank didn't start (thread spawn failed)

---

## Milestone 4: "Stress Test — Break It On Purpose"

**What we're testing:** What happens when we overload the system? Does the circuit
breaker catch it? Does the haircut protocol reset the market? Does everything
recover?

**Time:** ~45 seconds

```bash
# Run the scheduler
sudo ./target/debug/scx_agentic --verbose 2>/tmp/m4_stress.log &
SCHED_PID=$!
sleep 2

echo "=== Phase A: Normal load (baseline) ==="
stress-ng --cpu 1 --timeout 5s 2>/dev/null
sleep 1

echo "=== Phase B: Overload (should trigger circuit breaker) ==="
# Spawn many short-lived CPU hogs
for i in $(seq 1 50); do
    stress-ng --cpu 1 --timeout 10s 2>/dev/null &
done

# While overloaded, check if we can still run a command
sleep 3
TIME_START=$(date +%s%N)
ls /tmp > /dev/null
TIME_END=$(date +%s%N)
LATENCY=$(( (TIME_END - TIME_START) / 1000000 ))
echo "Interactive latency during overload: ${LATENCY}ms"

# Wait for the storm to pass
sleep 12
wait 2>/dev/null

echo "=== Phase C: Recovery ==="
# Simple work after the storm
stress-ng --cpu 1 --timeout 5s --metrics-brief 2>&1 | tee /tmp/m4_recovery.log
sleep 1

# Kill scheduler
sudo kill $SCHED_PID 2>/dev/null
wait $SCHED_PID 2>/dev/null
sleep 1

echo ""
echo "========================================="
echo "  STRESS TEST REPORT"
echo "========================================="

echo ""
echo "--- Circuit Breaker Activity ---"
TRIPS=$(grep -c "Circuit breaker TRIPPED" /tmp/m4_stress.log || echo "0")
RELEASES=$(grep -c "Circuit breaker released" /tmp/m4_stress.log || echo "0")
echo "Trips: $TRIPS | Releases: $RELEASES"
if [ "$TRIPS" -gt "0" ]; then
    echo "PASS: Circuit breaker activated under stress (this is correct behavior!)"
else
    echo "INFO: Circuit breaker didn't trip (threshold may need tuning)"
fi

echo ""
echo "--- Haircut Protocol ---"
HAIRCUTS=$(grep -c "Haircut complete" /tmp/m4_stress.log || echo "0")
echo "Haircuts: $HAIRCUTS"

echo ""
echo "--- Token Integrity ---"
if grep -q "TOKEN SUPPLY VIOLATION" /tmp/m4_stress.log; then
    echo "FAIL: Tokens leaked during stress!"
else
    echo "PASS: Token supply survived the storm"
fi

echo ""
echo "--- Recovery ---"
if [ -s /tmp/m4_recovery.log ]; then
    echo "PASS: System recovered and ran post-storm workload"
else
    echo "FAIL: Recovery workload didn't produce output"
fi

echo ""
echo "--- Interactive Responsiveness ---"
echo "Latency during overload: ${LATENCY}ms"
if [ "$LATENCY" -lt "1000" ]; then
    echo "PASS: System stayed responsive under load"
else
    echo "WARN: High latency during overload (expected under extreme load)"
fi

echo ""
echo "Full log: /tmp/m4_stress.log"
```

### What PASS looks like:
- Circuit breaker fires during the overload burst (this is correct! It's protecting the system)
- Circuit breaker releases after 100ms and the market reopens
- Token supply stays intact through the whole ordeal
- Post-storm workload completes normally (system recovered)
- Interactive command runs during overload (maybe slow, but not stuck)

### What FAIL looks like:
- System hangs during overload (watchdog kicks the scheduler out)
- Token supply violations after haircut (the slash math is wrong)
- System never recovers after the storm (scheduler is broken)

---

## Milestone 5: "The Full Picture — Sustained Run"

**What we're testing:** Can the scheduler run for 60 seconds under mixed load
without any problems? This is the graduation test.

**Time:** ~75 seconds

```bash
# Run the scheduler
sudo ./target/debug/scx_agentic --verbose 2>/tmp/m5_sustained.log &
SCHED_PID=$!
sleep 2

echo "=== 60-second sustained test ==="

# Background: sustained CPU work
stress-ng --cpu 1 --timeout 55s 2>/dev/null &

# Background: periodic I/O bursts
(for i in $(seq 1 10); do
    dd if=/dev/urandom of=/dev/null bs=1M count=10 2>/dev/null
    sleep 5
done) &

# Background: periodic short-lived processes (tests treasury creation/GC)
(for i in $(seq 1 100); do
    echo "scale=10; 4*a(1)" | bc -l > /dev/null 2>&1
    sleep 0.5
done) &

# Foreground: periodic latency checks
echo "Checking latency every 10 seconds..."
for i in $(seq 1 6); do
    sleep 10
    TIME_START=$(date +%s%N)
    ls /tmp > /dev/null
    TIME_END=$(date +%s%N)
    LATENCY=$(( (TIME_END - TIME_START) / 1000000 ))
    echo "  [${i}0s] Latency: ${LATENCY}ms"
done

# Cleanup
sudo kill $SCHED_PID 2>/dev/null
wait 2>/dev/null
sleep 1

echo ""
echo "========================================="
echo "  SUSTAINED RUN REPORT"
echo "========================================="

echo ""
echo "--- Duration ---"
FIRST=$(head -1 /tmp/m5_sustained.log | grep -oP '\d{2}:\d{2}:\d{2}' || echo "?")
LAST=$(tail -1 /tmp/m5_sustained.log | grep -oP '\d{2}:\d{2}:\d{2}' || echo "?")
echo "Ran from $FIRST to $LAST"
LINES=$(wc -l < /tmp/m5_sustained.log)
echo "Log lines: $LINES"

echo ""
echo "--- Token Economy ---"
if grep -q "TOKEN SUPPLY VIOLATION" /tmp/m5_sustained.log; then
    VIOLATIONS=$(grep -c "TOKEN SUPPLY VIOLATION" /tmp/m5_sustained.log)
    echo "FAIL: $VIOLATIONS token supply violations"
else
    echo "PASS: Token supply conserved for entire run"
fi

echo ""
echo "--- Circuit Breaker ---"
TRIPS=$(grep -c "Circuit breaker TRIPPED" /tmp/m5_sustained.log || echo "0")
echo "Trips during sustained run: $TRIPS"
if [ "$TRIPS" -lt "5" ]; then
    echo "PASS: Market was stable (< 5 trips)"
else
    echo "WARN: Frequent circuit breaker trips — market may be too sensitive"
fi

echo ""
echo "--- Central Bank ---"
if grep -q "Central Bank daemon started" /tmp/m5_sustained.log; then
    echo "PASS: Central Bank ran throughout"
else
    echo "FAIL: Central Bank missing"
fi

echo ""
echo "--- Shadow Core ---"
EXOG=$(grep -c "Exogenous event" /tmp/m5_sustained.log || echo "0")
echo "Exogenous events: $EXOG"

echo ""
echo "Full log: /tmp/m5_sustained.log"
echo ""
echo "========================================="
echo "  To inspect the full market activity:"
echo "  less /tmp/m5_sustained.log"
echo "========================================="
```

### What PASS looks like:
- Runs the full 60 seconds without crashing
- Token supply conserved the entire time
- Fewer than 5 circuit breaker trips under normal load
- Central Bank daemon running
- Interactive latency stays under 100ms consistently

### What FAIL looks like:
- Scheduler gets killed by the watchdog mid-run
- Token supply violations accumulate over time (leak)
- Circuit breaker fires constantly (market is broken)
- Latency degrades over time (memory leak or treasury bloat)

---

## Quick Reference: What Each System Does

| System | What it does | How you know it's working |
|--------|-------------|--------------------------|
| **Treasury** | Every process has a token balance | No "TOKEN SUPPLY VIOLATION" |
| **Order Book** | Highest bidder gets the CPU | Tasks actually run (stress-ng completes) |
| **Circuit Breaker** | Emergency stop when market thrashes | Trips during overload, releases after 100ms |
| **Haircut** | Slashes excess wealth during emergencies | "Haircut complete" in logs during breaker |
| **Demurrage** | Idle tokens decay back to reserve | Reserve doesn't drop to zero over time |
| **Shadow Core** | Control group running old-school scheduling | "Exogenous event" filtering in logs |
| **Bidding Agents** | PID controllers that auto-tune bids | Tasks get scheduled (not starved) |
| **Central Bank** | Background daemon adjusting policy | "Central Bank daemon started" in logs |

---

## If Something Goes Wrong

**System hangs:** Wait 5 seconds. The kernel watchdog will kill the scheduler
and restore normal scheduling. If you're SSH'd in, your connection should survive.

**Scheduler crashes immediately:** Check `/tmp/m1_load.log` for BPF verification
errors. These mean the C code in `main.bpf.c` has a bug.

**Token supply violations:** The market economy is leaking money. Check the
`dispatch_task()` and `haircut_protocol()` functions — the math for deducting
and refunding tokens has a bug.

**Circuit breaker never stops firing:** The threshold (500% of baseline) may be
too low for this machine. The constant `CIRCUIT_BREAKER_THRESHOLD` in `main.rs`
can be raised.

**Everything passes but latency is bad:** The demurrage rate might be too
aggressive (draining tokens before processes can use them). Or the bid heuristic
(10% of balance) might be too conservative. These are tuning problems, not bugs.
