
# The Agentic OS Kernel: Complete Implementation Plan

> **Purpose:** This document is a self-contained, LLM-executable engineering specification. It contains every detail an AI coding agent needs to implement a market-based CPU scheduler on top of the Linux `sched_ext` framework, building from the existing `scx_rustland` scheduler in this repository. Each phase includes exact file paths, line-level modification targets, complete code, validation steps, and explicit constraints. No external context is required.

---

## Table of Contents

1. [Vision and Mental Model](#1-vision-and-mental-model)
2. [Codebase Reality — What Exists Today](#2-codebase-reality--what-exists-today)
3. [Architecture](#3-architecture)
4. [Invariants and Development Rules](#4-invariants-and-development-rules)
5. [Market Primitives](#5-market-primitives)
6. [Phase 0: Fork and Scaffold](#6-phase-0-fork-and-scaffold)
7. [Phase 1: Core Market Mechanics](#7-phase-1-core-market-mechanics-the-order-book)
8. [Phase 2: Stability and the Circuit Breaker](#8-phase-2-stability-and-the-circuit-breaker)
9. [Phase 3: The Monetary Base and Demurrage](#9-phase-3-the-monetary-base-and-demurrage)
10. [Phase 4: Truth and Telemetry (Shadow Core)](#10-phase-4-truth-and-telemetry-shadow-core)
11. [Phase 5: The Cold Start (Deferred ESC)](#11-phase-5-the-cold-start-deferred-esc)
12. [Phase 6: The Central Bank (LLM Oracle)](#12-phase-6-the-central-bank-llm-oracle)
13. [Appendix A: Complete Shared Data Contract](#appendix-a-complete-shared-data-contract)
14. [Appendix B: BPF Map Registry](#appendix-b-bpf-map-registry)
15. [Appendix C: Risk Register](#appendix-c-risk-register)
16. [Appendix D: Validation Playbook](#appendix-d-validation-playbook)

---

## 1. Vision and Mental Model

We are replacing the Linux kernel's scheduling decision engine with a **High-Frequency Financial Market**. Instead of the kernel guessing what is important based on virtual runtime fairness, our system makes processes **explicitly bid for CPU time** using tokens drawn from a finite monetary supply.

### The Metaphor, Precisely

| Concept | Scheduler Analogy |
|---------|------------------|
| **Stock Exchange** | The Rust Clearinghouse — clears the auction every scheduling cycle |
| **Traders** | Bidding Agents — PID controllers attached to each process |
| **Currency** | Tokens — 1 token = 100 microseconds of CPU time |
| **Central Bank** | LLM Oracle — controls monetary policy (demurrage rate, token redistribution) |
| **IPO** | A new process entering the market, receiving its initial token allocation |
| **Circuit Breaker** | Emergency halt — falls back to EEVDF when the market thrashes |
| **Demurrage** | Wealth tax — unspent tokens decay back to the Central Bank reserve |

### What We Are Actually Replacing

The current `scx_rustland` scheduler uses a **deadline-priority model**:

```
deadline = vruntime + exec_runtime (capped at 100 slices)
```

Tasks are sorted by deadline in a `BTreeSet`. Lower deadline = scheduled first. Weight (priority) inversely scales virtual runtime accumulation. This is a **fairness model** — it tries to give every process its fair share.

Our market model replaces "fairness" with "intent funding." A process that needs low latency bids aggressively. A batch job bids conservatively. The system administrator's intent is encoded in token distribution, not in static nice values.

---

## 2. Codebase Reality — What Exists Today

> **CRITICAL: Read this section completely before writing any code. Every assumption about the codebase must come from here, not from intuition.**

### 2.1 File Map

**The scheduler we are forking:**
```
scheds/rust/scx_rustland/
  Cargo.toml              — Package manifest (version 1.1.0)
  build.rs                — Delegates to scx_rustland_core::RustLandBuilder
  src/
    main.rs               — 441 lines. Scheduler logic: Task struct, BTreeSet ordering,
                            drain_queued_tasks(), dispatch_task(), schedule() loop
    bpf.rs                — GENERATED at build time from core assets. Do NOT edit here.
    bpf_intf.rs           — GENERATED. Rust bindings from intf.h via bindgen.
    bpf_skel.rs           — GENERATED. BPF skeleton from main.bpf.c via libbpf.
    stats.rs              — Metrics struct and stats server
```

**The core library that generates BPF code:**
```
rust/scx_rustland_core/
  Cargo.toml              — Version 2.4.11
  src/
    lib.rs                — Exports VERSION, ALLOCATOR, RustLandBuilder
    rustland_builder.rs   — Build pipeline: embeds assets, runs BpfBuilder
  assets/
    bpf/
      intf.h              — 116 lines. THE canonical C/Rust struct contract
      main.bpf.c          — ~1180 lines. THE BPF scheduling hooks
    bpf.rs                — 603 lines. Rust BPF connector (QueuedTask, DispatchedTask, BpfScheduler)
```

**Build dependencies:**
```
rust/scx_cargo/           — BpfBuilder: compiles C to BPF, generates bindings
rust/scx_utils/           — Topology, UserExitInfo, build_id, compat macros
rust/scx_stats/           — Stats server framework
```

### 2.2 The Existing Data Contract (`intf.h`)

```c
// BPF -> Rust (via BPF_MAP_TYPE_RINGBUF "queued")
struct queued_task_ctx {
    s32 pid;
    s32 cpu;                 // CPU where the task was last running
    u64 nr_cpus_allowed;     // Affinity mask cardinality
    u64 flags;               // Enqueue flags (SCX_ENQ_*)
    u64 start_ts;            // Last time task started running (ns)
    u64 stop_ts;             // Last time task stopped running (ns)
    u64 exec_runtime;        // CPU time since last sleep (ns)
    u64 weight;              // Priority [1..10000], default 100
    u64 vtime;               // Current virtual runtime
    u64 enq_cnt;             // Generation counter (CRITICAL for correctness)
    char comm[16];           // Executable name
};

// Rust -> BPF (via BPF_MAP_TYPE_USER_RINGBUF "dispatched")
struct dispatched_task_ctx {
    s32 pid;
    s32 cpu;                 // Target CPU (RL_CPU_ANY = shared DSQ)
    u64 flags;               // Forwarded enqueue flags
    u64 slice_ns;            // Time slice in nanoseconds (0 = default)
    u64 vtime;               // Deadline / vruntime for DSQ ordering
    u64 enq_cnt;             // MUST match original — stale check in BPF
};
```

### 2.3 The `enq_cnt` Invariant

**This is the single most important correctness mechanism in the scheduler.**

In `main.bpf.c:689`:
```c
task->enq_cnt = ++tctx->enq_cnt;
```

In `main.bpf.c:560`:
```c
if (!tctx || tctx->enq_cnt > task->enq_cnt) {
    scx_bpf_dispatch_cancel();
    // ...
}
```

When a task is dequeued (e.g., it exits or sleeps) while still in the Rust scheduler's queue, BPF increments `enq_cnt`. When Rust later tries to dispatch the stale task, BPF sees that `tctx->enq_cnt > task->enq_cnt` and cancels the dispatch. **If you remove `enq_cnt` from `dispatched_task_ctx`, the scheduler will dispatch dead tasks and crash.**

### 2.4 The Build Pipeline

The build works in two levels:

1. `scx_rustland/build.rs` calls `scx_rustland_core::RustLandBuilder::new().build()`
2. `RustLandBuilder::build()` does:
   - Writes `intf.h` and `main.bpf.c` from embedded `include_bytes!()` assets to the build directory
   - Writes `src/bpf.rs` from embedded assets
   - Calls `scx_cargo::BpfBuilder` to compile C to BPF object, generate `bpf_intf.rs` (bindgen) and `bpf_skel.rs` (libbpf skeleton)

**Implication:** We CANNOT modify `intf.h` or `main.bpf.c` in the `scheds/rust/scx_rustland/` directory — those files are overwritten on every build. We must either:
- (a) Fork `scx_rustland_core` (invasive, affects other schedulers), OR
- (b) Create a new crate that uses `scx_cargo::BpfBuilder` directly with local source files

**We choose option (b).**

### 2.5 Current Dispatch Flow (Exact)

```
1. BPF: rustland_enqueue() is called when a task becomes runnable
   -> Writes queued_task_ctx to ring buffer "queued"
   -> Increments nr_queued

2. Rust: schedule() is called in a tight loop
   -> drain_queued_tasks():
      - Calls bpf.dequeue_task() which consumes from ring buffer
      - For each task: computes deadline = update_enqueued(task)
      - Inserts Task{qtask, deadline, timestamp} into BTreeSet
   -> dispatch_task():
      - Pops lowest-deadline task from BTreeSet
      - Creates DispatchedTask, sets slice_ns, vtime, cpu
      - Writes to user ring buffer "dispatched"
   -> notify_complete(nr_pending)

3. BPF: rustland_dispatch() is called when a CPU's local DSQ is empty
   -> Drains user ring buffer via handle_dispatched_task()
   -> For each task: calls dispatch_task() which:
      - Validates enq_cnt (rejects stale)
      - Dispatches to per-CPU DSQ or SHARED_DSQ
      - Kicks the target CPU
```

### 2.6 Existing Rust Task Ordering

```rust
// main.rs:131-154
struct Task {
    qtask: QueuedTask,
    deadline: u64,     // PRIMARY sort key (lower = scheduled first)
    timestamp: u64,    // SECONDARY sort key (earlier = wins tie)
}

impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)               // lowest deadline first
            .then_with(|| self.timestamp.cmp(&other.timestamp))  // oldest first on tie
            .then_with(|| self.qtask.pid.cmp(&other.qtask.pid))  // stable sort
    }
}
```

### 2.7 Key BPF Constants and Limits

```c
MAX_CPUS            = 1024
MAX_ENQUEUED_TASKS  = 4096
MAX_DISPATCH_SLOT   = 512  (MAX_ENQUEUED_TASKS / 8)
SHARED_DSQ          = MAX_CPUS (1024)
SCHED_DSQ           = MAX_CPUS + 1 (1025)
TASK_COMM_LEN       = 16
RL_CPU_ANY          = 1 << 20
```

---

## 3. Architecture

### 3.1 The Four Layers

```
Layer 4: Central Bank (LLM Oracle)
  |  Runs async every 1-3 seconds
  |  Adjusts: demurrage rate, token redistribution, IPO baseline
  |  NEVER touches the hot path
  v
Layer 3: Bidding Agents (per-process PID controllers)
  |  Compute bid_amount based on latency feedback
  |  Run inside the Clearinghouse (same Rust process)
  v
Layer 2: The Clearinghouse (Rust user-space)
  |  OrderBook: BTreeSet sorted by bid_amount (highest first)
  |  Clears the market: pop winner, deduct tokens, dispatch
  |  Applies demurrage, manages treasuries
  v
Layer 1: The Hook (eBPF/C kernel-space)
  |  Intercepts: enqueue, dispatch, running, stopping, init_task
  |  Passes tasks to Rust via ring buffer
  |  Reads dispatch decisions from Rust via user ring buffer
  |  Reads treasury/market_state from BPF maps (zero-copy)
```

### 3.2 Communication Channels

```
BPF ──[BPF_MAP_TYPE_RINGBUF "queued"]──────────> Rust
      Payload: queued_task_ctx (extended with market fields)

BPF <──[BPF_MAP_TYPE_USER_RINGBUF "dispatched"]── Rust
      Payload: dispatched_task_ctx (extended with market fields)

BPF <──[BPF_MAP_TYPE_HASH "treasury"]──────────── Rust (write-only)
      Per-process token ledger

BPF <──[BPF_MAP_TYPE_ARRAY "market"]───────────── Rust (write-only)
      Per-NUMA market state (clearing price, circuit breaker, demurrage rate)

BPF ──[BPF_MAP_TYPE_PERCPU_ARRAY "csw_counter"]──> Rust (read-only)
      Context-switch rate for circuit breaker

BPF ──[BPF_MAP_TYPE_HASH "transfer_requests"]────> Rust (read-only)
      Parent-to-child token transfer requests
```

---

## 4. Invariants and Development Rules

These rules are **absolute constraints**. Violating any of them is a build-breaking or correctness-breaking error.

### 4.1 Correctness Invariants

1. **`enq_cnt` MUST be preserved.** Every `dispatched_task_ctx` written to the user ring buffer MUST carry the `enq_cnt` from the original `queued_task_ctx`. BPF validates this in `dispatch_task()` at `main.bpf.c:560`. Removing or zeroing this field causes stale dispatches and kernel panics.
2. **`cpu` field MUST be preserved.** Every `dispatched_task_ctx` MUST include a valid `cpu` field (either a specific CPU ID or `RL_CPU_ANY`). BPF uses this to route tasks to per-CPU DSQs or the shared DSQ at `main.bpf.c:529-551`. Omitting this field means BPF doesn't know where to dispatch.
3. **`flags` field MUST be forwarded.** Enqueue flags from `queued_task_ctx.flags` MUST be passed through to `dispatched_task_ctx.flags`. BPF uses these in `scx_bpf_dsq_insert_vtime()` calls.
4. **Token supply is conserved.** `sum(all treasury_entry.balance) + reserve = total_supply` at ALL times. The LLM cannot mint tokens. Demurrage recycles tokens to the reserve. Spending deducts from treasury and returns to reserve.
5. **Struct layout is packed.** All structs crossing the C/Rust boundary use `__attribute__((packed))` in C. Rust bindings are generated by bindgen and use `#[repr(C, packed)]`. Do NOT add padding or reorder fields without updating both sides.

### 4.2 Development Rules

1. **Zero-Syscall Hot Path.** No syscalls in the scheduling loop. All BPF<->Rust communication via eBPF maps and ring buffers. The only acceptable blocking call is `bpf_ringbuf_poll` in the idle tier.
2. **O(log n) Dispatch.** Market clearing uses `BTreeSet` which is O(log n) for insert, pop, and removal. Do NOT switch to `BinaryHeap` — it lacks O(log n) arbitrary removal needed for task cancellation.
3. **Headless Environment.** Target: Ubuntu 24.04 HWE headless. No GUI, no Wayland, no X11. All telemetry via `/proc`, `/sys`, `perf_event_open`, and BPF maps.
4. **Iterative Build.** NEVER implement Phase N+1 until Phase N compiles, loads, and passes its exit criteria. Each phase is a complete, testable scheduler.
5. **Extension, Not Replacement.** Add new fields to existing structs (`queued_task_ctx`, `dispatched_task_ctx`). Do NOT delete existing fields. New structs go in the new `agentic_kernel.h` header. This ensures backward compatibility during development.
6. **Single Source of Truth.** The C header files (`intf.h`, `agentic_kernel.h`) are canonical. Rust structs (`QueuedTask`, `DispatchedTask`) must mirror them exactly. When you add a field to the C struct, add the corresponding field to the Rust struct AND update the `EnqueuedMessage::to_queued_task()` and `BpfScheduler::dispatch_task()` methods.

---

## 5. Market Primitives

### 5.1 Token-to-Timeslice Conversion

```
1 Token = 100 microseconds = 100,000 nanoseconds of guaranteed CPU time

Examples:
  bid_amount = 10   -> slice_ns = 1,000,000    (1ms)
  bid_amount = 50   -> slice_ns = 5,000,000    (5ms)
  bid_amount = 200  -> slice_ns = 20,000,000   (20ms, the current default)
```

The conversion in Rust:

```rust
const NSEC_PER_TOKEN: u64 = 100_000;  // 100 microseconds

fn tokens_to_slice_ns(tokens: u64) -> u64 {
    tokens.saturating_mul(NSEC_PER_TOKEN)
}
```

### 5.2 Token Supply

```
TOKENS_PER_CORE  = 1,000,000
total_supply     = nr_online_cpus * TOKENS_PER_CORE
IPO_BASELINE     = 1,000  (default allocation for new processes)
```

### 5.3 Adaptive Tiered Sleep

The Clearinghouse's main loop uses three tiers based on system load:

| Condition | Strategy | Latency | CPU Cost |
|-----------|----------|---------|----------|
| `nr_running > nr_cpus * 0.8` | Spin-poll ring buffer for 50us | ~1us | High |
| `nr_queued > 0` | `std::thread::yield_now()` then drain | ~10us | Medium |
| `nr_queued == 0 && nr_running == 0` | `bpf_ringbuf_poll` with 100ms timeout | ~ms | Near zero |

---

## 6. Phase 0: Fork and Scaffold

### Goal
Create `scheds/rust/scx_agentic/` as an independent scheduler crate with local BPF sources that we control directly. At the end of this phase, it should behave identically to `scx_rustland`.

### Step 0.1: Create the Directory Structure
```bash
mkdir -p scheds/rust/scx_agentic/src
```

Target structure:
```
scheds/rust/scx_agentic/
  Cargo.toml
  build.rs
  intf.h                    -- COPIED from rust/scx_rustland_core/assets/bpf/intf.h
  agentic_kernel.h          -- NEW, initially just the header guard
  main.bpf.c               -- COPIED from rust/scx_rustland_core/assets/bpf/main.bpf.c
  src/
    main.rs                 -- COPIED from scheds/rust/scx_rustland/src/main.rs
    bpf.rs                  -- COPIED from rust/scx_rustland_core/assets/bpf.rs
    bpf_skel.rs             -- GENERATED by build.rs
    bpf_intf.rs             -- GENERATED by build.rs
    stats.rs                -- COPIED from scheds/rust/scx_rustland/src/stats.rs
```

### Step 0.2: Write Cargo.toml

```toml
[package]
name = "scx_agentic"
version = "0.1.0"
authors = ["Agentic Kernel Project"]
edition = "2021"
description = "Market-based CPU scheduler using sched_ext"
license = "GPL-2.0-only"

[dependencies]
anyhow = "1"
plain = "0.2"
clap = { version = "4", features = ["derive", "env", "unicode", "wrap_help"] }
ctrlc = { version = "3", features = ["termination"] }
libbpf-rs = "=0.26.1"
libc = "0.2"
log = "0.4"
ordered-float = "5"
procfs = "0.18"
serde = { version = "1", features = ["derive"] }
scx_stats = { path = "../../../rust/scx_stats", version = "1.1.0" }
scx_stats_derive = { path = "../../../rust/scx_stats/scx_stats_derive", version = "1.1.0" }
scx_utils = { path = "../../../rust/scx_utils", version = "1.1.0" }
scx_rustland_core = { path = "../../../rust/scx_rustland_core", version = "2.4.11" }
simplelog = "0.12"

[build-dependencies]
scx_cargo = { path = "../../../rust/scx_cargo", version = "1.1.0" }
```
**Key difference from scx_rustland:** We depend on `scx_cargo` directly for build, NOT on `scx_rustland_core` as a build dependency. This lets us compile our own local BPF sources.

### Step 0.3: Write build.rs

```rust
// scheds/rust/scx_agentic/build.rs

fn main() {
    let mut builder = scx_cargo::BpfBuilder::new().unwrap();

    // Compile our local BPF sources (not the ones embedded in scx_rustland_core)
    builder.enable_intf("intf.h", "bpf_intf.rs");
    builder.enable_skel("main.bpf.c", "bpf");

    builder.build().unwrap();
}
```
**IMPORTANT:** The `enable_intf` and `enable_skel` calls look for files relative to the crate root (where Cargo.toml lives). So `intf.h` and `main.bpf.c` must be at `scheds/rust/scx_agentic/intf.h` and `scheds/rust/scx_agentic/main.bpf.c`.

### Step 0.4: Copy and Modify Source Files
1. **Copy `intf.h`** from `rust/scx_rustland_core/assets/bpf/intf.h` verbatim.
2. **Copy `main.bpf.c`** from `rust/scx_rustland_core/assets/bpf/main.bpf.c`. Then perform these renames:
   - `rustland_select_cpu` -> `agentic_select_cpu`
   - `rustland_enqueue` -> `agentic_enqueue`
   - `rustland_dispatch` -> `agentic_dispatch`
   - `rustland_runnable` -> `agentic_runnable`
   - `rustland_running` -> `agentic_running`
   - `rustland_stopping` -> `agentic_stopping`
   - `rustland_enable` -> `agentic_enable`
   - `rustland_init_task` -> `agentic_init_task`
   - `rustland_init` -> `agentic_init`
   - `rustland_exit` -> `agentic_exit`
   - The `SCX_OPS_DEFINE(rustland, ...)` at the bottom becomes `SCX_OPS_DEFINE(agentic, ...)`
   - The `.name = "rustland"` becomes `.name = "agentic"`
3. **Copy `bpf.rs`** from `rust/scx_rustland_core/assets/bpf.rs` to `src/bpf.rs`. Then:
   - Change all occurrences of `rustland` in `scx_ops_open!`, `scx_ops_load!`, `scx_ops_attach!` macros to `agentic`. Specifically:
     - `scx_ops_open!(skel_builder, open_object, rustland, open_opts)` -> `scx_ops_open!(skel_builder, open_object, agentic, open_opts)`
     - `scx_ops_load!(skel, rustland, uei)` -> `scx_ops_load!(skel, agentic, uei)`
     - `scx_ops_attach!(skel, rustland)` -> `scx_ops_attach!(skel, agentic)`
   - Change `skel.struct_ops.rustland_mut()` to `skel.struct_ops.agentic_mut()` (appears 4 times)
   - Remove the import of `scx_rustland_core::ALLOCATOR` and instead use `scx_rustland_core::ALLOCATOR` directly (since we still have `scx_rustland_core` as a runtime dependency for the allocator)
4. **Copy `main.rs`** from `scheds/rust/scx_rustland/src/main.rs`. Then:
   - Change `SCHEDULER_NAME` from `"RustLand"` to `"Agentic"`
   - Change the `bpf::BpfScheduler::init()` call's `name` parameter from `"rustland"` to `"agentic"` (line 185)
5. **Copy `stats.rs`** from `scheds/rust/scx_rustland/src/stats.rs` verbatim.
6. **Create `agentic_kernel.h`** as an empty header:

```c
#ifndef __AGENTIC_KERNEL_H
#define __AGENTIC_KERNEL_H

// Market-based scheduling extensions.
// Structs will be added incrementally in Phase 1+.

#include <linux/types.h>

#endif // __AGENTIC_KERNEL_H
```

### Step 0.5: Update the Module Declarations in main.rs
The `main.rs` file's module declarations need to match the generated file names. Verify these lines exist at the top:

```rust
mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;

#[rustfmt::skip]
mod bpf;
use bpf::*;
```

### Step 0.6: Add to Workspace
If the repository uses a Cargo workspace (check the root `Cargo.toml`), add `scheds/rust/scx_agentic` to the workspace members list. If it uses a `meson.build` system, add the new scheduler there too.

### Validation
```bash
cd scheds/rust/scx_agentic
cargo build 2>&1 | head -50

# If build succeeds, test loading (requires root and sched_ext kernel):
sudo ./target/debug/scx_agentic --verbose &
stress-ng --cpu 4 --timeout 10s
# Verify no crashes, then:
sudo kill %1
```

### Exit Criteria
- [ ] `cargo build` succeeds with no errors
- [ ] `cargo clippy` has no warnings beyond upstream ones
- [ ] Scheduler loads and runs `stress-ng --cpu 4 --timeout 10s` without crashing
- [ ] All dispatch statistics (nr_user_dispatches, nr_kernel_dispatches) are non-zero
- [ ] Behavior is identical to stock `scx_rustland`

---

## 7. Phase 1: Core Market Mechanics (The Order Book)

### Goal
Replace the vruntime-deadline ordering with bid-based ordering. Use static heuristic bids (no agents yet). At the end of this phase, tasks bid for CPU time using tokens from a treasury, and the highest bidder wins.

### Step 1.1: Define Market Types in agentic_kernel.h
Replace the empty header with the full market type definitions:

```c
#ifndef __AGENTIC_KERNEL_H
#define __AGENTIC_KERNEL_H

#include <linux/types.h>

/* === Constants === */
#define MAX_TRACKED_PIDS    131072
#define MAX_NUMA_NODES      8
#define IPO_BASELINE        1000       /* Default tokens for new processes */
#define TOKENS_PER_CORE     1000000    /* Hard cap per CPU core */
#define NSEC_PER_TOKEN      100000     /* 1 token = 100 microseconds */

/* === Process Classification === */
typedef enum {
    CLASS_INTERACTIVE = 0,  /* Games, UI, audio DAC writers */
    CLASS_BATCH       = 1,  /* Compilers, encoders, bulk jobs */
    CLASS_DAEMON      = 2,  /* Databases, servers */
    CLASS_REALTIME    = 3,  /* Explicit RT — bypasses market */
    CLASS_UNKNOWN     = 4,  /* Default: triggers deferred IPO */
} process_class_t;

/*
 * Per-process token ledger.
 * Rust Clearinghouse is sole writer. BPF reads for bid computation.
 * Map type: BPF_MAP_TYPE_HASH, key = pid (s32), max_entries = MAX_TRACKED_PIDS
 */
struct treasury_entry {
    __u64 balance;
    __u64 last_update_ts;    /* Timestamp of last demurrage application */
    __u64 total_spent;       /* Lifetime tokens spent */
    __u8  ipo_complete;      /* 0 = in IPO/ESC tuning, 1 = steady state */
    __u8  process_class;     /* process_class_t cached for fast BPF reads */
    __u8  pad[6];            /* Explicit padding — no implicit ABI holes */
} __attribute__((packed));

/*
 * Per-NUMA market state.
 * Rust writes, BPF reads zero-copy.
 * Map type: BPF_MAP_TYPE_ARRAY, key = numa_node (u32), max_entries = MAX_NUMA_NODES
 */
struct market_state {
    __u64 current_clearing_price;
    __u64 total_tokens_in_circulation;
    __u32 circuit_breaker_tripped;   /* 1 = tripped, 0 = normal */
    __u32 current_demurrage_rate;    /* lambda * 1000 (integer: 1 = 0.001%, 100 = 0.1%) */
} __attribute__((packed));

#endif /* __AGENTIC_KERNEL_H */
```

### Step 1.2: Extend intf.h
Add market fields to the existing structs. **Append to the end of each struct — do NOT reorder existing fields.**

Add to `queued_task_ctx` (after the `comm` field, before the closing `};`):
```c
	u64 bid_amount;          /* Tokens offered this auction tick */
	u64 treasury_balance;    /* Token balance snapshot before bid */
	u64 starvation_ns;       /* Nanoseconds since last CPU dispatch */
	u32 class_hint;          /* process_class_t */
	u32 numa_node;           /* NUMA topology domain */
```

Add to `dispatched_task_ctx` (after the `enq_cnt` field, before the closing `};`):
```c
	u64 clearing_price;      /* Tokens deducted from winner's treasury */
	u64 sequence;            /* Monotonic auction cycle counter */
```

Add the include at the top of `intf.h` (after the existing includes):
```c
#include "agentic_kernel.h"
```

### Step 1.3: Add BPF Maps in main.bpf.c
Add these map definitions after the existing `dispatched` map definition (after line ~160 in the copied file):

```c
/*
 * Per-process token treasury.
 * Rust Clearinghouse is the sole writer; BPF reads for bid computation.
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, __s32);
	__type(value, struct treasury_entry);
	__uint(max_entries, MAX_TRACKED_PIDS);
} treasury SEC(".maps");

/*
 * Per-NUMA market state.
 * Rust writes; BPF reads for circuit breaker checks.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, __u32);
	__type(value, struct market_state);
	__uint(max_entries, MAX_NUMA_NODES);
} market SEC(".maps");
```

### Step 1.4: Add last_dispatch_ts to BPF task_ctx
In the BPF-internal `task_ctx` struct (around line 167 of `main.bpf.c`), add a field to track last dispatch time for starvation calculation:

```c
struct task_ctx {
	u64 start_ts;
	u64 stop_ts;
	u64 exec_runtime;
	u64 enq_cnt;
	u64 last_dispatch_ts;  /* NEW: timestamp of last successful dispatch */
};
```

Update `agentic_running()` to record dispatch time:
```c
// In agentic_running(), after tctx->start_ts = scx_bpf_now();
tctx->last_dispatch_ts = tctx->start_ts;
```

### Step 1.5: Populate Market Fields in get_task_info()
Modify `get_task_info()` in `main.bpf.c` to populate the new fields. Add after the existing `bpf_core_read_str` line:

```c
static void get_task_info(struct queued_task_ctx *task,
			  const struct task_struct *p,
			  struct task_ctx *tctx, u64 enq_flags, s32 prev_cpu)
{
	/* Existing fields — DO NOT MODIFY */
	task->pid = p->pid;
	task->cpu = prev_cpu;
	task->nr_cpus_allowed = p->nr_cpus_allowed;
	task->flags = enq_flags;
	task->start_ts = tctx->start_ts;
	task->stop_ts = tctx->stop_ts;
	task->exec_runtime = tctx->exec_runtime;
	task->weight = p->scx.weight;
	task->vtime = p->scx.dsq_vtime;
	task->enq_cnt = ++tctx->enq_cnt;
	bpf_core_read_str(&task->comm, sizeof(task->comm), &p->comm);

	/* NEW: Market fields */
	u64 now = scx_bpf_now();
	struct treasury_entry *te = bpf_map_lookup_elem(&treasury, &p->pid);
	u64 balance = te ? te->balance : IPO_BASELINE;

	task->bid_amount = balance / 10;  /* Static heuristic: bid 10% of treasury */
	task->treasury_balance = balance;
	task->starvation_ns = tctx->last_dispatch_ts ?
		(now - tctx->last_dispatch_ts) : 0;
	task->class_hint = te ? te->process_class : CLASS_UNKNOWN;

	/* Determine NUMA node */
	s32 numa_node = get_task_numa_node(p);
	task->numa_node = (numa_node >= 0) ? (u32)numa_node : 0;
}
```

### Step 1.6: Update Rust QueuedTask Struct
In `src/bpf.rs`, add the new fields to the `QueuedTask` struct:

```rust
#[derive(Debug, PartialEq, Eq, PartialOrd, Clone)]
pub struct QueuedTask {
    pub pid: i32,
    pub cpu: i32,
    pub nr_cpus_allowed: u64,
    pub flags: u64,
    pub start_ts: u64,
    pub stop_ts: u64,
    pub exec_runtime: u64,
    pub weight: u64,
    pub vtime: u64,
    pub enq_cnt: u64,
    pub comm: [c_char; TASK_COMM_LEN],
    // NEW: Market fields
    pub bid_amount: u64,
    pub treasury_balance: u64,
    pub starvation_ns: u64,
    pub class_hint: u32,
    pub numa_node: u32,
}
```

Update `EnqueuedMessage::to_queued_task()` to populate the new fields:

```rust
fn to_queued_task(&self) -> QueuedTask {
    QueuedTask {
        pid: self.inner.pid,
        cpu: self.inner.cpu,
        nr_cpus_allowed: self.inner.nr_cpus_allowed,
        flags: self.inner.flags,
        start_ts: self.inner.start_ts,
        stop_ts: self.inner.stop_ts,
        exec_runtime: self.inner.exec_runtime,
        weight: self.inner.weight,
        vtime: self.inner.vtime,
        enq_cnt: self.inner.enq_cnt,
        comm: self.inner.comm,
        // NEW: Market fields
        bid_amount: self.inner.bid_amount,
        treasury_balance: self.inner.treasury_balance,
        starvation_ns: self.inner.starvation_ns,
        class_hint: self.inner.class_hint,
        numa_node: self.inner.numa_node,
    }
}
```

### Step 1.7: Update Rust DispatchedTask Struct
In `src/bpf.rs`, add the new fields:

```rust
pub struct DispatchedTask {
    pub pid: i32,
    pub cpu: i32,
    pub flags: u64,
    pub slice_ns: u64,
    pub vtime: u64,
    pub enq_cnt: u64,
    // NEW: Market fields
    pub clearing_price: u64,
    pub sequence: u64,
}
```

Update `DispatchedTask::new()`:

```rust
pub fn new(task: &QueuedTask) -> Self {
    DispatchedTask {
        pid: task.pid,
        cpu: task.cpu,
        flags: task.flags,
        slice_ns: 0,
        vtime: 0,
        enq_cnt: task.enq_cnt,
        clearing_price: 0,
        sequence: 0,
    }
}
```

Update `BpfScheduler::dispatch_task()` to write the new fields:

```rust
pub fn dispatch_task(&mut self, task: &DispatchedTask) -> Result<(), libbpf_rs::Error> {
    let mut urb_sample = self
        .dispatched
        .reserve(std::mem::size_of::<bpf_intf::dispatched_task_ctx>())?;
    let bytes = urb_sample.as_mut();
    let dispatched_task = plain::from_mut_bytes::<bpf_intf::dispatched_task_ctx>(bytes)
        .expect("failed to convert bytes");

    let bpf_intf::dispatched_task_ctx {
        pid,
        cpu,
        flags,
        slice_ns,
        vtime,
        enq_cnt,
        clearing_price,
        sequence,
        ..
    } = dispatched_task;

    *pid = task.pid;
    *cpu = task.cpu;
    *flags = task.flags;
    *slice_ns = task.slice_ns;
    *vtime = task.vtime;
    *enq_cnt = task.enq_cnt;
    *clearing_price = task.clearing_price;
    *sequence = task.sequence;

    self.dispatched
        .submit(urb_sample)
        .expect("failed to submit task");

    Ok(())
}
```

### Step 1.8: Implement the Order Book in main.rs
Replace the existing `Task` struct and `Scheduler` with the market-based versions:

```rust
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime};

// === Market Constants ===
const NSEC_PER_TOKEN: u64 = 100_000; // 1 token = 100us
const IPO_BASELINE: u64 = 1_000;
const TOKENS_PER_CORE: u64 = 1_000_000;

// === Treasury Entry (Rust-side mirror, NOT the BPF struct) ===
#[derive(Debug, Clone)]
struct Treasury {
    balance: u64,
    total_spent: u64,
    process_class: u8,
    ipo_complete: bool,
}

impl Treasury {
    fn new() -> Self {
        Treasury {
            balance: IPO_BASELINE,
            total_spent: 0,
            process_class: 4, // CLASS_UNKNOWN
            ipo_complete: false,
        }
    }
}

// === Task with Bid ===
#[derive(Debug, PartialEq, Eq, Clone)]
struct Task {
    qtask: QueuedTask,
    bid_amount: u64,    // Tokens offered
    enqueue_time: u64,  // Nanosecond timestamp for tie-breaking
}

// Ordering: highest bid first (reverse). On tie: oldest task first. Then by PID for stability.
impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.bid_amount
            .cmp(&self.bid_amount)
            .then_with(|| self.enqueue_time.cmp(&other.enqueue_time))
            .then_with(|| self.qtask.pid.cmp(&other.qtask.pid))
    }
}

impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// === Order Book ===
struct OrderBook {
    book: BTreeSet<Task>,
    sequence: u64,
}

impl OrderBook {
    fn new() -> Self {
        OrderBook {
            book: BTreeSet::new(),
            sequence: 0,
        }
    }

    fn submit_bid(&mut self, task: Task) {
        self.book.insert(task);
    }

    fn clear_market(&mut self) -> Option<(Task, u64)> {
        let winner = self.book.pop_first()?;
        let clearing_price = winner.bid_amount;
        self.sequence += 1;
        Some((winner, clearing_price))
    }

    fn len(&self) -> usize {
        self.book.len()
    }
}
```

### Step 1.9: Rewrite the Scheduler Struct

```rust
struct Scheduler<'a> {
    bpf: BpfScheduler<'a>,
    opts: &'a Opts,
    stats_server: StatsServer<(), Metrics>,
    order_book: OrderBook,
    treasuries: HashMap<i32, Treasury>,  // pid -> treasury
    reserve: u64,                        // Central Bank reserve
    total_supply: u64,                   // Total token supply (invariant)
    init_page_faults: u64,
    slice_ns: u64,
    slice_ns_min: u64,
}
```

### Step 1.10: Rewrite init()

```rust
fn init(opts: &'a Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
    let stats_server = StatsServer::new(stats::server_data()).launch()?;

    let slice_ns = opts.slice_us * NSEC_PER_USEC;
    let slice_ns_min = opts.slice_us_min * NSEC_PER_USEC;

    let bpf = BpfScheduler::init(
        open_object,
        opts.libbpf.clone().into_bpf_open_opts(),
        opts.exit_dump_len,
        opts.partial,
        opts.verbose,
        true,
        opts.numa_local,
        slice_ns_min,
        "agentic",
    )?;

    let nr_cpus = *bpf.nr_online_cpus_mut();  // Note: need &mut self here
    let total_supply = nr_cpus * TOKENS_PER_CORE;

    info!(
        "{} version {} - tokens_per_core={} total_supply={}",
        SCHEDULER_NAME,
        build_id::full_version(env!("CARGO_PKG_VERSION")),
        TOKENS_PER_CORE,
        total_supply,
    );

    Ok(Self {
        bpf,
        opts,
        stats_server,
        order_book: OrderBook::new(),
        treasuries: HashMap::new(),
        reserve: total_supply,  // All tokens start in reserve
        total_supply,
        init_page_faults: 0,
        slice_ns,
        slice_ns_min,
    })
}
```
*Note: The `nr_online_cpus` counter needs to be read. Since `BpfScheduler::init` is already called, `bss_data` is available. You may need to adjust the borrow checker — read `nr_online_cpus` from bss_data after construction if needed.*

### Step 1.11: Rewrite drain_queued_tasks()

```rust
fn drain_queued_tasks(&mut self) {
    loop {
        match self.bpf.dequeue_task() {
            Ok(Some(task)) => {
                let pid = task.pid;
                let bid_amount = task.bid_amount;
                let timestamp = Self::now();

                // Ensure treasury exists for this process
                if !self.treasuries.contains_key(&pid) {
                    // New process: allocate IPO baseline from reserve
                    let ipo = IPO_BASELINE.min(self.reserve);
                    self.reserve -= ipo;
                    self.treasuries.insert(pid, Treasury {
                        balance: ipo,
                        total_spent: 0,
                        process_class: task.class_hint as u8,
                        ipo_complete: false,
                    });
                }

                self.order_book.submit_bid(Task {
                    qtask: task,
                    bid_amount,
                    enqueue_time: timestamp,
                });
            }
            Ok(None) => break,
            Err(err) => {
                warn!("Error: {err}");
                break;
            }
        }
    }
}
```

### Step 1.12: Rewrite dispatch_task()

```rust
fn dispatch_task(&mut self) -> bool {
    let Some((winner, clearing_price)) = self.order_book.clear_market() else {
        return true;
    };

    let mut dispatched = DispatchedTask::new(&winner.qtask);

    // Convert tokens to timeslice, with minimum bound
    let slice_from_bid = clearing_price.saturating_mul(NSEC_PER_TOKEN);
    dispatched.slice_ns = slice_from_bid.max(self.slice_ns_min);

    // Use bid amount as vtime for DSQ ordering (higher bid = lower vtime = earlier dispatch)
    dispatched.vtime = u64::MAX - winner.bid_amount;

    // Market metadata
    dispatched.clearing_price = clearing_price;
    dispatched.sequence = self.order_book.sequence;

    // CPU selection: PRESERVE existing logic exactly
    dispatched.cpu = if self.opts.percpu_local {
        winner.qtask.cpu
    } else {
        match self.bpf.select_cpu(winner.qtask.pid, winner.qtask.cpu, winner.qtask.flags) {
            cpu if cpu >= 0 => cpu,
            _ => RL_CPU_ANY,
        }
    };

    // Deduct tokens from treasury
    if let Some(treasury) = self.treasuries.get_mut(&winner.qtask.pid) {
        let cost = clearing_price.min(treasury.balance);
        treasury.balance -= cost;
        treasury.total_spent += cost;
        self.reserve += cost;  // Tokens return to reserve
    }

    // Send to BPF
    if self.bpf.dispatch_task(&dispatched).is_err() {
        // Failed: refund tokens and re-enqueue
        if let Some(treasury) = self.treasuries.get_mut(&winner.qtask.pid) {
            treasury.balance += clearing_price.min(treasury.total_spent);
            treasury.total_spent -= clearing_price.min(treasury.total_spent);
            self.reserve -= clearing_price.min(self.reserve);
        }
        self.order_book.submit_bid(winner);
        return false;
    }

    true
}
```

### Step 1.13: Rewrite schedule()

```rust
fn schedule(&mut self) {
    self.drain_queued_tasks();
    self.dispatch_task();

    // Notify the dispatcher if there are still pending tasks
    self.bpf.notify_complete(self.order_book.len() as u64);
}
```
*Note: The Adaptive Tiered Sleep is deferred to a later sub-step within Phase 1. For now, the existing `notify_complete` call includes `std::thread::yield_now()` internally (see `bpf.rs:387`), which provides medium-tier behavior.*

### Step 1.14: Treasury Sync to BPF Maps
Add a method to write treasury state back to BPF maps so that `get_task_info()` can read it:

```rust
fn sync_treasuries_to_bpf(&mut self) {
    for (&pid, treasury) in &self.treasuries {
        let entry = bpf_intf::treasury_entry {
            balance: treasury.balance,
            last_update_ts: Self::now(),
            total_spent: treasury.total_spent,
            ipo_complete: if treasury.ipo_complete { 1 } else { 0 },
            process_class: treasury.process_class,
            pad: [0; 6],
        };

        let key = pid.to_ne_bytes();
        let value = unsafe {
            std::slice::from_raw_parts(
                &entry as *const _ as *const u8,
                std::mem::size_of::<bpf_intf::treasury_entry>(),
            )
        };

        let _ = self.bpf.skel.maps.treasury.update(&key, value, libbpf_rs::MapFlags::ANY);
    }
}
```

Call this at the end of `schedule()`:
```rust
fn schedule(&mut self) {
    self.drain_queued_tasks();
    self.dispatch_task();
    self.sync_treasuries_to_bpf();
    self.bpf.notify_complete(self.order_book.len() as u64);
}
```

### Step 1.15: Garbage Collection of Exited Processes
Processes that exit will have stale treasury entries. Add cleanup:

```rust
fn gc_treasuries(&mut self) {
    // Remove treasuries for PIDs that no longer exist
    let dead_pids: Vec<i32> = self.treasuries.keys()
        .filter(|&&pid| {
            // Check if process still exists via /proc
            !std::path::Path::new(&format!("/proc/{}", pid)).exists()
        })
        .copied()
        .collect();

    for pid in dead_pids {
        if let Some(treasury) = self.treasuries.remove(&pid) {
            self.reserve += treasury.balance;  // Return tokens to reserve

            // Also remove from BPF map
            let key = pid.to_ne_bytes();
            let _ = self.bpf.skel.maps.treasury.delete(&key);
        }
    }
}
```
Call this periodically (e.g., every 1000 scheduling cycles) in the `run()` loop.

### Validation
```bash
cargo build

# Run with verbose logging:
sudo ./target/debug/scx_agentic --verbose 2>&1 | tee /tmp/agentic.log &

# Generate load:
stress-ng --cpu 4 --timeout 30s

# Check that tasks are being bid-ordered:
grep "bid_amount" /tmp/agentic.log | head -20

# Check dispatch statistics:
sudo ./target/debug/scx_agentic --stats 5
```

### Exit Criteria
- [ ] Scheduler compiles and loads without errors
- [ ] Tasks are ordered by `bid_amount` (highest first), not by deadline
- [ ] Timeslices reflect `bid_amount * 100us` conversion
- [ ] Token supply invariant holds: `sum(treasuries) + reserve == total_supply`
- [ ] `enq_cnt` validation still prevents stale dispatches
- [ ] New BPF maps (`treasury`, `market`) are created and accessible

---

## 8. Phase 2: Stability and the Circuit Breaker

### Goal
Prevent the market from thrashing when all processes bid maximum or when context-switch rates spike. Implement an automatic fallback to EEVDF-style scheduling with a "haircut protocol" that resets the market.

### Step 2.1: Add Context-Switch Counter BPF Map
In `main.bpf.c`, add after the market map:

```c
/*
 * Per-CPU context-switch counter for circuit breaker monitoring.
 */
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__type(key, __u32);
	__type(value, __u64);
	__uint(max_entries, 1);
} csw_counter SEC(".maps");
```

In `agentic_stopping()`, after the `nr_running` decrement, add:
```c
/* Increment per-CPU context-switch counter */
__u32 csw_key = 0;
__u64 *csw_count = bpf_map_lookup_elem(&csw_counter, &csw_key);
if (csw_count)
	__sync_fetch_and_add(csw_count, 1);
```

### Step 2.2: Add Circuit Breaker Check in BPF Enqueue
In `agentic_enqueue()`, before calling `queue_task_to_userspace()`, add a circuit breaker check:

```c
/*
 * Check circuit breaker status. If tripped, bypass the market
 * and dispatch directly using EEVDF-style vtime ordering.
 */
s32 task_numa = get_task_numa_node(p);
__u32 numa_key = (task_numa >= 0) ? (u32)task_numa : 0;
struct market_state *ms = bpf_map_lookup_elem(&market, &numa_key);
if (ms && ms->circuit_breaker_tripped) {
	scx_bpf_dsq_insert_vtime(p, SHARED_DSQ,
				 slice_ns, p->scx.dsq_vtime, enq_flags);
	__sync_fetch_and_add(&nr_kernel_dispatches, 1);
	goto out_kick;
}
```

### Step 2.3: Circuit Breaker Monitor in Rust

```rust
struct Scheduler<'a> {
    // ... existing fields ...
    csw_baseline: f64,           // Rolling 1-second average context-switch rate
    csw_last_check: Instant,     // Last time we checked csw rate
    csw_last_total: u64,         // Total csw count at last check
    circuit_breaker_active: bool,
    circuit_breaker_until: Option<Instant>,
    schedule_cycle: u64,         // Counter for periodic tasks
}
```

```rust
const CIRCUIT_BREAKER_THRESHOLD: f64 = 5.0;  // 500% of baseline
const CIRCUIT_BREAKER_DURATION_MS: u64 = 100;
const CIRCUIT_BREAKER_CHECK_INTERVAL_MS: u64 = 10;

fn check_circuit_breaker(&mut self) {
    let now = Instant::now();

    // Check if active breaker has expired
    if let Some(until) = self.circuit_breaker_until {
        if now >= until {
            // Breaker window expired — re-open market
            self.circuit_breaker_active = false;
            self.circuit_breaker_until = None;
            self.reset_market_state_in_bpf(false);
            info!("Circuit breaker released, market re-opened");
        }
        return;
    }

    if now.duration_since(self.csw_last_check).as_millis() < CIRCUIT_BREAKER_CHECK_INTERVAL_MS as u128 {
        return;
    }

    let total_csw = self.read_csw_total();
    let elapsed_ms = now.duration_since(self.csw_last_check).as_millis() as f64;
    let rate = (total_csw - self.csw_last_total) as f64 / elapsed_ms;

    if self.csw_baseline == 0.0 {
        self.csw_baseline = rate;
    } else {
        self.csw_baseline = self.csw_baseline * 0.95 + rate * 0.05;
    }

    if self.csw_baseline > 0.0 && rate > self.csw_baseline * CIRCUIT_BREAKER_THRESHOLD {
        warn!("Circuit breaker TRIPPED: csw rate {:.1}/ms vs baseline {:.1}/ms", rate, self.csw_baseline);
        self.trip_circuit_breaker();
    }

    self.csw_last_check = now;
    self.csw_last_total = total_csw;
}
//... (Implement read_csw_total() & trip_circuit_breaker() similarly to Phase 2 guide)
```

### Step 2.4: The Haircut Protocol
```rust
fn haircut_protocol(&mut self) {
    self.order_book.book.clear();
    for treasury in self.treasuries.values_mut() {
        if treasury.balance > IPO_BASELINE {
            let excess = treasury.balance - IPO_BASELINE;
            let slash = (excess as f64 * 0.90) as u64;
            treasury.balance -= slash;
            self.reserve += slash;
        }
    }
}
```

### Exit Criteria
- [ ] Artificial thrashing triggers the circuit breaker
- [ ] System remains responsive during the 100ms EEVDF fallback window
- [ ] Haircut protocol slashes balances

---

## 9. Phase 3: The Monetary Base and Demurrage

### Goal

Implement token economics: hard supply cap, decay (demurrage), fork policy, and rate-limited transfers.

### Step 3.1: Demurrage Implementation

Add to the `Scheduler` struct:

```rust
demurrage_rate: f64,          // Lambda: 0.00001 (0.001%) to 0.001 (0.1%) per ms
last_demurrage: Instant,
```

Add the demurrage method:

```rust
const DEMURRAGE_INTERVAL_MS: u64 = 1;  // Apply every millisecond
const DEMURRAGE_MIN: f64 = 0.00001;    // 0.001% per ms
const DEMURRAGE_MAX: f64 = 0.001;      // 0.1% per ms
const DEMURRAGE_DEFAULT: f64 = 0.0001; // 0.01% per ms

fn apply_demurrage(&mut self) {
    let now = Instant::now();
    let elapsed_ms = now.duration_since(self.last_demurrage).as_millis() as u64;
    if elapsed_ms < DEMURRAGE_INTERVAL_MS {
        return;
    }

    let mut total_tax: u64 = 0;

    for treasury in self.treasuries.values_mut() {
        let tax = (treasury.balance as f64 * self.demurrage_rate * elapsed_ms as f64) as u64;
        if tax > 0 {
            treasury.balance = treasury.balance.saturating_sub(tax);
            total_tax += tax;
        }
    }

    self.reserve += total_tax;
    self.last_demurrage = now;
}
```

Call `self.apply_demurrage()` at the start of every `schedule()` cycle.

### Step 3.2: Fork Policy

In `main.bpf.c`, modify `agentic_init_task()` to record new process creation:

```c
s32 BPF_STRUCT_OPS(agentic_init_task, struct task_struct *p,
		   struct scx_init_task_args *args)
{
	struct task_ctx *tctx;

	tctx = bpf_task_storage_get(&task_ctx_stor, p, 0,
				    BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!tctx)
		return -ENOMEM;

	tctx->last_dispatch_ts = 0;

	return 0;
}
```

On the Rust side, new processes get `IPO_BASELINE` from the reserve when first seen in `drain_queued_tasks()`. This already happens in Step 1.11. Children do NOT inherit parent treasuries — they get fresh IPO allocations.

### Step 3.3: Transfer Mechanism

Add a BPF map for transfer requests in `main.bpf.c`:

```c
struct transfer_request {
	__s32 parent_pid;
	__u64 amount;
} __attribute__((packed));

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, __s32);            /* child pid */
	__type(value, struct transfer_request);
	__uint(max_entries, 4096);
} transfer_requests SEC(".maps");

/* Per-process transfer rate limiter */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, __s32);            /* parent pid */
	__type(value, __u64);          /* transfers in current window */
	__uint(max_entries, MAX_TRACKED_PIDS);
} transfer_rate SEC(".maps");
```

On the Rust side, poll the `transfer_requests` map periodically:

```rust
const MAX_TRANSFERS_PER_SECOND: u64 = 10;

fn process_transfers(&mut self) {
    // Iterate over transfer_requests map
    // For each entry: validate parent has balance, rate limit not exceeded
    // Execute transfer: deduct from parent, add to child
    // Delete the processed entry from the map

    let mut processed: Vec<i32> = Vec::new();

    // Use map iteration via libbpf-rs
    for key in self.bpf.skel.maps.transfer_requests.keys() {
        let child_pid = i32::from_ne_bytes(key[..4].try_into().unwrap_or([0; 4]));

        if let Ok(Some(value)) = self.bpf.skel.maps.transfer_requests.lookup(
            &key, libbpf_rs::MapFlags::ANY
        ) {
            // Parse transfer_request
            if value.len() >= 12 {
                let parent_pid = i32::from_ne_bytes(value[0..4].try_into().unwrap());
                let amount = u64::from_ne_bytes(value[4..12].try_into().unwrap());

                // Validate and execute
                if let Some(parent) = self.treasuries.get_mut(&parent_pid) {
                    if parent.balance >= amount {
                        parent.balance -= amount;
                        if let Some(child) = self.treasuries.get_mut(&child_pid) {
                            child.balance += amount;
                        }
                        // Note: no net change to reserve — it's a peer transfer
                    }
                }
            }
            processed.push(child_pid);
        }
    }

    for pid in processed {
        let _ = self.bpf.skel.maps.transfer_requests.delete(&pid.to_ne_bytes());
    }
}
```

### Step 3.4: Token Supply Audit

Add an assertion that runs periodically:

```rust
fn audit_token_supply(&self) {
    let total_in_treasuries: u64 = self.treasuries.values().map(|t| t.balance).sum();
    let total = total_in_treasuries + self.reserve;

    if total != self.total_supply {
        warn!(
            "TOKEN SUPPLY VIOLATION: treasuries={} + reserve={} = {} (expected {})",
            total_in_treasuries, self.reserve, total, self.total_supply
        );
    }
}
```

### Exit Criteria

- [ ] Demurrage visibly decays idle process balances (log treasury balances over time)
- [ ] Token supply invariant holds under sustained load
- [ ] Fork creates child with baseline tokens from reserve, parent unchanged
- [ ] `sum(all balances) + reserve == total_supply` at all times (audit passes)

---

## 10. Phase 4: Truth and Telemetry (Shadow Core)

### Goal

Isolate one core per NUMA node to run standard EEVDF. Route 5% of tasks there. Compute `delta_IPC = IPC_market - IPC_shadow` to measure whether the market is actually helping.

### Step 4.1: Shadow Core BPF Map

In `main.bpf.c`:

```c
/*
 * Per-CPU shadow core flag.
 * Set to 1 by Rust for shadow cores, 0 for market cores.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, __u32);
	__type(value, __u8);
	__uint(max_entries, 1024);  /* MAX_CPUS */
} shadow_cores SEC(".maps");
```

### Step 4.2: Route 5% of Tasks to Shadow Cores

In `agentic_enqueue()`, after the circuit breaker check but before the market path:

```c
/* Shadow core routing: 5% random sample bypasses market */
__u32 shadow_cpu_key = prev_cpu;
__u8 *is_shadow = bpf_map_lookup_elem(&shadow_cores, &shadow_cpu_key);

if (!is_shadow || !(*is_shadow)) {
	/* Not already on a shadow core — check if we should route there */
	if (bpf_get_prandom_u32() % 20 == 0) {
		/* Find a shadow core for this NUMA node */
		/* Route to shared DSQ with EEVDF vtime ordering */
		scx_bpf_dsq_insert_vtime(p, SHARED_DSQ,
					 slice_ns, p->scx.dsq_vtime, enq_flags);
		__sync_fetch_and_add(&nr_kernel_dispatches, 1);
		goto out_kick;
	}
}
```

### Step 4.3: IPC Measurement in Rust

Use `perf_event_open` to read instructions and cycles per NUMA node:

```rust
use std::fs;

struct IpcSample {
    market_ipc: f64,
    shadow_ipc: f64,
    delta_ipc: f64,
    timestamp: Instant,
}

fn sample_ipc(&self) -> Vec<IpcSample> {
    // Read from /sys/devices/cpu/events/ or use perf_event_open
    // This is a placeholder — actual implementation uses perf syscalls
    // or reads from perf stat running in the background
    Vec::new()
}
```

The full IPC measurement implementation involves `perf_event_open` syscalls which are complex. For Phase 4, start with `/proc/stat`-based CPU utilization as a proxy, then upgrade to hardware PMC counters.

### Step 4.4: Exogenous Event Filter

```rust
fn is_exogenous_event(samples: &[IpcSample]) -> bool {
    // If both market and shadow IPC drop >20% in the same window,
    // it's exogenous (thermal throttle, page fault storm)
    if samples.len() < 2 { return false; }

    let prev = &samples[samples.len() - 2];
    let curr = &samples[samples.len() - 1];

    let market_drop = prev.market_ipc > 0.0 &&
        (prev.market_ipc - curr.market_ipc) / prev.market_ipc > 0.20;
    let shadow_drop = prev.shadow_ipc > 0.0 &&
        (prev.shadow_ipc - curr.shadow_ipc) / prev.shadow_ipc > 0.20;

    market_drop && shadow_drop
}
```

### Exit Criteria

- [ ] Shadow cores are identified and marked in BPF map
- [ ] ~5% of tasks route to shadow cores (verify via dispatch stats)
- [ ] IPC or CPU utilization delta is computed and logged
- [ ] Exogenous events (e.g., `stress-ng --vm 4`) are correctly filtered

---

## 11. Phase 5: The Cold Start (Deferred ESC)

### Goal

Auto-tune bidding agents for each process without disrupting startup.

### Step 5.1: Process Classification

In BPF, check cgroup membership or environment variables at `init_task` time. For simplicity, start with a BPF map that Rust can populate based on process name:

```rust
fn classify_process(&self, task: &QueuedTask) -> u8 {
    let comm = task.comm_str();
    match comm.as_str() {
        "Xwayland" | "firefox" | "chrome" | "gnome-shell" | "pipewire" | "pulseaudio"
            => 0, // CLASS_INTERACTIVE
        "gcc" | "cc1" | "make" | "cargo" | "rustc" | "ffmpeg" | "x264"
            => 1, // CLASS_BATCH
        "postgres" | "mysqld" | "nginx" | "apache2" | "systemd"
            => 2, // CLASS_DAEMON
        _ => 4, // CLASS_UNKNOWN
    }
}
```

### Step 5.2: IPO Baselines by Class

```rust
fn ipo_for_class(class: u8) -> u64 {
    match class {
        0 => IPO_BASELINE * 2,    // CLASS_INTERACTIVE: 2x
        1 => IPO_BASELINE / 2,    // CLASS_BATCH: 0.5x
        2 => IPO_BASELINE,        // CLASS_DAEMON: 1x
        3 => 0,                   // CLASS_REALTIME: bypasses market
        _ => IPO_BASELINE,        // CLASS_UNKNOWN: 1x
    }
}
```

### Step 5.3: ESC Tuning

For `CLASS_UNKNOWN` and `CLASS_DAEMON` processes, after context-switch frequency stabilizes:

```rust
struct EscState {
    phase: f64,           // Current perturbation phase (radians)
    amplitude: f64,       // Perturbation amplitude (fraction of bid)
    frequency: f64,       // Perturbation frequency (Hz)
    latency_samples: Vec<(f64, f64)>,  // (perturbation, latency) pairs
    best_multiplier: f64,
}

const ESC_AMPLITUDE: f64 = 0.05;       // 5% perturbation
const ESC_FREQUENCY: f64 = 10.0;       // 10 Hz
const ESC_SAMPLES_NEEDED: usize = 100; // Samples before convergence

impl EscState {
    fn step(&mut self, current_latency_ns: u64) -> f64 {
        let perturbation = self.amplitude * (self.phase * self.frequency).sin();
        self.phase += 0.01;

        self.latency_samples.push((perturbation, current_latency_ns as f64));

        if self.latency_samples.len() >= ESC_SAMPLES_NEEDED {
            // Find the perturbation that minimized latency
            self.best_multiplier = self.compute_optimal_multiplier();
        }

        1.0 + perturbation  // Return bid multiplier
    }

    fn compute_optimal_multiplier(&self) -> f64 {
        // Simple: find perturbation value with lowest average latency
        // In practice, use gradient estimation via correlation
        let (sum_pert, sum_lat, n) = self.latency_samples.iter()
            .fold((0.0, 0.0, 0.0), |(sp, sl, n), (p, l)| (sp + p, sl + l, n + 1.0));

        let avg_pert = sum_pert / n;
        let avg_lat = sum_lat / n;

        // Gradient estimate
        let gradient: f64 = self.latency_samples.iter()
            .map(|(p, l)| (p - avg_pert) * (l - avg_lat))
            .sum::<f64>() / n;

        // Step in the direction that reduces latency
        (1.0 - gradient.signum() * ESC_AMPLITUDE).clamp(0.5, 2.0)
    }

    fn is_converged(&self) -> bool {
        self.latency_samples.len() >= ESC_SAMPLES_NEEDED
    }
}
```

### Step 5.4: PID Controller Bidding Agent

```rust
struct BiddingAgent {
    target_latency_ns: u64,
    kp: f64,
    ki: f64,
    kd: f64,
    integral: f64,
    prev_error: f64,
    bid_multiplier: f64,  // Set by ESC or class default
}

impl BiddingAgent {
    fn new_for_class(class: u8) -> Self {
        let (target, kp, ki, kd, multiplier) = match class {
            0 => (1_000_000, 0.5, 0.01, 0.1, 0.20), // Interactive: 1ms target, bid 20%
            1 => (50_000_000, 0.1, 0.001, 0.05, 0.05), // Batch: 50ms target, bid 5%
            _ => (5_000_000, 0.3, 0.005, 0.08, 0.10),  // Default: 5ms target, bid 10%
        };

        BiddingAgent {
            target_latency_ns: target,
            kp, ki, kd,
            integral: 0.0,
            prev_error: 0.0,
            bid_multiplier: multiplier,
        }
    }

    fn compute_bid(&mut self, current_latency_ns: u64, treasury_balance: u64) -> u64 {
        let error = self.target_latency_ns as f64 - current_latency_ns as f64;
        self.integral = (self.integral + error).clamp(-1e9, 1e9);
        let derivative = error - self.prev_error;
        self.prev_error = error;

        let adjustment = self.kp * error + self.ki * self.integral + self.kd * derivative;
        let normalized = adjustment / self.target_latency_ns as f64;

        let bid_fraction = (self.bid_multiplier * (1.0 + normalized))
            .clamp(0.01, 0.50);

        (treasury_balance as f64 * bid_fraction) as u64
    }
}
```

### Exit Criteria

- [ ] Process classification assigns correct classes based on comm name
- [ ] ESC tuning converges for test processes (visible in logs)
- [ ] `ipo_complete` transitions to 1 after ESC convergence
- [ ] PID controllers produce responsive bid adjustments under load changes

---

## 12. Phase 6: The Central Bank (LLM Oracle)

### Goal

Connect an async LLM daemon that adjusts macroeconomic policy based on telemetry.

### Step 6.1: Telemetry Snapshot

```rust
#[derive(serde::Serialize)]
struct TelemetrySnapshot {
    timestamp: u64,
    numa_nodes: Vec<NumaSnapshot>,
    top_starved: Vec<StarvedProcess>,
    demurrage_rate: f64,
    reserve_balance: u64,
    total_supply: u64,
    circuit_breaker_trips_last_60s: u32,
}

#[derive(serde::Serialize)]
struct NumaSnapshot {
    id: u32,
    delta_ipc: f64,
    avg_clearing_price: u64,
    tokens_in_circulation: u64,
}

#[derive(serde::Serialize)]
struct StarvedProcess {
    pid: i32,
    comm: String,
    starvation_ms: u64,
    class: String,
    balance: u64,
}
```

### Step 6.2: LLM Policy Decision

```rust
#[derive(serde::Deserialize)]
struct PolicyDecision {
    demurrage_rate: Option<f64>,       // New lambda value
    ipo_baseline: Option<u64>,         // New default IPO
    redistribute: Vec<Redistribution>, // Targeted token transfers
}

#[derive(serde::Deserialize)]
struct Redistribution {
    target_class: String,
    tokens: u64,
}
```

### Step 6.3: Async Daemon Architecture

A separate thread runs every 1-3 seconds:

```rust
struct CentralBank {
    policy: Arc<Mutex<PolicyDecision>>,
    telemetry: Arc<Mutex<TelemetrySnapshot>>,
}

// This thread NEVER touches the scheduling hot path.
// It writes policy decisions to shared state that the
// Clearinghouse reads asynchronously.
fn central_bank_loop(bank: &CentralBank) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let snapshot = bank.telemetry.lock().unwrap().clone();
        let decision = llm_query(&snapshot); // or fallback heuristic
        *bank.policy.lock().unwrap() = decision;
    }
}
```

### Step 6.4: LLM Backend Options

- **Local:** `llama.cpp` with a small model (e.g., Llama 3 8B quantized) for air-gapped environments.
- **API:** Claude API or similar for higher-quality decisions in connected environments.
- **Fallback:** If LLM is unavailable, maintain current demurrage rate and distribute recycled tokens proportionally to starvation_ns.

### Step 6.5: Policy Application

The Clearinghouse reads the latest `PolicyDecision` once per schedule cycle:

```rust
fn apply_policy(&mut self) {
    let decision = self.central_bank_policy.lock().unwrap().clone();

    if let Some(rate) = decision.demurrage_rate {
        self.demurrage_rate = rate.clamp(DEMURRAGE_MIN, DEMURRAGE_MAX);
    }

    if let Some(baseline) = decision.ipo_baseline {
        // Only affects new processes going forward
        self.ipo_baseline = baseline;
    }

    for redist in &decision.redistribute {
        // Route recycled demurrage tokens to specific classes
        for treasury in self.treasuries.values_mut() {
            if self.class_name(treasury.process_class) == redist.target_class {
                let share = redist.tokens / self.count_class(treasury.process_class).max(1);
                let available = share.min(self.reserve);
                treasury.balance += available;
                self.reserve -= available;
            }
        }
    }
}
```

The LLM CANNOT mint new tokens — it can only redirect existing supply.

### Exit Criteria

- [ ] Central Bank daemon runs independently of scheduling loop
- [ ] Demurrage rate adjustments are observable in treasury decay rates
- [ ] Token redistribution reaches starved interactive processes within 3 seconds
- [ ] System degrades gracefully when LLM is unavailable (fallback policy)

---

## Appendix A: Complete Shared Data Contract

### `intf.h` (Final State After All Phases)

```c
#ifndef __INTF_H
#define __INTF_H

#define MAX(x, y) ((x) > (y) ? (x) : (y))
#define MIN(x, y) ((x) < (y) ? (x) : (y))

#define NSEC_PER_SEC	1000000000L
#define CLOCK_BOOTTIME	7

#include <stdbool.h>
#include "agentic_kernel.h"

#ifndef __kptr
#ifdef __KERNEL__
#error "__kptr_ref not defined in the kernel"
#endif
#define __kptr
#endif

#ifndef __VMLINUX_H__
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long u64;
typedef signed char s8;
typedef signed short s16;
typedef signed int s32;
typedef signed long s64;
typedef int pid_t;
#endif

#define MAX_CPUS 1024

#ifndef TASK_COMM_LEN
#define TASK_COMM_LEN	16
#endif

enum { RL_CPU_ANY = 1 << 20, };

/* BPF -> Rust */
struct queued_task_ctx {
	s32 pid;
	s32 cpu;
	u64 nr_cpus_allowed;
	u64 flags;
	u64 start_ts;
	u64 stop_ts;
	u64 exec_runtime;
	u64 weight;
	u64 vtime;
	u64 enq_cnt;
	char comm[TASK_COMM_LEN];
	/* Market extensions (Phase 1) */
	u64 bid_amount;
	u64 treasury_balance;
	u64 starvation_ns;
	u32 class_hint;
	u32 numa_node;
};

/* Rust -> BPF */
struct dispatched_task_ctx {
	s32 pid;
	s32 cpu;
	u64 flags;
	u64 slice_ns;
	u64 vtime;
	u64 enq_cnt;
	/* Market extensions (Phase 1) */
	u64 clearing_price;
	u64 sequence;
};

#endif /* __INTF_H */
```

### `agentic_kernel.h` (Final State)
See Step 1.1 above — it contains `process_class_t`, `treasury_entry`, `market_state`, and all constants.

---

## Appendix B: BPF Map Registry

| Map Name | Type | Key | Value | Size | Writer | Reader | Phase |
|----------|------|-----|-------|------|--------|--------|-------|
| `queued` | RINGBUF | — | `queued_task_ctx` | 4096 entries | BPF | Rust | 0 |
| `dispatched` | USER_RINGBUF | — | `dispatched_task_ctx` | 4096 entries | Rust | BPF | 0 |
| `treasury` | HASH | `s32` (pid) | `treasury_entry` | 131072 | Rust | BPF | 1 |
| `market` | ARRAY | `u32` (numa) | `market_state` | 8 | Rust | BPF | 1 |
| `csw_counter` | PERCPU_ARRAY | `u32` (0) | `u64` | 1 | BPF | Rust | 2 |
| `transfer_requests` | HASH | `s32` (child pid) | `transfer_request` | 4096 | BPF | Rust | 3 |
| `transfer_rate` | HASH | `s32` (parent pid) | `u64` (count) | 131072 | BPF | BPF | 3 |
| `shadow_cores` | ARRAY | `u32` (cpu) | `u8` | 1024 | Rust | BPF | 4 |

---

## Appendix C: Risk Register

| # | Risk | Impact | Likelihood | Mitigation | Phase |
|---|------|--------|------------|------------|-------|
| 1 | Ring buffer overflow under bid storm | Tasks dropped | Medium | Circuit breaker (Phase 2) + increase `MAX_ENQUEUED_TASKS` | 1-2 |
| 2 | `enq_cnt` race after struct extension | Stale dispatches | High | Preserve `enq_cnt` field position and validation unchanged | 1 |
| 3 | Demurrage rate oscillation | Market instability | Medium | Bound lambda changes to 10% per adjustment period; EMA smoothing | 3 |
| 4 | Token supply drift from rounding | Leak/inflation | Medium | Periodic audit assertion (`audit_token_supply`) | 1-3 |
| 5 | ESC perturbations cause latency spikes | User-visible jank | Low | Limit perturbation amplitude to 5% of current bid; abort if latency exceeds 2x target | 5 |
| 6 | LLM latency > 3s | Stale policy decisions | Medium | Fallback to proportional redistribution; never block on LLM | 6 |
| 7 | Shadow core sample bias | Inaccurate delta_IPC | Low | Randomize sample selection per-enqueue, not per-process | 4 |
| 8 | Treasury map exceeds 131072 entries | BPF map full | Low | Garbage-collect exited processes (`gc_treasuries` every 1000 cycles) | 3 |

---

## Appendix D: Validation Playbook

### Quick Smoke Test (After Any Phase)

```bash
# Build
cargo build -p scx_agentic

# Verify binary runs
./target/debug/scx_agentic --version

# Load and run for 30 seconds (requires root and sched_ext kernel)
sudo timeout 30 ./target/debug/scx_agentic --verbose 2>&1 | tee /tmp/smoke.log
```

### Load Test (Phase 1+)

```bash
# Start scheduler
sudo ./target/debug/scx_agentic --verbose &

# CPU stress test
stress-ng --cpu 4 --timeout 30s

# Verify dispatch stats are non-zero
grep -i "dispatch" /tmp/smoke.log

# Clean up
sudo kill %1
```

### Circuit Breaker Test (Phase 2+)

```bash
# Artificial thrashing: many short-lived processes
for i in $(seq 1 100); do stress-ng --cpu 1 --timeout 30s & done

# Monitor breaker trips
sudo ./target/debug/scx_agentic --verbose 2>&1 | grep -i "circuit"

# Verify system stays responsive during breaker window
time ls /tmp  # Should complete quickly even during breaker
```

### Token Conservation Test (Phase 3+)

```bash
# Run under load and check for supply violations
sudo ./target/debug/scx_agentic --verbose 2>&1 | grep -i "TOKEN SUPPLY"

# If no violations appear, the invariant holds
```

### Shadow Core Test (Phase 4+)

```bash
# Run and verify shadow core telemetry
sudo ./target/debug/scx_agentic --verbose 2>&1 | grep -i "shadow\|exogenous\|ipc"

# Exogenous event test: memory pressure should trigger simultaneous drop
stress-ng --vm 4 --timeout 20s
```
