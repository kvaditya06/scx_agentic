# The Agentic Kernel: Revised Engineering Plan

## Preamble — Why This Revision Exists

The original manifesto (`agentic_kernel.md`) describes an ambitious and architecturally sound vision: replace static scheduling heuristics with an explicit micro-economy where processes bid for CPU time. This revised plan preserves that vision but corrects several factual assumptions about the scx_rustland codebase and fills structural gaps that would block implementation.

### Key corrections from the integrity review:

1. **scx_rustland is NOT a FIFO scheduler.** It uses a deadline-priority `BTreeSet` sorted by `vruntime + exec_runtime`, weight-scaled. The "communist breadline" metaphor does not describe this scheduler — it already has a priority mechanism. Our work replaces a *virtual-time fairness model* with an *economic bidding model*.

2. **The existing data contract (`intf.h`) defines `queued_task_ctx` and `dispatched_task_ctx`** — not the plan's `task_bid` and `dispatch_slot`. A wholesale struct replacement would break every hook in `main.bpf.c`. We must migrate incrementally.

3. **The `dispatch_slot` in the original plan omits critical fields**: `cpu` (which DSQ to target), `enq_cnt` (generation counter that prevents stale dispatches), and `flags`. Without these, BPF dispatch is broken.

4. **BPF source code lives in `scx_rustland_core/assets/`**, not directly in the scheduler directory. The files in `scheds/rust/scx_rustland/` are generated at build time. We must either fork the core or create a new scheduler crate.

5. **`BinaryHeap` is the wrong data structure.** The existing `BTreeSet` supports O(log n) arbitrary removal (needed when tasks exit or are cancelled). `BinaryHeap` does not. We keep the `BTreeSet` and change its ordering to bid-based.

---

## Architecture Overview

The four-layer architecture from the original plan remains correct:

1. **The Hook (eBPF/C):** Intercepts scheduling events, writes task metadata to ring buffers, reads dispatch decisions back.
2. **The Clearinghouse (Rust):** An Order Book that ingests bids and clears the market. Replaces the current vruntime-deadline scheduler.
3. **The Bidding Agents (User Space):** PID/ESC controllers that determine bid amounts per process.
4. **The Central Bank (LLM):** Async oracle that manages token supply and demurrage rate.

### Communication Topology (Unchanged)

```
BPF Hook ──[BPF_MAP_TYPE_RINGBUF]──> Rust Clearinghouse
BPF Hook <──[BPF_MAP_TYPE_USER_RINGBUF]── Rust Clearinghouse
BPF Hook <──[BPF_MAP_TYPE_ARRAY/HASH]──> Rust Clearinghouse  (new: treasury, market_state)
```

---

## Development Rules

* **Zero-Syscall Hot Path:** All shared state between BPF and Rust via eBPF maps. No syscalls in the scheduling loop.
* **O(log n) Dispatch:** Market clearing via `BTreeSet` is O(log n). This is acceptable — O(1) would require a bucket-based structure that sacrifices tie-breaking precision.
* **Headless Environment:** Ubuntu 24.04 HWE, no GUI.
* **Iterative Build:** Each phase must compile and pass basic scheduling tests before the next phase begins.
* **Preserve `enq_cnt` invariant:** Every dispatch decision must carry the task's generation counter. BPF rejects stale dispatches where `enq_cnt` has changed.

---

## Market Primitives

* **Token-to-Timeslice:** 1 Token = 100 microseconds of CPU time. A 50-token bid yields a 5ms timeslice.
* **Adaptive Tiered Sleep** for the Clearinghouse polling loop:
  * *High Load (nr_queued > threshold):* Spin-poll ring buffer for 50us.
  * *Medium Load:* `std::thread::yield_now()`.
  * *Idle:* `epoll`/`bpf_ringbuf_poll` blocking wait.

---

## Phase 0: Fork and Scaffold

**Goal:** Create an independent scheduler crate that we can modify without breaking upstream scx_rustland.

### Tasks

1. **Create `scheds/rust/scx_agentic/`** as a new scheduler crate.
   - Copy `scx_rustland`'s `Cargo.toml`, `build.rs`, `src/main.rs`, `src/stats.rs`.
   - Copy `scx_rustland_core/assets/bpf/intf.h` and `main.bpf.c` into `scheds/rust/scx_agentic/` as local files.
   - Fork `scx_rustland_core/assets/bpf.rs` into `src/bpf.rs` as a local module.
   - Update `build.rs` to compile BPF from local sources instead of core assets (use `scx_cargo::BpfBuilder` directly).
   - Rename the BPF ops struct from `rustland` to `agentic`.

2. **Verify the fork compiles and schedules correctly** — it should behave identically to scx_rustland at this point.

3. **Add the shared header `agentic_kernel.h`** alongside `intf.h`. Initially empty — structs migrate here incrementally in later phases.

### Exit Criteria
- `cargo build` succeeds for `scx_agentic`.
- The scheduler can be loaded and run basic workloads (e.g., `stress-ng --cpu 4`).
- All existing tests pass.

---

## Phase 1: Core Market Mechanics (The Order Book)

**Goal:** Replace vruntime-deadline ordering with bid-based ordering. Static heuristic bids only — no agents yet.

### Step 1.1: Extend the Data Contract

**`intf.h` changes** — add market fields to existing structs rather than replacing them:

```c
// Add to queued_task_ctx:
    u64 bid_amount;          // Tokens offered this auction tick
    u64 treasury_balance;    // Snapshot before bid
    u64 starvation_ns;       // Time since last dispatch
    u32 class_hint;          // process_class_t (new enum, see below)
    u32 numa_node;           // NUMA domain

// Add to dispatched_task_ctx:
    u64 clearing_price;      // Tokens deducted from winner
    u64 sequence;            // Monotonic auction cycle counter
    // KEEP existing: cpu, flags, enq_cnt, slice_ns, vtime
```

Add `process_class_t` enum and new structs to `agentic_kernel.h`:

```c
typedef enum {
    CLASS_INTERACTIVE = 0,
    CLASS_BATCH       = 1,
    CLASS_DAEMON      = 2,
    CLASS_REALTIME    = 3,
    CLASS_UNKNOWN     = 4,
} process_class_t;

struct treasury_entry {
    __u64 balance;
    __u64 last_update_ts;
    __u64 total_spent;
    __u8  ipo_complete;
    __u8  process_class;
    __u8  pad[6];
} __attribute__((packed));

struct market_state {
    __u64 current_clearing_price;
    __u64 total_tokens_in_circulation;
    __u32 circuit_breaker_tripped;
    __u32 current_demurrage_rate;
} __attribute__((packed));
```

### Step 1.2: Add BPF Maps for Treasury and Market State

In `main.bpf.c`:

```c
// Per-process treasury — Rust is sole writer, BPF reads
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __type(key, __s32);                    // pid
    __type(value, struct treasury_entry);
    __uint(max_entries, 131072);           // MAX_TRACKED_PIDS
} treasury SEC(".maps");

// Per-NUMA market state — Rust writes, BPF reads
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __type(key, __u32);                    // numa_node index
    __type(value, struct market_state);
    __uint(max_entries, 8);                // MAX_NUMA_NODES
} market SEC(".maps");
```

### Step 1.3: Static Heuristic Bidding in BPF

In the `rustland_enqueue` (now `agentic_enqueue`) hook, populate the new fields before submitting to the ring buffer:

```c
// Static bid heuristic (replaced by agents in Phase 5)
struct treasury_entry *te = bpf_map_lookup_elem(&treasury, &p->pid);
u64 balance = te ? te->balance : IPO_BASELINE;
task->bid_amount = balance / 10;  // Bid 10% of treasury
task->treasury_balance = balance;
task->starvation_ns = now - tctx->last_dispatch_ts;
task->class_hint = CLASS_UNKNOWN;
task->numa_node = cyclic_node_id(p->cpus_ptr);
```

### Step 1.4: Replace Task Ordering in Rust

Change the `Task` struct and its `Ord` implementation in `main.rs`:

```rust
#[derive(Debug, PartialEq, Eq, Clone)]
struct Task {
    qtask: QueuedTask,
    bid_amount: u64,
    enqueue_time: u64,  // nanosecond timestamp for tie-breaking
}

impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Max-heap semantics: highest bid wins (reverse order in BTreeSet)
        other.bid_amount
            .cmp(&self.bid_amount)
            .then_with(|| self.enqueue_time.cmp(&other.enqueue_time))  // oldest wins tie
            .then_with(|| self.qtask.pid.cmp(&other.qtask.pid))
    }
}
```

### Step 1.5: Market Clearing in Rust

Add `OrderBook` abstraction wrapping the `BTreeSet<Task>`:

```rust
struct OrderBook {
    book: BTreeSet<Task>,
    sequence: u64,
}

impl OrderBook {
    fn submit_bid(&mut self, task: Task) {
        self.book.insert(task);
    }

    fn clear_market(&mut self) -> Option<(Task, u64)> {
        let winner = self.book.pop_first()?;
        let clearing_price = winner.bid_amount;
        self.sequence += 1;
        Some((winner, clearing_price))
    }
}
```

### Step 1.6: Dispatch with Market Fields

When dispatching, fill the new fields on `DispatchedTask`:

```rust
fn dispatch_task(&mut self) -> bool {
    let Some((winner, clearing_price)) = self.order_book.clear_market() else {
        return true;
    };

    let mut dispatched = DispatchedTask::new(&winner.qtask);
    dispatched.slice_ns = clearing_price * 100_000;  // tokens -> nanoseconds
    dispatched.clearing_price = clearing_price;
    dispatched.sequence = self.order_book.sequence;
    // PRESERVE existing CPU selection logic
    dispatched.cpu = self.select_cpu_for(&winner.qtask);
    // PRESERVE enq_cnt
    dispatched.enq_cnt = winner.qtask.enq_cnt;

    if self.bpf.dispatch_task(&dispatched).is_err() {
        self.order_book.submit_bid(winner);  // re-enqueue on failure
        return false;
    }
    true
}
```

### Step 1.7: Adaptive Tiered Sleep

Replace the current tight polling loop with:

```rust
fn schedule(&mut self) {
    let nr_queued = self.bpf.nr_queued();
    let nr_running = self.bpf.nr_running();

    if nr_running > self.high_load_threshold {
        // Hot path: spin-poll for 50us
        let deadline = Instant::now() + Duration::from_micros(50);
        while Instant::now() < deadline {
            self.drain_queued_tasks();
        }
    } else if nr_queued > 0 {
        // Medium: yield and retry
        std::thread::yield_now();
        self.drain_queued_tasks();
    } else {
        // Idle: block on ring buffer
        self.bpf.queued_poll(Duration::from_millis(100));
        self.drain_queued_tasks();
    }

    self.dispatch_tasks();
    self.bpf.notify_complete(self.order_book.len() as u64);
}
```

### Step 1.8: BPF Sequence Validation

In `handle_dispatched_task` in `main.bpf.c`, add stale-slot rejection:

```c
static volatile __u64 last_sequence = 0;

static long handle_dispatched_task(struct bpf_dynptr *dynptr, void *context) {
    const struct dispatched_task_ctx *task;
    task = bpf_dynptr_data(dynptr, 0, sizeof(*task));
    if (!task)
        return 0;

    // Reject stale dispatch slots
    if (task->sequence <= last_sequence)
        return 0;
    last_sequence = task->sequence;

    dispatch_task(task);
    return !!scx_bpf_dispatch_nr_slots();
}
```

### Exit Criteria
- Scheduler compiles and loads.
- Tasks are ordered by bid amount (observable via debug logging).
- Timeslices reflect token-to-time conversion (bid * 100us).
- No regression in basic workload handling vs. stock scx_rustland.
- `enq_cnt` validation still prevents stale dispatches.

---

## Phase 2: Stability & The Circuit Breaker

**Goal:** Prevent market thrashing or deadlock during bidding wars.

### Step 2.1: Context-Switch Rate Monitoring in BPF

Add a per-CPU counter and rolling baseline:

```c
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __type(key, __u32);
    __type(value, __u64);
    __uint(max_entries, 1);
} csw_counter SEC(".maps");

// In agentic_stopping():
__u32 key = 0;
__u64 *count = bpf_map_lookup_elem(&csw_counter, &key);
if (count)
    __sync_fetch_and_add(count, 1);
```

The Rust side reads this map every 10ms, maintains a 1-second rolling average, and computes deviation.

### Step 2.2: Circuit Breaker Trip

When context-switch rate exceeds 500% of baseline:

1. Rust sets `market_state.circuit_breaker_tripped = 1` for the affected NUMA node.
2. BPF checks this flag in `agentic_enqueue`. If tripped, bypass the ring buffer and dispatch directly to the shared DSQ using native EEVDF-style vtime ordering.
3. The breaker holds for 100ms (configurable).

### Step 2.3: The Haircut Protocol

During the 100ms breaker window:

1. Rust drains and discards all pending bids in the `OrderBook`.
2. For every `treasury_entry` where `balance > IPO_BASELINE`:
   - Apply 90% slash: `balance = IPO_BASELINE + (balance - IPO_BASELINE) * 0.10`
3. Reset `market_state.circuit_breaker_tripped = 0`.
4. Market re-opens with cold-start balances.

### Step 2.4: Fallback Correctness

The BPF fallback path must use the existing dispatch infrastructure (per-CPU and shared DSQs with vtime ordering). This is already implemented in the current codebase's `dispatch_direct` path — we just need to gate it on the circuit breaker flag.

### Exit Criteria
- Artificial thrashing test (all processes bid maximum) triggers the breaker.
- System remains schedulable during the 100ms fallback window.
- Market re-opens cleanly with slashed balances.
- No deadlocks or task starvation during breaker events.

---

## Phase 3: The Monetary Base & Demurrage

**Goal:** Prevent runaway inflation and hoarding.

### Step 3.1: Token Supply Invariant

- Hard cap: 1,000,000 tokens per CPU core.
- On scheduler init, Rust calculates `total_supply = nr_online_cpus * 1_000_000`.
- IPO baseline per process: `total_supply / max_expected_processes` (tunable, default: 1000 tokens).
- The Central Bank reserve holds all unallocated tokens. Sum of all `treasury_entry.balance` + reserve = `total_supply` at all times.

### Step 3.2: Demurrage (Wealth Tax)

Every millisecond, the Rust Clearinghouse applies decay to all treasuries:

```rust
fn apply_demurrage(&mut self) {
    let now = Instant::now();
    let elapsed_ms = now.duration_since(self.last_demurrage).as_millis();
    if elapsed_ms == 0 { return; }

    let lambda = self.demurrage_rate;  // 0.001% to 0.1% per ms

    for entry in self.treasuries.values_mut() {
        let tax = (entry.balance as f64 * lambda * elapsed_ms as f64) as u64;
        entry.balance = entry.balance.saturating_sub(tax);
        self.reserve += tax;
    }

    self.last_demurrage = now;
}
```

The demurrage rate (lambda) is stored in `market_state.current_demurrage_rate` and bounded between 1 (0.001%) and 100 (0.1%), scaled by 1000x for integer representation.

### Step 3.3: Fork Policy

Attach to `sched_process_fork` via a BPF tracepoint or the existing `init_task` hook:

- Child processes receive `IPO_BASELINE` tokens from the reserve — NOT from the parent.
- Parent-to-child explicit transfers are permitted via a new BPF map acting as an IPC channel:

```c
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __type(key, __s32);                        // child pid
    __type(value, __u64);                      // transfer amount
    __uint(max_entries, 4096);
} transfer_requests SEC(".maps");
```

Rust polls this map, validates the transfer (parent has sufficient balance, rate limit not exceeded), and updates both treasuries.

### Step 3.4: BPF Rate Limiting

A per-process transfer rate counter in BPF:

```c
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __type(key, __s32);                        // parent pid
    __type(value, __u64);                      // transfers in current window
    __uint(max_entries, 131072);
} transfer_rate SEC(".maps");
```

If a process exceeds the rate cap (e.g., 10 transfers per second), BPF drops the request before waking Rust.

### Exit Criteria
- Token supply invariant holds under load (total balances + reserve = total_supply).
- Demurrage visibly decays idle process balances over time.
- Fork creates child with baseline tokens, parent balance unchanged.
- Rate-limited transfers are dropped in BPF (observable via counter).

---

## Phase 4: Truth & Telemetry (Shadow Core)

**Goal:** Establish a causal feedback loop so the Central Bank can evaluate policy effectiveness.

### Step 4.1: NUMA-Aware Shadow Core Isolation

- On init, Rust identifies one core per NUMA node and marks it as a shadow core via CPU affinity.
- Shadow cores run standard EEVDF scheduling (BPF direct dispatch path, no market participation).
- A random 5% of newly enqueued tasks are routed to shadow cores by setting their `cpu` field to the shadow core ID in BPF.

Implementation: add a `shadow_core` per-CPU flag in a BPF array map. In `agentic_enqueue`, check a per-task random value (`bpf_get_prandom_u32() % 20 == 0`) and route to the shadow core's DSQ.

### Step 4.2: IPC Delta Measurement

Rust reads hardware performance counters via `perf_event_open` (or `/sys/devices/cpu/`) for each NUMA node:

- `IPC_market`: instructions-per-cycle on market cores.
- `IPC_shadow`: instructions-per-cycle on shadow cores.
- `delta_IPC = IPC_market - IPC_shadow` per NUMA node.

Store in a ring buffer for the Central Bank to consume.

### Step 4.3: Exogenous Event Filter

If both `IPC_market` and `IPC_shadow` drop simultaneously (within the same 100ms window), classify as exogenous (thermal throttle, page fault storm). The Central Bank ignores these events — it only adjusts policy when `delta_IPC` diverges (market drops while shadow holds steady, or vice versa).

### Exit Criteria
- Shadow cores are isolated and running EEVDF.
- 5% task routing is observable in dispatch statistics.
- `delta_IPC` is computed and logged per NUMA node.
- Exogenous events are correctly filtered out.

---

## Phase 5: The Cold Start (Deferred ESC)

**Goal:** Auto-tune bidding behavior without ruining application startup.

### Step 5.1: Process Classification via cgroup Hints

Add a cgroup-based classification system:

- Read `/sys/fs/cgroup/<group>/cpu.agentic.class` (or a custom xattr/file) at task init.
- Map to `process_class_t`: `CLASS_INTERACTIVE`, `CLASS_BATCH`, `CLASS_DAEMON`, `CLASS_REALTIME`.
- If no hint exists, assign `CLASS_UNKNOWN`.

Alternatively, support an `LD_PRELOAD` library that writes the class hint into a process-local BPF map entry at startup.

### Step 5.2: IPO Baseline by Class

| Class | IPO Baseline | Bid Strategy |
|-------|-------------|--------------|
| `CLASS_INTERACTIVE` | 2x standard | Aggressive (bid 20% of treasury) |
| `CLASS_BATCH` | 0.5x standard | Conservative (bid 5% of treasury) |
| `CLASS_DAEMON` | 1x standard | Deferred (wait for ESC tuning) |
| `CLASS_REALTIME` | N/A | Bypasses market entirely (BPF direct dispatch) |
| `CLASS_UNKNOWN` | 1x standard | Deferred (wait for ESC tuning) |

### Step 5.3: Extremum Seeking Control (ESC) Tuning

For processes in `CLASS_UNKNOWN` or `CLASS_DAEMON`:

1. Wait until context-switch frequency stabilizes (variance drops below threshold over a 500ms window) — indicates exit from initialization chaos.
2. Begin ESC: inject small sinusoidal perturbations to the process's bid multiplier.
3. Measure latency response (dispatch-to-running delay).
4. Map the latency curve and find the optimal bid multiplier.
5. Set `treasury_entry.ipo_complete = 1`. ESC never re-runs for this process instance.

### Step 5.4: Bidding Agent Attachment

Once ESC completes (or for classified processes immediately), attach a PID controller:

```rust
struct BiddingAgent {
    pid: i32,
    target_latency_ns: u64,
    kp: f64,  // proportional gain
    ki: f64,  // integral gain
    kd: f64,  // derivative gain
    integral: f64,
    prev_error: f64,
}

impl BiddingAgent {
    fn compute_bid(&mut self, current_latency_ns: u64, treasury_balance: u64) -> u64 {
        let error = self.target_latency_ns as f64 - current_latency_ns as f64;
        self.integral += error;
        let derivative = error - self.prev_error;
        self.prev_error = error;

        let adjustment = self.kp * error + self.ki * self.integral + self.kd * derivative;
        let bid = (treasury_balance as f64 * 0.10 * (1.0 + adjustment)).clamp(1.0, treasury_balance as f64 * 0.50);
        bid as u64
    }
}
```

The PID constants (`kp`, `ki`, `kd`) are set per-process by ESC tuning results, specific to that binary on its NUMA node.

### Exit Criteria
- Classified processes receive correct IPO baselines.
- ESC tuning converges for synthetic test processes.
- `ipo_complete` flag transitions from 0 to 1.
- PID controllers produce responsive bid adjustments under varying load.

---

## Phase 6: The Central Bank (LLM Oracle)

**Goal:** Connect physical hardware intent to macroeconomic token supply.

### Step 6.1: Async Daemon Architecture

A separate thread (or tokio task) runs every 1-3 seconds:

```rust
async fn central_bank_loop(telemetry: Arc<Telemetry>, policy: Arc<Mutex<Policy>>) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        let snapshot = telemetry.snapshot();
        let decision = llm_query(&snapshot).await;
        policy.lock().unwrap().apply(decision);
    }
}
```

This thread NEVER touches the scheduling hot path. It writes policy decisions to shared state that the Clearinghouse reads asynchronously.

### Step 6.2: Telemetry Ingestion

The LLM receives a JSON snapshot:

```json
{
  "numa_nodes": [
    {
      "id": 0,
      "delta_ipc": 0.15,
      "market_clearing_price": 42,
      "tokens_in_circulation": 950000,
      "circuit_breaker_trips_last_60s": 0
    }
  ],
  "top_starved_processes": [
    {"pid": 1234, "comm": "firefox", "starvation_ms": 50, "class": "interactive"}
  ],
  "wchan_summary": {"waiting_on_io": 12, "waiting_on_futex": 5, "runnable": 200},
  "demurrage_rate": 0.01,
  "reserve_balance": 50000
}
```

Sources:
- `delta_IPC` from Phase 4 shadow cores.
- `/proc/<pid>/wchan` for wait channel classification.
- `perf` buffer write detection for audio DAC / network socket activity (optional, Phase 6b).

### Step 6.3: Policy Outputs

The LLM can adjust:

1. **Demurrage rate (lambda):** Bounded `[0.001%, 0.1%]` per millisecond. Raising lambda forces hoarded tokens back to reserve. Lowering lambda stabilizes a volatile market.
2. **Token redistribution:** Route recycled demurrage tokens to specific process classes:
   - "Premium Latency" tokens to interactive/urgent threads.
   - "Bulk Throughput" tokens to batch jobs (lower priority but larger allocations).
3. **IPO baseline adjustment:** Shift the default baseline up or down based on system-wide utilization.

The LLM CANNOT mint new tokens — it can only redirect existing supply.

### Step 6.4: LLM Backend Options

- **Local:** `llama.cpp` with a small model (e.g., Llama 3 8B quantized) for air-gapped environments.
- **API:** Claude API or similar for higher-quality decisions in connected environments.
- **Fallback:** If LLM is unavailable, maintain current demurrage rate and distribute recycled tokens proportionally to starvation_ns.

### Exit Criteria
- Central Bank daemon runs independently of scheduling loop.
- Demurrage rate adjustments are observable in treasury decay rates.
- Token redistribution reaches starved interactive processes within 3 seconds.
- System degrades gracefully when LLM is unavailable (fallback policy).

---

## Appendix A: Revised Shared Data Contract

This is the canonical schema. The key principle is **extension, not replacement** — we add market fields to existing structs and introduce new structs alongside them.

### Extended `intf.h` (additions to existing structs)

```c
// === ADD to queued_task_ctx (after existing fields) ===
    u64 bid_amount;          // Tokens offered this auction tick
    u64 treasury_balance;    // Token balance snapshot before bid
    u64 starvation_ns;       // Nanoseconds since last CPU dispatch
    u32 class_hint;          // process_class_t
    u32 numa_node;           // NUMA topology domain

// === ADD to dispatched_task_ctx (after existing fields) ===
    u64 clearing_price;      // Tokens deducted from winner's treasury
    u64 sequence;            // Monotonic auction cycle counter
    // NOTE: cpu, flags, enq_cnt, slice_ns, vtime are PRESERVED
```

### New `agentic_kernel.h`

```c
#ifndef __AGENTIC_KERNEL_H
#define __AGENTIC_KERNEL_H

#include <linux/types.h>

#define MAX_TRACKED_PIDS    131072
#define MAX_NUMA_NODES      8
#define IPO_BASELINE        1000       // Default tokens per new process
#define TOKENS_PER_CORE     1000000    // Hard cap per CPU core

typedef enum {
    CLASS_INTERACTIVE = 0,
    CLASS_BATCH       = 1,
    CLASS_DAEMON      = 2,
    CLASS_REALTIME    = 3,
    CLASS_UNKNOWN     = 4,
} process_class_t;

// Per-process token ledger
// Rust Clearinghouse is sole writer. BPF is read-only.
// Map type: BPF_MAP_TYPE_HASH, key = pid (s32)
struct treasury_entry {
    __u64 balance;
    __u64 last_update_ts;
    __u64 total_spent;
    __u8  ipo_complete;      // 0 = in IPO/ESC tuning, 1 = steady state
    __u8  process_class;     // process_class_t
    __u8  pad[6];
} __attribute__((packed));

// Per-NUMA market state
// Rust writes, BPF reads zero-copy.
// Map type: BPF_MAP_TYPE_ARRAY, key = numa_node (u32), max_entries = MAX_NUMA_NODES
struct market_state {
    __u64 current_clearing_price;
    __u64 total_tokens_in_circulation;
    __u32 circuit_breaker_tripped;   // 1 = tripped, 0 = normal
    __u32 current_demurrage_rate;    // lambda * 1000 (integer: 1 = 0.001%, 100 = 0.1%)
} __attribute__((packed));

// Transfer request (parent -> child token transfer)
// Map type: BPF_MAP_TYPE_HASH, key = child pid (s32)
struct transfer_request {
    __s32 parent_pid;
    __u64 amount;
} __attribute__((packed));

#endif // __AGENTIC_KERNEL_H
```

---

## Appendix B: BPF Map Summary

| Map Name | Type | Key | Value | Writer | Reader | Phase |
|----------|------|-----|-------|--------|--------|-------|
| `queued` | RINGBUF | — | `queued_task_ctx` | BPF | Rust | 0 (existing) |
| `dispatched` | USER_RINGBUF | — | `dispatched_task_ctx` | Rust | BPF | 0 (existing) |
| `treasury` | HASH | `s32` (pid) | `treasury_entry` | Rust | BPF | 1 |
| `market` | ARRAY | `u32` (numa) | `market_state` | Rust | BPF | 1 |
| `csw_counter` | PERCPU_ARRAY | `u32` (0) | `u64` | BPF | Rust | 2 |
| `transfer_requests` | HASH | `s32` (child pid) | `transfer_request` | BPF | Rust | 3 |
| `transfer_rate` | HASH | `s32` (parent pid) | `u64` (count) | BPF | BPF | 3 |
| `shadow_cores` | ARRAY | `u32` (cpu) | `u8` (is_shadow) | Rust | BPF | 4 |

---

## Appendix C: Risk Register

| Risk | Impact | Mitigation | Phase |
|------|--------|------------|-------|
| Ring buffer overflow under bid storm | Tasks dropped, starvation | Circuit breaker (Phase 2) + increase `MAX_ENQUEUED_TASKS` | 1-2 |
| Demurrage rate oscillation | Market instability | Bound lambda changes to 10% per adjustment period | 3 |
| ESC perturbations cause latency spikes | User-visible jank | Limit perturbation amplitude to 5% of current bid; abort if latency exceeds 2x target | 5 |
| LLM latency > 3s | Stale policy decisions | Fallback to proportional redistribution; never block on LLM | 6 |
| `enq_cnt` race after market migration | Stale dispatches accepted | Preserve existing `enq_cnt` validation unchanged | 1 |
| Shadow core sample bias | Inaccurate delta_IPC | Randomize sample selection per-enqueue, not per-process | 4 |
| Treasury map exceeds 131072 entries | BPF map full, new processes can't be tracked | Garbage-collect exited processes on `sched_process_exit` tracepoint | 3 |
