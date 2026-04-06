# Agentic Kernel - Implementation Checkpoint

**Date:** 2026-04-06
**Status:** ALL PHASES COMPLETE (0-6). Live tested. LLM Central Bank verified end-to-end.

---

## What's Been Done

### Phase 0: Fork and Scaffold (COMPLETE)
- Created `scheds/rust/scx_agentic/` as independent scheduler crate
- Copied and forked all source files from `scx_rustland` and `scx_rustland_core/assets/`
- Renamed all BPF hooks from `rustland_*` to `agentic_*` (in `main.bpf.c`)
- Renamed all Rust-side `scx_ops_open!/load!/attach!` macro calls from `rustland` to `agentic` (in `src/bpf.rs`)
- Renamed `skel.struct_ops.rustland_mut()` to `skel.struct_ops.agentic_mut()` (4 occurrences in `src/bpf.rs`)
- Changed scheduler name to `"Agentic"` and BPF init name to `"agentic"` (in `src/main.rs`)
- Wrote custom `build.rs` using `scx_cargo::BpfBuilder` directly (bypasses `scx_rustland_core` asset embedding)
- Created `src/bpf_skel.rs` and `src/bpf_intf.rs` as thin `include!()` wrappers from `OUT_DIR` with warning suppression
- Added `scheds/rust/scx_agentic` to workspace `Cargo.toml`
- **Build verified: compiles clean, binary runs, reports version correctly**

### Phase 1: Core Market Mechanics (COMPLETE)
All market primitives implemented: treasury, order book, bid-based dispatch. Build verified.

#### Files modified:

**`agentic_kernel.h`** — Full market type definitions:
- Constants: `MAX_TRACKED_PIDS`, `MAX_NUMA_NODES`, `IPO_BASELINE`, `TOKENS_PER_CORE`, `NSEC_PER_TOKEN`
- Process class defines: `CLASS_INTERACTIVE` through `CLASS_UNKNOWN` (as `#define`, not enum, to avoid BPF issues)
- `struct treasury_entry` — per-process token ledger (balance, total_spent, process_class, ipo_complete)
- `struct market_state` — per-NUMA market state (clearing_price, tokens_in_circulation, circuit_breaker, demurrage_rate)
- Uses plain C types (`u8`, `u64`, etc.) defined by `intf.h`, NOT `<linux/types.h>` (which conflicts with vmlinux.h)

**`intf.h`** — Extended existing structs:
- `#include "agentic_kernel.h"` placed AFTER the `#ifndef __VMLINUX_H__` typedef block (critical ordering)
- `queued_task_ctx` extended with: `bid_amount`, `treasury_balance`, `starvation_ns`, `class_hint`, `numa_node`
- `dispatched_task_ctx` extended with: `clearing_price`, `sequence`
- All existing fields preserved in original order

**`main.bpf.c`** — BPF-side changes:
- Added `treasury` BPF_MAP_TYPE_HASH map (key=s32 pid, value=treasury_entry)
- Added `market` BPF_MAP_TYPE_ARRAY map (key=u32 numa_node, value=market_state)
- Added `last_dispatch_ts` field to `struct task_ctx`
- `get_task_info()` now populates market fields (bid_amount = balance/10, starvation_ns, class_hint, numa_node)
- `agentic_running()` now records `last_dispatch_ts`

**`src/bpf.rs`** — Rust BPF connector:
- `QueuedTask` struct extended with: `bid_amount`, `treasury_balance`, `starvation_ns`, `class_hint`, `numa_node`
- `DispatchedTask` struct extended with: `clearing_price`, `sequence`
- `DispatchedTask::new()` initializes new fields to 0
- `EnqueuedMessage::to_queued_task()` maps all new fields
- `BpfScheduler::dispatch_task()` writes `clearing_price` and `sequence` to user ring buffer

**`src/main.rs`** — Complete rewrite with market logic:
- `Treasury` struct for Rust-side bookkeeping (balance, total_spent, process_class)
- `Task` struct with bid-based ordering (highest bid first, oldest wins tie)
- `OrderBook` struct wrapping `BTreeSet<Task>` with `submit_bid()` and `clear_market()`
- `Scheduler` struct with: order_book, treasuries HashMap, reserve, total_supply
- `drain_queued_tasks()` — creates IPO treasury for new processes from reserve
- `dispatch_task()` — clears market, converts tokens to timeslice (NSEC_PER_TOKEN), deducts from treasury, refunds on failure
- `sync_treasuries_to_bpf()` — writes Rust treasury state to BPF map every 100 cycles
- `gc_treasuries()` — removes dead process treasuries every 1000 cycles, returns tokens to reserve
- Uses `libbpf_rs::MapCore` trait for map operations
- CPU selection logic preserved exactly from original

### Phase 2: Circuit Breaker (COMPLETE)

#### Files modified:

**`main.bpf.c`** — BPF-side circuit breaker:
- Added `csw_counter` PERCPU_ARRAY map (key=u32, value=u64, max_entries=1)
- `agentic_stopping()` increments per-CPU context-switch counter
- `agentic_enqueue()` checks `market_state.circuit_breaker_tripped` — if set, bypasses userspace and dispatches via `scx_bpf_dsq_insert_vtime()` to SHARED_DSQ

**`src/main.rs`** — Rust-side circuit breaker:
- Added `Instant` import, circuit breaker constants (5x threshold, 100ms duration, 10ms check interval)
- `Scheduler` struct extended with: `csw_baseline`, `csw_last_check`, `csw_last_total`, `circuit_breaker_active`, `circuit_breaker_until`
- `read_csw_total()` — reads PERCPU_ARRAY and sums per-CPU values via `lookup_percpu()`
- `reset_market_state_in_bpf(tripped)` — writes circuit_breaker_tripped flag to all 8 NUMA nodes
- `haircut_protocol()` — drains order book, slashes 90% of excess balances above IPO_BASELINE
- `trip_circuit_breaker()` — sets BPF flag, starts 100ms timer, executes haircut
- `check_circuit_breaker()` — EMA-based CSW rate monitoring, trips on 500% spike, auto-releases after 100ms
- `schedule()` — calls `check_circuit_breaker()` every cycle, drains ring buffer without processing during active breaker

---

### Phase 3: Monetary Base & Demurrage (COMPLETE)

#### Files modified:

**`agentic_kernel.h`** — Added transfer_request struct:
- `struct transfer_request` with `parent_pid` (s32) and `amount` (u64), packed

**`main.bpf.c`** — New BPF maps:
- `transfer_requests` BPF_MAP_TYPE_HASH (key=s32 child_pid, value=transfer_request, max=4096)
- `transfer_rate` BPF_MAP_TYPE_HASH (key=s32 parent_pid, value=u64, max=MAX_TRACKED_PIDS)

**`src/main.rs`** — Token economics:
- Demurrage constants: DEMURRAGE_INTERVAL_MS=1, DEMURRAGE_DEFAULT=0.0001 (0.01%/ms)
- `Scheduler` extended with: `demurrage_rate`, `last_demurrage`
- `apply_demurrage()` — decays all balances by rate*elapsed_ms, returns tax to reserve
- `process_transfers()` — polls transfer_requests BPF map, validates parent balance, executes peer transfers
- `audit_token_supply()` — verifies sum(balances) + reserve == total_supply, warns on violation
- `schedule()` — calls apply_demurrage() every cycle, process_transfers() every 100 cycles, audit every 1000 cycles

---

## What Remains for Later Phases
- Demurrage (wealth tax) — periodic balance decay
- Fork policy — children get IPO from reserve, not parent
- Transfer mechanism — BPF map for parent->child token transfers
- Rate limiting

### Phase 4: Shadow Core Telemetry (COMPLETE)

#### Files modified:

**`main.bpf.c`** — Shadow core BPF infrastructure:
- Added `shadow_cores` BPF_MAP_TYPE_ARRAY map (key=u32 cpu_id, value=u8 flag, max=1024)
- `agentic_enqueue()` checks shadow_cores map — tasks on shadow cores dispatch via EEVDF
- 5% random sampling routes non-shadow tasks to SHARED_DSQ for control group

**`src/main.rs`** — Shadow core telemetry:
- Added `IpcSample` struct (market_util, shadow_util, delta, timestamp)
- Added `CpuTimes` struct for /proc/stat parsing
- `Scheduler` extended with: shadow_cpus, market_cpus, ipc_history (VecDeque), prev_cpu_times
- `init()` designates last CPU as shadow core, writes flags to BPF map
- `read_cpu_times()` — parses /proc/stat for per-CPU busy/total jiffies
- `compute_util()` — computes utilization delta for a set of CPUs
- `sample_ipc()` — periodic (100ms) sampling of market vs shadow utilization
- `is_exogenous_event()` — detects simultaneous >20% drop on both market and shadow
- `schedule()` — calls sample_ipc() every cycle (self-throttled), logs exogenous events
### Phase 5: Cold Start / ESC Tuning (COMPLETE)

#### Files modified:

**`src/main.rs`** — Process classification and bidding agents:
- Constants: CLASS_INTERACTIVE through CLASS_UNKNOWN, ESC_AMPLITUDE, ESC_FREQUENCY, ESC_SAMPLES_NEEDED
- `EscState` struct — ESC tuning with sinusoidal perturbation, gradient-based optimal multiplier computation
- `BiddingAgent` struct — PID controller with class-specific gains (kp/ki/kd), integrated ESC tuning
- `BiddingAgent::new_for_class()` — class-specific target latency, PID gains, bid multiplier
- `BiddingAgent::compute_bid()` — ESC phase during tuning, then steady-state PID control
- `classify_process()` — comm-name based classification (interactive/batch/daemon/unknown)
- `ipo_for_class()` — class-based IPO: 2x interactive, 0.5x batch, 1x daemon/unknown
- `drain_queued_tasks()` — creates agent alongside treasury, uses agent for bid computation
- `gc_treasuries()` — also cleans up agents for dead processes
- `sync_treasuries_to_bpf()` — sets ipo_complete=1 when ESC converges
- `haircut_protocol()` — uses class-aware baseline, resets agent integral on trip

### Phase 6: LLM Central Bank (COMPLETE)

#### Files modified:

**`Cargo.toml`** — Added `serde_json = "1"` dependency

**`src/main.rs`** — Central Bank infrastructure:
- `TelemetrySnapshot`, `NumaSnapshot`, `StarvedProcess` structs (serde::Serialize)
- `PolicyDecision`, `Redistribution` structs (serde::Deserialize)
- `CentralBank` struct with Arc<Mutex<>> shared state for policy and telemetry
- `CentralBank::spawn()` — background thread running every 2 seconds
- `CentralBank::fallback_heuristic()` — adjusts demurrage based on reserve ratio, redistributes to starved interactive processes
- `class_name()`, `count_class()` — helpers for policy application
- `build_telemetry_snapshot()` — assembles snapshot from scheduler state
- `apply_policy()` — reads latest PolicyDecision, clamps demurrage, distributes tokens by class
- `run()` — spawns Central Bank thread on startup
- `schedule()` — publishes telemetry and applies policy every 500 cycles

---

## Key Files

| File | Path | Status |
|------|------|--------|
| Plan | `instructions/agentic_os_kernel.md` | Complete |
| Original vision | `instructions/agentic_kernel.md` | Reference |
| Revised plan | `instructions/agentic_kernel_revised_plan.md` | Reference |
| This checkpoint | `instructions/checkpoint.md` | Current |
| Cargo.toml | `scheds/rust/scx_agentic/Cargo.toml` | Done |
| build.rs | `scheds/rust/scx_agentic/build.rs` | Done |
| agentic_kernel.h | `scheds/rust/scx_agentic/agentic_kernel.h` | Done |
| intf.h | `scheds/rust/scx_agentic/intf.h` | Done |
| main.bpf.c | `scheds/rust/scx_agentic/main.bpf.c` | Done |
| src/bpf.rs | `scheds/rust/scx_agentic/src/bpf.rs` | Done |
| src/main.rs | `scheds/rust/scx_agentic/src/main.rs` | Done |
| src/bpf_skel.rs | `scheds/rust/scx_agentic/src/bpf_skel.rs` | Done |
| src/bpf_intf.rs | `scheds/rust/scx_agentic/src/bpf_intf.rs` | Done |
| src/stats.rs | `scheds/rust/scx_agentic/src/stats.rs` | Copied unchanged |
| Workspace | `Cargo.toml` (root) | Updated |

## Lessons Learned
1. `agentic_kernel.h` MUST be included AFTER vmlinux-compatible typedefs in `intf.h` — `<linux/types.h>` conflicts with vmlinux.h in BPF programs
2. Use `#define` not `typedef enum` for process class constants in BPF headers
3. `scx_cargo::BpfBuilder` generates `bpf_skel.rs` and `bpf_intf.rs` in `OUT_DIR` — need thin wrapper files in `src/` with `include!(concat!(env!("OUT_DIR"), ...))`
4. The skeleton generator names types after the BPF ops struct name — renaming `rustland` to `agentic` changes all generated Rust type names
5. `libbpf_rs::MapCore` trait must be imported to call `.update()` and `.delete()` on BPF maps
6. BPF `PERCPU_ARRAY` maps use `lookup_percpu()` in Rust, returning `Vec<Vec<u8>>` — one entry per CPU
7. BPF variable declarations must be at the top of the block (C89 style) — use braces `{}` to create a new scope if needed mid-function
