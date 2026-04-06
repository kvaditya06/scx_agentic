# The Agentic Kernel: End-to-End Architectural Manifesto

## 1. The Paradigm Shift
We are replacing the Linux kernel's default "Completely Fair Scheduler" (EEVDF/CFS) with a High-Frequency Financial Market. Modern OS schedulers guess what is important based on static rules. Our system explicitly funds user intent. We are moving from a "communist breadline" (waiting in a FIFO queue) to a "capitalist market" (processes bidding for CPU cycles based on micro-economies).

## 2. The Four-Layer Architecture
We are modifying the `scx_rustland` scheduler (v1.1.0) using the `sched_ext` eBPF framework.
1. **The Hook (eBPF / C):** Intercepts CPU scheduling events and passes them to user space via ring buffers.
2. **The Clearinghouse (Rust):** An ultra-fast Order Book (Max-Heap) that ingests bids and grants CPU time in microseconds.
3. **The Bidding Agents (User Space):** Algorithmic controllers (PID/ESC) attached to processes that spend allocated budgets to buy CPU time.
4. **The Central Bank (LLM):** An asynchronous AI oracle that monitors system telemetry and acts as the macroeconomic policy engine, distributing tokens.

## 3. Strict Development Rules (AI Directives)
* **Zero-Syscall Observability:** State shared between the kernel, the Bidding Agents, and the Clearinghouse MUST happen via eBPF maps. Syscalls in the hot path equal death.
* **O(1) Dispatch:** Market clearing must be O(1) or O(log n).
* **Headless Environment:** Assume Ubuntu 24.04 HWE headless. No GUI or Wayland code.
* **Iterative Build:** NEVER implement a future phase until the current phase compiles and proves stable.

## 4. Market Primitives
Before any logic is implemented, the system must adhere to these physical definitions:

* **Token-to-Timeslice Semantics:** 1 Token = 100 microseconds (`100us`) of guaranteed CPU time. If a process wins the auction with a 50-token bid, it is granted a 5ms timeslice before it is preempted and forced to bid again.
* **Wakeup Strategy (Adaptive Tiered Sleep):** The Rust Clearinghouse does not use naive blocking or 100% CPU spin-polling. It uses an adaptive sleep tier:
  * *High Load:* Spin-poll the BPF ring buffer for exactly 50 microseconds.
  * *Medium Load:* Yield the thread (`std::thread::yield_now()`).
  * *Idle:* Park the thread and rely on `epoll` or the BPF `bpf_ringbuf_poll` block to wake the Clearinghouse only when a new task arrives.

## 5. The Engineering Roadmap

### Phase 1: Core Market Mechanics (The Order Book)
**Goal:** Prove the Rust matching engine can ingest tasks and clear the market deterministically.
* **Data Structure:** Implement a `BinaryHeap` Max-Heap in `src/main.rs`.
* **The Bid:** Integrate the `task_bid` struct (see Appendix A). Tie-breaker: oldest task wins on equal bids.
* **Integration:** Replace the FIFO dispatch loop with `OrderBook.submit_bid()` and `OrderBook.clear_market()`. Use static heuristic bids for this phase. Implement the Adaptive Tiered Sleep wakeup strategy.
* **Dispatch:** Write auction results into `dispatch_slot` (see Appendix A). BPF must validate the `sequence` counter on every read to reject stale slots.

### Phase 2: Stability & The Circuit Breaker
**Goal:** Prevent the market from thrashing or deadlocking during a bidding war.
* **BPF Fallback:** Monitor context-switch rates in the BPF hook. If they spike 500% above baseline, trip the circuit breaker.
* **The Haircut Protocol:** When the breaker trips, bypass the Rust Order Book and fall back to native EEVDF for 100ms. During this window, the Clearinghouse cancels all pending bids and applies a universal 90% treasury slash to any process holding above the IPO baseline. The market re-opens with cold-start balances, forcing processes to re-establish resource starvation organically.

### Phase 3: The Monetary Base & Demurrage
**Goal:** Prevent runaway inflation and process hoarding.
* **Hard Cap:** Exactly 1,000,000 tokens in circulation per CPU core at all times. The LLM cannot mint; it can only redirect flow.
* **Demurrage (Wealth Tax):** Implement a decay scalar (λ). Every millisecond, unspent tokens decay by λ percent and return to the Central Bank reserve. λ is bounded between `0.001%` and `0.1%` per millisecond.
* **Fork Policy:** Intercept `sched_process_fork`. Child processes DO NOT inherit parent treasuries. Every forked child receives the strict IPO baseline. Explicit parent-to-child transfers are permitted via IPC call to the Clearinghouse, reducing the parent's own balance.
* **BPF Rate Limiting:** A secondary eBPF program attached to the IPC syscall tracepoint enforces a per-process transfer rate cap. Excess calls are dropped with `-EAGAIN` before the Rust daemon is ever woken.

### Phase 4: Truth & Telemetry (Shadow Core)
**Goal:** Establish a causal feedback loop so the Central Bank knows if its policies actually work.
* **NUMA-Aware Isolation:** Permanently isolate one core per NUMA node to run standard EEVDF. Route a random 5% sample of total workload through these shadow cores.
* **Delta IPC:** Calculate `ΔIPC = IPC_market - IPC_EEVDF` per NUMA node independently. If both the market IPC and shadow IPC crater simultaneously, the Central Bank classifies the event as exogenous (thermal throttle, page fault storm) and does not update policy. It only adjusts when ΔIPC turns sharply negative in isolation.

### Phase 5: The Cold Start (Deferred ESC)
**Goal:** Auto-tune the Bidding Agents without ruining application startup times.
* **cgroup Hinting:** Use `cgroups` or a lightweight `LD_PRELOAD` library to declare process class at spawn (`CLASS_INTERACTIVE`, `CLASS_BATCH`, `CLASS_DAEMON`, `CLASS_REALTIME`).
* **Deferred Tuning:** Processes spawning without a hint receive a Deferred IPO. The system waits until the process's context-switch frequency stabilizes (indicating exit from chaotic initialization), then executes Extremum Seeking Control (ESC): small, controlled CPU allocation perturbations are injected and the latency response curve is mapped to auto-tune that process's PID constants for its exact binary on its exact NUMA node.
* **IPO Complete Flag:** Once tuning finishes, the `ipo_complete` flag in `treasury_entry` is set to `1`. The ESC tuning sequence will never re-run for that process instance.

### Phase 6: The Central Bank (LLM Oracle)
**Goal:** Connect physical hardware intent to the macroeconomic token supply.
* **The Faucet:** A lightweight local LLM (or API) daemon runs every 1-3 seconds asynchronously. It never touches the scheduling hot path.
* **Headless Telemetry Ingestion:** The LLM receives:
  * `ΔIPC` metrics per NUMA node (from Phase 4)
  * `/proc/<pid>/wchan` wait channel states to detect blocked or waiting interactive threads
  * `perf` buffer write detection for active audio DAC streams and network sockets
* **Distribution:** The LLM routes recycled demurrage tokens to active interactive/urgent threads as "Premium Latency" tokens, leaving batch jobs with "Bulk Throughput" tokens.
* **Macro Control (Liquidity):** The LLM has write access to the demurrage rate (λ), bounded between `0.001%` and `0.1%` per millisecond. Raising λ forces hoarded tokens back into reserve and injects liquidity to starved threads. Lowering λ stabilises a volatile market.

---

## Appendix A: Shared Data Contract (`agentic_kernel.h`)
This schema is the canonical memory boundary between the eBPF kernel hook and the user-space Rust Clearinghouse. Do not modify any struct in Rust without updating the corresponding C header, and vice versa. All structs are explicitly packed to prevent ABI padding corruption across the C/Rust boundary.

```c
#ifndef __AGENTIC_KERNEL_H
#define __AGENTIC_KERNEL_H

#include <linux/types.h>

#define MAX_TRACKED_PIDS    131072
#define MAX_NUMA_NODES      8
#define DISPATCH_MAP_MAX    256

// Process classification declared at spawn via cgroup hint or LD_PRELOAD.
// class_hint fields in all structs below MUST use these constants.
typedef enum {
    CLASS_INTERACTIVE = 0,  // Games, UI, audio DAC writers
    CLASS_BATCH       = 1,  // Compilers, encoders, bulk jobs
    CLASS_DAEMON      = 2,  // Databases, servers (deferred IPO)
    CLASS_REALTIME    = 3,  // Explicit RT — bypasses market entirely
    CLASS_UNKNOWN     = 4,  // Default: triggers deferred IPO
} process_class_t;

// 1. Ring Buffer payload submitted by the kernel hook on task enqueue.
//    Written by BPF, consumed by the Rust Clearinghouse.
struct task_bid {
    __s32 pid;
    __s32 tgid;
    __u64 bid_amount;       // Tokens offered this auction tick
    __u64 enqueue_time;     // ktime_get_ns() — nanosecond tie-breaker
    __u64 treasury_balance; // Token balance snapshot before bid
    __u64 starvation_ns;    // Nanoseconds since last CPU dispatch
    __u32 class_hint;       // process_class_t
    __u32 numa_node;        // NUMA topology domain
} __attribute__((packed));

// 2. Auction result written by Rust into the per-CPU dispatch map.
//    BPF scheduler reads this to make the dispatch decision.
//    CRITICAL: BPF must check sequence on every read. Reject if unchanged.
struct dispatch_slot {
    __s32 pid;
    __u32 timeslice_us;     // 1 token = 100us. bid_amount * 100 = timeslice_us.
    __u64 clearing_price;   // Tokens deducted from winner's treasury
    __u64 sequence;         // Monotonic counter — incremented by Rust each cycle
} __attribute__((packed));

// 3. Per-process token ledger. Rust Clearinghouse is sole writer. BPF is read-only.
struct treasury_entry {
    __u64 balance;
    __u64 last_update_ts;   // Timestamp of last demurrage application
    __u64 total_spent;
    __u8  ipo_complete;     // 0 = in IPO/ESC tuning window, 1 = steady state
    __u8  process_class;    // process_class_t cached for fast BPF reads
    __u8  pad[6];           // Explicit padding — no implicit ABI holes
} __attribute__((packed));

// 4. NUMA-local market state. Rust writes, BPF reads zero-copy.
//    Indexed by numa_node (0..MAX_NUMA_NODES).
struct market_state {
    __u64 current_clearing_price;
    __u64 total_tokens_in_circulation;
    __u32 circuit_breaker_tripped;  // 1 = tripped, 0 = normal
    __u32 current_demurrage_rate;   // Scaled lambda (0.001% to 0.1% per ms)
} __attribute__((packed));

#endif // __AGENTIC_KERNEL_H
```