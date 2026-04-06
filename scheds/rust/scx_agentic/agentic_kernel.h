#ifndef __AGENTIC_KERNEL_H
#define __AGENTIC_KERNEL_H

/*
 * Market-based scheduling extensions for the Agentic Kernel.
 *
 * NOTE: This header is included from intf.h AFTER the vmlinux-compatible
 * type definitions (u8, u16, u32, u64, s32, etc.). Do NOT include
 * <linux/types.h> here — it conflicts with vmlinux.h in BPF programs.
 * Use the plain C types already defined by intf.h instead.
 */

/* === Constants === */
#define MAX_TRACKED_PIDS    131072
#define MAX_NUMA_NODES      8
#define IPO_BASELINE        1000       /* Default tokens for new processes */
#define TOKENS_PER_CORE     1000000    /* Hard cap per CPU core */
#define NSEC_PER_TOKEN      100000     /* 1 token = 100 microseconds */

/* === Process Classification === */
#define CLASS_INTERACTIVE   0  /* Games, UI, audio DAC writers */
#define CLASS_BATCH         1  /* Compilers, encoders, bulk jobs */
#define CLASS_DAEMON        2  /* Databases, servers */
#define CLASS_REALTIME      3  /* Explicit RT — bypasses market */
#define CLASS_UNKNOWN       4  /* Default: triggers deferred IPO */

/*
 * Per-process token ledger.
 * Rust Clearinghouse is sole writer. BPF reads for bid computation.
 * Map type: BPF_MAP_TYPE_HASH, key = pid (s32), max_entries = MAX_TRACKED_PIDS
 */
struct treasury_entry {
    u64 balance;
    u64 last_update_ts;    /* Timestamp of last demurrage application */
    u64 total_spent;       /* Lifetime tokens spent */
    u8  ipo_complete;      /* 0 = in IPO/ESC tuning, 1 = steady state */
    u8  process_class;     /* CLASS_* constant */
    u8  pad[6];            /* Explicit padding — no implicit ABI holes */
} __attribute__((packed));

/*
 * Per-NUMA market state.
 * Rust writes, BPF reads zero-copy.
 * Map type: BPF_MAP_TYPE_ARRAY, key = numa_node (u32), max_entries = MAX_NUMA_NODES
 */
struct market_state {
    u64 current_clearing_price;
    u64 total_tokens_in_circulation;
    u32 circuit_breaker_tripped;   /* 1 = tripped, 0 = normal */
    u32 current_demurrage_rate;    /* lambda * 1000 (integer: 1 = 0.001%, 100 = 0.1%) */
} __attribute__((packed));

/*
 * Transfer request: parent donates tokens to child.
 * Written by userspace (or BPF on fork), consumed by Rust Clearinghouse.
 * Map type: BPF_MAP_TYPE_HASH, key = child pid (s32), max_entries = 4096
 */
struct transfer_request {
    s32 parent_pid;
    u64 amount;
} __attribute__((packed));

#endif /* __AGENTIC_KERNEL_H */
