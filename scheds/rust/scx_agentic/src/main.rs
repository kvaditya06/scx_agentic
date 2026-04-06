// scx_agentic: Market-based CPU scheduler
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.
mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;

#[rustfmt::skip]
mod bpf;
use bpf::*;

mod stats;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::io::BufRead;
use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use clap::Parser;
use libbpf_rs::MapCore;
use libbpf_rs::OpenObject;
use log::info;
use log::warn;
use procfs::process::Process;
use scx_stats::prelude::*;
use scx_utils::build_id;
use scx_utils::libbpf_clap_opts::LibbpfOpts;
use scx_utils::UserExitInfo;
use stats::Metrics;

const SCHEDULER_NAME: &str = "Agentic";

// Market constants
const NSEC_PER_TOKEN: u64 = 100_000; // 1 token = 100 microseconds
const IPO_BASELINE: u64 = 1_000;     // Default tokens for new processes
const TOKENS_PER_CORE: u64 = 1_000_000;

/// scx_agentic: market-based CPU scheduler
///
/// Processes bid for CPU time using tokens from a finite monetary supply.
/// Highest bidder wins the auction each scheduling cycle.
#[derive(Debug, Parser)]
struct Opts {
    /// Scheduling slice duration in microseconds.
    #[clap(short = 's', long, default_value = "20000")]
    slice_us: u64,

    /// Scheduling minimum slice duration in microseconds.
    #[clap(short = 'S', long, default_value = "1000")]
    slice_us_min: u64,

    /// If set, per-CPU tasks are dispatched directly to their only eligible CPU.
    #[clap(short = 'l', long, action = clap::ArgAction::SetTrue)]
    percpu_local: bool,

    /// Enable NUMA-local idle CPU selection.
    #[clap(short = 'n', long, action = clap::ArgAction::SetTrue)]
    numa_local: bool,

    /// If specified, only tasks with SCHED_EXT policy are switched.
    #[clap(short = 'p', long, action = clap::ArgAction::SetTrue)]
    partial: bool,

    /// Exit debug dump buffer length. 0 indicates default.
    #[clap(long, default_value = "0")]
    exit_dump_len: u32,

    /// Enable verbose output.
    #[clap(short = 'v', long, action = clap::ArgAction::SetTrue)]
    verbose: bool,

    /// Enable stats monitoring with the specified interval.
    #[clap(long)]
    stats: Option<f64>,

    /// Run in stats monitoring mode with the specified interval.
    #[clap(long)]
    monitor: Option<f64>,

    /// Show descriptions for statistics.
    #[clap(long)]
    help_stats: bool,

    /// Print scheduler version and exit.
    #[clap(short = 'V', long, action = clap::ArgAction::SetTrue)]
    version: bool,

    #[clap(flatten, next_help_heading = "Libbpf Options")]
    pub libbpf: LibbpfOpts,
}

// Time constants.
const NSEC_PER_USEC: u64 = 1_000;

// Circuit breaker constants
const CIRCUIT_BREAKER_THRESHOLD: f64 = 5.0;         // 500% of baseline triggers trip
const CIRCUIT_BREAKER_DURATION_MS: u64 = 100;        // Market closed for 100ms
const CIRCUIT_BREAKER_CHECK_INTERVAL_MS: u64 = 10;   // Check every 10ms

// Demurrage constants
const DEMURRAGE_INTERVAL_MS: u64 = 1;     // Apply every millisecond
const DEMURRAGE_MIN: f64 = 0.00001;       // 0.001% per ms
const DEMURRAGE_MAX: f64 = 0.001;         // 0.1% per ms
const DEMURRAGE_DEFAULT: f64 = 0.0001;    // 0.01% per ms

// Transfer constants
const MAX_TRANSFERS_PER_SECOND: u64 = 10;

// Shadow core telemetry constants
const IPC_SAMPLE_INTERVAL_MS: u64 = 100;  // Sample every 100ms
const IPC_HISTORY_SIZE: usize = 60;        // Keep 60 samples (~6 seconds)
const EXOGENOUS_DROP_THRESHOLD: f64 = 0.20; // 20% drop = exogenous event

// ESC tuning constants
const ESC_AMPLITUDE: f64 = 0.05;       // 5% perturbation
const ESC_FREQUENCY: f64 = 10.0;       // 10 Hz
const ESC_SAMPLES_NEEDED: usize = 100; // Samples before convergence

// Process class constants (match agentic_kernel.h)
const CLASS_INTERACTIVE: u8 = 0;
const CLASS_BATCH: u8 = 1;
const CLASS_DAEMON: u8 = 2;
const CLASS_REALTIME: u8 = 3;
const CLASS_UNKNOWN: u8 = 4;

// Per-process treasury (Rust-side bookkeeping)
#[derive(Debug, Clone)]
struct Treasury {
    balance: u64,
    total_spent: u64,
    process_class: u8,
}

impl Treasury {
    fn new(class_hint: u8) -> Self {
        Treasury {
            balance: IPO_BASELINE,
            total_spent: 0,
            process_class: class_hint,
        }
    }
}

// Task with bid-based ordering
#[derive(Debug, PartialEq, Eq, Clone)]
struct Task {
    qtask: QueuedTask,
    bid_amount: u64,
    enqueue_time: u64,
}

// Highest bid first. On tie: oldest task first. Then PID for stability.
impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .bid_amount
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

// Order Book: wraps a BTreeSet with market clearing semantics
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

// IPC/utilization sample for shadow core telemetry
#[derive(Debug, Clone)]
struct IpcSample {
    market_util: f64,   // CPU utilization on market cores (proxy for IPC)
    shadow_util: f64,   // CPU utilization on shadow cores
    delta: f64,         // market_util - shadow_util
    timestamp: Instant,
}

// Snapshot of /proc/stat CPU times for a single CPU
#[derive(Debug, Clone, Default)]
struct CpuTimes {
    busy: u64,   // user + nice + system + irq + softirq + steal
    total: u64,  // busy + idle + iowait
}

// ESC tuning state for auto-tuning bid multipliers
#[derive(Debug, Clone)]
struct EscState {
    phase: f64,
    amplitude: f64,
    frequency: f64,
    latency_samples: Vec<(f64, f64)>,
    best_multiplier: f64,
    converged: bool,
}

impl EscState {
    fn new() -> Self {
        EscState {
            phase: 0.0,
            amplitude: ESC_AMPLITUDE,
            frequency: ESC_FREQUENCY,
            latency_samples: Vec::new(),
            best_multiplier: 1.0,
            converged: false,
        }
    }

    fn step(&mut self, current_latency_ns: u64) -> f64 {
        if self.converged {
            return self.best_multiplier;
        }

        let perturbation = self.amplitude * (self.phase * self.frequency).sin();
        self.phase += 0.01;

        self.latency_samples
            .push((perturbation, current_latency_ns as f64));

        if self.latency_samples.len() >= ESC_SAMPLES_NEEDED {
            self.best_multiplier = self.compute_optimal_multiplier();
            self.converged = true;
        }

        1.0 + perturbation
    }

    fn compute_optimal_multiplier(&self) -> f64 {
        let n = self.latency_samples.len() as f64;
        if n == 0.0 {
            return 1.0;
        }

        let (sum_pert, sum_lat) = self
            .latency_samples
            .iter()
            .fold((0.0, 0.0), |(sp, sl), (p, l)| (sp + p, sl + l));

        let avg_pert = sum_pert / n;
        let avg_lat = sum_lat / n;

        let gradient: f64 = self
            .latency_samples
            .iter()
            .map(|(p, l)| (p - avg_pert) * (l - avg_lat))
            .sum::<f64>()
            / n;

        (1.0 - gradient.signum() * ESC_AMPLITUDE).clamp(0.5, 2.0)
    }
}

// PID controller bidding agent — one per process
#[derive(Debug, Clone)]
struct BiddingAgent {
    target_latency_ns: u64,
    kp: f64,
    ki: f64,
    kd: f64,
    integral: f64,
    prev_error: f64,
    bid_multiplier: f64,
    esc: Option<EscState>,
}

impl BiddingAgent {
    fn new_for_class(class: u8) -> Self {
        let (target, kp, ki, kd, multiplier) = match class {
            CLASS_INTERACTIVE => (1_000_000u64, 0.5, 0.01, 0.1, 0.20),
            CLASS_BATCH => (50_000_000, 0.1, 0.001, 0.05, 0.05),
            CLASS_DAEMON => (5_000_000, 0.3, 0.005, 0.08, 0.10),
            _ => (5_000_000, 0.3, 0.005, 0.08, 0.10),
        };

        // ESC tuning for unknown and daemon classes
        let esc = match class {
            CLASS_UNKNOWN | CLASS_DAEMON => Some(EscState::new()),
            _ => None,
        };

        BiddingAgent {
            target_latency_ns: target,
            kp,
            ki,
            kd,
            integral: 0.0,
            prev_error: 0.0,
            bid_multiplier: multiplier,
            esc,
        }
    }

    fn compute_bid(&mut self, current_latency_ns: u64, treasury_balance: u64) -> u64 {
        // ESC tuning phase: adjust multiplier if not yet converged
        if let Some(ref mut esc) = self.esc {
            let esc_mult = esc.step(current_latency_ns);
            if esc.converged {
                self.bid_multiplier = (self.bid_multiplier * esc.best_multiplier).clamp(0.01, 0.50);
                self.esc = None; // Done tuning
            } else {
                // During tuning, apply perturbation to base multiplier
                let error = self.target_latency_ns as f64 - current_latency_ns as f64;
                let base_fraction = self.bid_multiplier * esc_mult;
                let bid_fraction = base_fraction.clamp(0.01, 0.50);
                self.prev_error = error;
                return (treasury_balance as f64 * bid_fraction).max(1.0) as u64;
            }
        }

        // Steady-state PID control
        let error = self.target_latency_ns as f64 - current_latency_ns as f64;
        self.integral = (self.integral + error).clamp(-1e9, 1e9);
        let derivative = error - self.prev_error;
        self.prev_error = error;

        let adjustment = self.kp * error + self.ki * self.integral + self.kd * derivative;
        let normalized = adjustment / self.target_latency_ns as f64;

        let bid_fraction = (self.bid_multiplier * (1.0 + normalized)).clamp(0.01, 0.50);

        (treasury_balance as f64 * bid_fraction).max(1.0) as u64
    }
}

// === Central Bank (Phase 6) ===

#[derive(Debug, Clone, serde::Serialize)]
struct TelemetrySnapshot {
    timestamp: u64,
    numa_nodes: Vec<NumaSnapshot>,
    top_starved: Vec<StarvedProcess>,
    demurrage_rate: f64,
    reserve_balance: u64,
    total_supply: u64,
    circuit_breaker_trips_last_60s: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
struct NumaSnapshot {
    id: u32,
    delta_ipc: f64,
    avg_clearing_price: u64,
    tokens_in_circulation: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct StarvedProcess {
    pid: i32,
    comm: String,
    starvation_ms: u64,
    class: String,
    balance: u64,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
struct PolicyDecision {
    demurrage_rate: Option<f64>,
    ipo_baseline: Option<u64>,
    #[serde(default)]
    redistribute: Vec<Redistribution>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Redistribution {
    target_class: String,
    tokens: u64,
}

struct CentralBank {
    policy: Arc<Mutex<PolicyDecision>>,
    telemetry: Arc<Mutex<Option<TelemetrySnapshot>>>,
    circuit_breaker_trips: Arc<Mutex<u32>>,
}

impl CentralBank {
    fn new() -> Self {
        CentralBank {
            policy: Arc::new(Mutex::new(PolicyDecision::default())),
            telemetry: Arc::new(Mutex::new(None)),
            circuit_breaker_trips: Arc::new(Mutex::new(0)),
        }
    }

    // Spawn the Central Bank background thread.
    // Uses fallback heuristic until an LLM backend is configured.
    fn spawn(&self) {
        let policy = Arc::clone(&self.policy);
        let telemetry = Arc::clone(&self.telemetry);

        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(2));

                let snapshot = {
                    let guard = telemetry.lock().unwrap();
                    guard.clone()
                };

                let decision = match snapshot {
                    Some(snap) => Self::fallback_heuristic(&snap),
                    None => PolicyDecision::default(),
                };

                *policy.lock().unwrap() = decision;
            }
        });
    }

    // Fallback heuristic when LLM is unavailable:
    // - Adjust demurrage based on reserve level
    // - Redistribute to starved interactive processes
    fn fallback_heuristic(snapshot: &TelemetrySnapshot) -> PolicyDecision {
        let mut decision = PolicyDecision::default();

        // If reserve is low (<10% of supply), increase demurrage to recycle tokens
        let reserve_ratio = snapshot.reserve_balance as f64 / snapshot.total_supply.max(1) as f64;
        if reserve_ratio < 0.10 {
            decision.demurrage_rate = Some(
                (snapshot.demurrage_rate * 1.05).min(DEMURRAGE_MAX),
            );
        } else if reserve_ratio > 0.50 {
            // If reserve is high, decrease demurrage to let processes accumulate
            decision.demurrage_rate = Some(
                (snapshot.demurrage_rate * 0.95).max(DEMURRAGE_MIN),
            );
        }

        // Redistribute to starved interactive processes
        if !snapshot.top_starved.is_empty() {
            let interactive_starved: Vec<&StarvedProcess> = snapshot
                .top_starved
                .iter()
                .filter(|p| p.class == "interactive")
                .collect();

            if !interactive_starved.is_empty() {
                // Give up to 1% of reserve to starved interactive processes
                let budget = (snapshot.reserve_balance as f64 * 0.01) as u64;
                if budget > 0 {
                    decision.redistribute.push(Redistribution {
                        target_class: "interactive".to_string(),
                        tokens: budget,
                    });
                }
            }
        }

        decision
    }
}

// Main scheduler object
struct Scheduler<'a> {
    bpf: BpfScheduler<'a>,
    opts: &'a Opts,
    stats_server: StatsServer<(), Metrics>,
    order_book: OrderBook,
    treasuries: HashMap<i32, Treasury>,
    reserve: u64,
    total_supply: u64,
    init_page_faults: u64,
    slice_ns: u64,
    slice_ns_min: u64,
    schedule_cycle: u64,
    // Circuit breaker state
    csw_baseline: f64,
    csw_last_check: Instant,
    csw_last_total: u64,
    circuit_breaker_active: bool,
    circuit_breaker_until: Option<Instant>,
    // Demurrage state
    demurrage_rate: f64,
    last_demurrage: Instant,
    // Shadow core telemetry
    shadow_cpus: Vec<u32>,
    market_cpus: Vec<u32>,
    ipc_history: VecDeque<IpcSample>,
    last_ipc_sample: Instant,
    prev_cpu_times: HashMap<u32, CpuTimes>,
    // Bidding agents (Phase 5)
    agents: HashMap<i32, BiddingAgent>,
    // Central Bank (Phase 6)
    central_bank: CentralBank,
    circuit_breaker_trip_count: u32,
}

impl<'a> Scheduler<'a> {
    fn init(opts: &'a Opts, open_object: &'a mut MaybeUninit<OpenObject>) -> Result<Self> {
        let stats_server = StatsServer::new(stats::server_data()).launch()?;

        let slice_ns = opts.slice_us * NSEC_PER_USEC;
        let slice_ns_min = opts.slice_us_min * NSEC_PER_USEC;

        // Low-level BPF connector.
        let mut bpf = BpfScheduler::init(
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

        let nr_cpus = *bpf.nr_online_cpus_mut();
        let total_supply = nr_cpus * TOKENS_PER_CORE;

        // Designate shadow cores: last CPU as shadow (simple strategy).
        // With NUMA awareness, would pick one per NUMA node.
        let shadow_cpus: Vec<u32> = if nr_cpus > 1 {
            vec![(nr_cpus - 1) as u32]
        } else {
            vec![]
        };
        let market_cpus: Vec<u32> = (0..nr_cpus as u32)
            .filter(|c| !shadow_cpus.contains(c))
            .collect();

        // Write shadow core flags to BPF map
        for cpu in 0..nr_cpus as u32 {
            let key = cpu.to_ne_bytes();
            let val: u8 = if shadow_cpus.contains(&cpu) { 1 } else { 0 };
            let _ = bpf
                .skel
                .maps
                .shadow_cores
                .update(&key, &[val], libbpf_rs::MapFlags::ANY);
        }

        info!(
            "{} version {} - market scheduler, {} CPUs ({} market, {} shadow), {} total tokens",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION")),
            nr_cpus,
            market_cpus.len(),
            shadow_cpus.len(),
            total_supply,
        );

        Ok(Self {
            bpf,
            opts,
            stats_server,
            order_book: OrderBook::new(),
            treasuries: HashMap::new(),
            reserve: total_supply,
            total_supply,
            init_page_faults: 0,
            slice_ns,
            slice_ns_min,
            schedule_cycle: 0,
            csw_baseline: 0.0,
            csw_last_check: Instant::now(),
            csw_last_total: 0,
            circuit_breaker_active: false,
            circuit_breaker_until: None,
            demurrage_rate: DEMURRAGE_DEFAULT,
            last_demurrage: Instant::now(),
            shadow_cpus,
            market_cpus,
            ipc_history: VecDeque::with_capacity(IPC_HISTORY_SIZE),
            last_ipc_sample: Instant::now(),
            prev_cpu_times: HashMap::new(),
            agents: HashMap::new(),
            central_bank: CentralBank::new(),
            circuit_breaker_trip_count: 0,
        })
    }

    fn get_metrics(&mut self) -> Metrics {
        let page_faults = Self::get_page_faults().unwrap_or_default();
        if self.init_page_faults == 0 {
            self.init_page_faults = page_faults;
        }
        let nr_page_faults = page_faults - self.init_page_faults;

        Metrics {
            nr_running: *self.bpf.nr_running_mut(),
            nr_cpus: *self.bpf.nr_online_cpus_mut(),
            nr_queued: *self.bpf.nr_queued_mut(),
            nr_scheduled: *self.bpf.nr_scheduled_mut(),
            nr_page_faults,
            nr_user_dispatches: *self.bpf.nr_user_dispatches_mut(),
            nr_kernel_dispatches: *self.bpf.nr_kernel_dispatches_mut(),
            nr_cancel_dispatches: *self.bpf.nr_cancel_dispatches_mut(),
            nr_bounce_dispatches: *self.bpf.nr_bounce_dispatches_mut(),
            nr_failed_dispatches: *self.bpf.nr_failed_dispatches_mut(),
            nr_sched_congested: *self.bpf.nr_sched_congested_mut(),
        }
    }

    // Return current timestamp in ns.
    fn now() -> u64 {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        ts.as_nanos() as u64
    }

    // Classify a process based on its comm name.
    fn classify_process(task: &QueuedTask) -> u8 {
        let comm = task.comm_str();
        match comm.as_str() {
            "Xwayland" | "firefox" | "chrome" | "gnome-shell" | "pipewire" | "pulseaudio" => {
                CLASS_INTERACTIVE
            }
            "gcc" | "cc1" | "make" | "cargo" | "rustc" | "ffmpeg" | "x264" => CLASS_BATCH,
            "postgres" | "mysqld" | "nginx" | "apache2" | "systemd" => CLASS_DAEMON,
            _ => CLASS_UNKNOWN,
        }
    }

    // Get IPO allocation based on process class.
    fn ipo_for_class(class: u8) -> u64 {
        match class {
            CLASS_INTERACTIVE => IPO_BASELINE * 2,
            CLASS_BATCH => IPO_BASELINE / 2,
            CLASS_DAEMON => IPO_BASELINE,
            CLASS_REALTIME => 0,
            _ => IPO_BASELINE,
        }
    }

    // Drain tasks from BPF ring buffer and submit them as bids to the order book.
    fn drain_queued_tasks(&mut self) {
        loop {
            match self.bpf.dequeue_task() {
                Ok(Some(task)) => {
                    let pid = task.pid;
                    let timestamp = Self::now();

                    // Classify and create treasury + agent for new processes
                    if !self.treasuries.contains_key(&pid) {
                        let class = Self::classify_process(&task);
                        let ipo = Self::ipo_for_class(class).min(self.reserve);
                        self.reserve -= ipo;
                        let mut treasury = Treasury::new(class);
                        treasury.balance = ipo;
                        self.treasuries.insert(pid, treasury);
                        self.agents.insert(pid, BiddingAgent::new_for_class(class));
                    }

                    // Compute bid via agent (or use BPF heuristic as fallback)
                    let bid_amount = if let Some(agent) = self.agents.get_mut(&pid) {
                        let treasury = self.treasuries.get(&pid).unwrap();
                        let latency = task.starvation_ns;
                        agent.compute_bid(latency, treasury.balance)
                    } else {
                        task.bid_amount
                    };

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

    // Clear the market: dispatch the highest bidder.
    fn dispatch_task(&mut self) -> bool {
        let Some((winner, clearing_price)) = self.order_book.clear_market() else {
            return true;
        };

        let mut dispatched = DispatchedTask::new(&winner.qtask);

        // Convert tokens to timeslice, with minimum bound
        let slice_from_bid = clearing_price.saturating_mul(NSEC_PER_TOKEN);
        dispatched.slice_ns = slice_from_bid.max(self.slice_ns_min);

        // Use inverted bid as vtime for DSQ ordering (higher bid = lower vtime = earlier)
        dispatched.vtime = u64::MAX - winner.bid_amount;

        // Market metadata
        dispatched.clearing_price = clearing_price;
        dispatched.sequence = self.order_book.sequence;

        // CPU selection — preserve existing logic
        dispatched.cpu = if self.opts.percpu_local {
            winner.qtask.cpu
        } else {
            match self
                .bpf
                .select_cpu(winner.qtask.pid, winner.qtask.cpu, winner.qtask.flags)
            {
                cpu if cpu >= 0 => cpu,
                _ => RL_CPU_ANY,
            }
        };

        // Deduct tokens from treasury
        if let Some(treasury) = self.treasuries.get_mut(&winner.qtask.pid) {
            let cost = clearing_price.min(treasury.balance);
            treasury.balance -= cost;
            treasury.total_spent += cost;
            self.reserve += cost;
        }

        // Send to BPF dispatcher
        if self.bpf.dispatch_task(&dispatched).is_err() {
            // Failed: refund tokens and re-enqueue
            if let Some(treasury) = self.treasuries.get_mut(&winner.qtask.pid) {
                let refund = clearing_price.min(treasury.total_spent);
                treasury.balance += refund;
                treasury.total_spent -= refund;
                self.reserve = self.reserve.saturating_sub(refund);
            }
            self.order_book.submit_bid(winner);
            return false;
        }

        true
    }

    // Sync Rust-side treasury state to BPF maps so get_task_info() can read balances.
    fn sync_treasuries_to_bpf(&mut self) {
        for (&pid, treasury) in &self.treasuries {
            // ipo_complete = 1 if the agent has no ESC (already classified)
            // or ESC has converged
            let ipo_complete = match self.agents.get(&pid) {
                Some(agent) => {
                    if agent.esc.is_none() {
                        1
                    } else {
                        0
                    }
                }
                None => 1,
            };

            let entry = bpf_intf::treasury_entry {
                balance: treasury.balance,
                last_update_ts: Self::now(),
                total_spent: treasury.total_spent,
                ipo_complete,
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

            let _ = self
                .bpf
                .skel
                .maps
                .treasury
                .update(&key, value, libbpf_rs::MapFlags::ANY);
        }
    }

    // Remove treasuries and agents for processes that no longer exist.
    fn gc_treasuries(&mut self) {
        let dead_pids: Vec<i32> = self
            .treasuries
            .keys()
            .filter(|&&pid| !std::path::Path::new(&format!("/proc/{}", pid)).exists())
            .copied()
            .collect();

        for pid in dead_pids {
            if let Some(treasury) = self.treasuries.remove(&pid) {
                self.reserve += treasury.balance;

                let key = pid.to_ne_bytes();
                let _ = self.bpf.skel.maps.treasury.delete(&key);
            }
            self.agents.remove(&pid);
        }
    }

    // Apply demurrage (wealth tax) — decay unspent token balances back to reserve.
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

    // Process pending transfer requests from BPF map.
    fn process_transfers(&mut self) {
        let mut processed: Vec<i32> = Vec::new();

        for key in self.bpf.skel.maps.transfer_requests.keys() {
            let child_pid = i32::from_ne_bytes(match key[..4].try_into() {
                Ok(b) => b,
                Err(_) => continue,
            });

            if let Ok(Some(value)) = self
                .bpf
                .skel
                .maps
                .transfer_requests
                .lookup(&key, libbpf_rs::MapFlags::ANY)
            {
                if value.len() >= 12 {
                    let parent_pid = i32::from_ne_bytes(value[0..4].try_into().unwrap());
                    let amount = u64::from_ne_bytes(value[4..12].try_into().unwrap());

                    // Validate parent has sufficient balance and execute transfer
                    let parent_ok = self
                        .treasuries
                        .get(&parent_pid)
                        .map_or(false, |t| t.balance >= amount);

                    if parent_ok && self.treasuries.contains_key(&child_pid) {
                        self.treasuries.get_mut(&parent_pid).unwrap().balance -= amount;
                        self.treasuries.get_mut(&child_pid).unwrap().balance += amount;
                        // No net change to reserve — peer transfer
                    }
                }
                processed.push(child_pid);
            }
        }

        for pid in processed {
            let _ = self
                .bpf
                .skel
                .maps
                .transfer_requests
                .delete(&pid.to_ne_bytes());
        }
    }

    // Read total context switches across all CPUs from the PERCPU_ARRAY map.
    fn read_csw_total(&self) -> u64 {
        let key = 0u32.to_ne_bytes();
        match self
            .bpf
            .skel
            .maps
            .csw_counter
            .lookup_percpu(&key, libbpf_rs::MapFlags::ANY)
        {
            Ok(Some(values)) => values
                .iter()
                .map(|v| {
                    if v.len() >= 8 {
                        u64::from_ne_bytes(v[..8].try_into().unwrap_or([0; 8]))
                    } else {
                        0
                    }
                })
                .sum(),
            _ => 0,
        }
    }

    // Write circuit breaker flag to all NUMA nodes in BPF market map.
    fn reset_market_state_in_bpf(&mut self, tripped: bool) {
        for numa_node in 0..8u32 {
            let ms = bpf_intf::market_state {
                current_clearing_price: 0,
                total_tokens_in_circulation: self.total_supply - self.reserve,
                circuit_breaker_tripped: if tripped { 1 } else { 0 },
                current_demurrage_rate: 10,
            };

            let key = numa_node.to_ne_bytes();
            let value = unsafe {
                std::slice::from_raw_parts(
                    &ms as *const _ as *const u8,
                    std::mem::size_of::<bpf_intf::market_state>(),
                )
            };

            let _ = self
                .bpf
                .skel
                .maps
                .market
                .update(&key, value, libbpf_rs::MapFlags::ANY);
        }
    }

    // Haircut protocol: slash excess balances and drain the order book.
    fn haircut_protocol(&mut self) {
        // Drain all pending bids
        self.order_book.book.clear();

        // Apply 90% slash to all treasuries above baseline
        for (&pid, treasury) in self.treasuries.iter_mut() {
            let class_baseline = Self::ipo_for_class(treasury.process_class);
            if treasury.balance > class_baseline {
                let excess = treasury.balance - class_baseline;
                let slash = (excess as f64 * 0.90) as u64;
                treasury.balance -= slash;
                self.reserve += slash;
            }
            // Reset agent PID integral to prevent windup after haircut
            if let Some(agent) = self.agents.get_mut(&pid) {
                agent.integral = 0.0;
            }
        }

        info!(
            "Haircut complete: {} treasuries slashed, reserve={}",
            self.treasuries.len(),
            self.reserve
        );
    }

    // Trip the circuit breaker: set BPF flag, execute haircut.
    fn trip_circuit_breaker(&mut self) {
        self.circuit_breaker_active = true;
        self.circuit_breaker_until =
            Some(Instant::now() + Duration::from_millis(CIRCUIT_BREAKER_DURATION_MS));
        self.circuit_breaker_trip_count += 1;

        self.reset_market_state_in_bpf(true);
        self.haircut_protocol();
    }

    // Monitor context-switch rate and trip/release the circuit breaker.
    fn check_circuit_breaker(&mut self) {
        let now = Instant::now();

        // Check if active breaker has expired
        if let Some(until) = self.circuit_breaker_until {
            if now >= until {
                self.circuit_breaker_active = false;
                self.circuit_breaker_until = None;
                self.reset_market_state_in_bpf(false);
                info!("Circuit breaker released, market re-opened");
            }
            return;
        }

        // Only check at the configured interval
        if now.duration_since(self.csw_last_check).as_millis()
            < CIRCUIT_BREAKER_CHECK_INTERVAL_MS as u128
        {
            return;
        }

        // Read per-CPU counters and compute rate
        let total_csw = self.read_csw_total();
        let elapsed_ms = now.duration_since(self.csw_last_check).as_millis() as f64;
        if elapsed_ms == 0.0 {
            return;
        }
        let rate = (total_csw.saturating_sub(self.csw_last_total)) as f64 / elapsed_ms;

        // Update baseline with exponential moving average
        if self.csw_baseline == 0.0 {
            self.csw_baseline = rate;
        } else {
            self.csw_baseline = self.csw_baseline * 0.95 + rate * 0.05;
        }

        // Trip if rate spikes above threshold
        if self.csw_baseline > 0.0 && rate > self.csw_baseline * CIRCUIT_BREAKER_THRESHOLD {
            warn!(
                "Circuit breaker TRIPPED: csw rate {:.1}/ms vs baseline {:.1}/ms",
                rate, self.csw_baseline
            );
            self.trip_circuit_breaker();
        }

        self.csw_last_check = now;
        self.csw_last_total = total_csw;
    }

    // Read per-CPU times from /proc/stat.
    fn read_cpu_times() -> HashMap<u32, CpuTimes> {
        let mut result = HashMap::new();
        let Ok(file) = std::fs::File::open("/proc/stat") else {
            return result;
        };
        let reader = io::BufReader::new(file);

        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if !line.starts_with("cpu") || line.starts_with("cpu ") {
                continue;
            }
            // Lines like: cpu0 1234 56 789 ...
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 8 {
                continue;
            }
            let cpu_id: u32 = match parts[0].strip_prefix("cpu").and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => continue,
            };
            let vals: Vec<u64> = parts[1..]
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
            if vals.len() < 7 {
                continue;
            }
            // user, nice, system, idle, iowait, irq, softirq, [steal]
            let busy = vals[0] + vals[1] + vals[2] + vals[5] + vals[6]
                + vals.get(7).copied().unwrap_or(0);
            let total = busy + vals[3] + vals[4];
            result.insert(cpu_id, CpuTimes { busy, total });
        }
        result
    }

    // Compute average utilization for a set of CPUs since last sample.
    fn compute_util(&self, cpus: &[u32], current: &HashMap<u32, CpuTimes>) -> f64 {
        let mut total_busy_delta = 0u64;
        let mut total_delta = 0u64;

        for &cpu in cpus {
            let Some(cur) = current.get(&cpu) else {
                continue;
            };
            let prev = self.prev_cpu_times.get(&cpu);
            let (prev_busy, prev_total) = match prev {
                Some(p) => (p.busy, p.total),
                None => (0, 0),
            };
            let busy_delta = cur.busy.saturating_sub(prev_busy);
            let t_delta = cur.total.saturating_sub(prev_total);
            total_busy_delta += busy_delta;
            total_delta += t_delta;
        }

        if total_delta == 0 {
            0.0
        } else {
            total_busy_delta as f64 / total_delta as f64
        }
    }

    // Sample IPC (using CPU utilization as proxy) for market vs shadow cores.
    fn sample_ipc(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_ipc_sample).as_millis() < IPC_SAMPLE_INTERVAL_MS as u128 {
            return;
        }

        let current = Self::read_cpu_times();

        // Only compute if we have a previous snapshot
        if !self.prev_cpu_times.is_empty() {
            let market_util = self.compute_util(&self.market_cpus, &current);
            let shadow_util = self.compute_util(&self.shadow_cpus, &current);
            let delta = market_util - shadow_util;

            let sample = IpcSample {
                market_util,
                shadow_util,
                delta,
                timestamp: now,
            };

            if self.ipc_history.len() >= IPC_HISTORY_SIZE {
                self.ipc_history.pop_front();
            }
            self.ipc_history.push_back(sample);
        }

        self.prev_cpu_times = current;
        self.last_ipc_sample = now;
    }

    // Detect exogenous events: both market and shadow drop >20% simultaneously.
    fn is_exogenous_event(&self) -> bool {
        if self.ipc_history.len() < 2 {
            return false;
        }
        let prev = &self.ipc_history[self.ipc_history.len() - 2];
        let curr = &self.ipc_history[self.ipc_history.len() - 1];

        let market_drop = prev.market_util > 0.0
            && (prev.market_util - curr.market_util) / prev.market_util > EXOGENOUS_DROP_THRESHOLD;
        let shadow_drop = prev.shadow_util > 0.0
            && (prev.shadow_util - curr.shadow_util) / prev.shadow_util > EXOGENOUS_DROP_THRESHOLD;

        market_drop && shadow_drop
    }

    fn class_name(class: u8) -> &'static str {
        match class {
            CLASS_INTERACTIVE => "interactive",
            CLASS_BATCH => "batch",
            CLASS_DAEMON => "daemon",
            CLASS_REALTIME => "realtime",
            _ => "unknown",
        }
    }

    fn count_class(&self, class: u8) -> u64 {
        self.treasuries
            .values()
            .filter(|t| t.process_class == class)
            .count() as u64
    }

    // Build a telemetry snapshot for the Central Bank.
    fn build_telemetry_snapshot(&self) -> TelemetrySnapshot {
        // Compute average delta from IPC history
        let avg_delta = if self.ipc_history.is_empty() {
            0.0
        } else {
            let sum: f64 = self.ipc_history.iter().map(|s| s.market_util - s.shadow_util).sum();
            sum / self.ipc_history.len() as f64
        };

        let numa_nodes = vec![NumaSnapshot {
            id: 0,
            delta_ipc: avg_delta,
            avg_clearing_price: 0, // Could track running average
            tokens_in_circulation: self.total_supply - self.reserve,
        }];

        // Find top 5 starved processes (those with highest starvation and lowest balance)
        let mut starved: Vec<StarvedProcess> = self
            .treasuries
            .iter()
            .filter(|(_, t)| t.balance < IPO_BASELINE / 2)
            .map(|(&pid, t)| StarvedProcess {
                pid,
                comm: format!("pid:{}", pid), // comm not stored in treasury
                starvation_ms: 0,
                class: Self::class_name(t.process_class).to_string(),
                balance: t.balance,
            })
            .collect();
        starved.sort_by_key(|s| s.balance);
        starved.truncate(5);

        TelemetrySnapshot {
            timestamp: Self::now(),
            numa_nodes,
            top_starved: starved,
            demurrage_rate: self.demurrage_rate,
            reserve_balance: self.reserve,
            total_supply: self.total_supply,
            circuit_breaker_trips_last_60s: self.circuit_breaker_trip_count,
        }
    }

    // Apply the latest policy decision from the Central Bank.
    fn apply_policy(&mut self) {
        let decision = self.central_bank.policy.lock().unwrap().clone();

        if let Some(rate) = decision.demurrage_rate {
            self.demurrage_rate = rate.clamp(DEMURRAGE_MIN, DEMURRAGE_MAX);
        }

        for redist in &decision.redistribute {
            let class = match redist.target_class.as_str() {
                "interactive" => CLASS_INTERACTIVE,
                "batch" => CLASS_BATCH,
                "daemon" => CLASS_DAEMON,
                _ => continue,
            };
            let count = self.count_class(class).max(1);
            let share = redist.tokens / count;

            for treasury in self.treasuries.values_mut() {
                if treasury.process_class == class {
                    let available = share.min(self.reserve);
                    treasury.balance += available;
                    self.reserve -= available;
                }
            }
        }
    }

    // Verify token supply conservation invariant.
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

    // Main scheduling function
    fn schedule(&mut self) {
        self.schedule_cycle += 1;

        // Check circuit breaker every cycle
        self.check_circuit_breaker();

        // If breaker is active, BPF handles dispatch directly — just drain to keep ring buffer empty
        if self.circuit_breaker_active {
            loop {
                match self.bpf.dequeue_task() {
                    Ok(Some(_)) => continue,
                    _ => break,
                }
            }
            self.bpf.notify_complete(0);
            return;
        }

        // Apply demurrage every cycle (self-throttles to 1ms intervals)
        self.apply_demurrage();

        self.drain_queued_tasks();
        self.dispatch_task();

        // Periodic maintenance
        if self.schedule_cycle % 1000 == 0 {
            self.gc_treasuries();
            self.audit_token_supply();
        }
        if self.schedule_cycle % 100 == 0 {
            self.sync_treasuries_to_bpf();
            self.process_transfers();
        }

        // Shadow core telemetry (self-throttles to 100ms intervals)
        self.sample_ipc();
        if self.is_exogenous_event() {
            info!(
                "Exogenous event detected: both market and shadow utilization dropped >{}%",
                (EXOGENOUS_DROP_THRESHOLD * 100.0) as u32
            );
        }

        // Central Bank: publish telemetry and apply policy every 500 cycles
        if self.schedule_cycle % 500 == 0 {
            let snapshot = self.build_telemetry_snapshot();
            *self.central_bank.telemetry.lock().unwrap() = Some(snapshot);
            self.apply_policy();
        }

        self.bpf.notify_complete(self.order_book.len() as u64);
    }

    fn get_page_faults() -> Result<u64, io::Error> {
        let myself = Process::myself().map_err(io::Error::other)?;
        let stat = myself.stat().map_err(io::Error::other)?;

        Ok(stat.minflt + stat.majflt)
    }

    fn run(&mut self) -> Result<UserExitInfo> {
        let (res_ch, req_ch) = self.stats_server.channels();

        // Spawn the Central Bank background thread
        self.central_bank.spawn();
        info!("Central Bank daemon started (fallback heuristic mode)");

        while !self.bpf.exited() {
            self.schedule();

            if req_ch.try_recv().is_ok() {
                res_ch.send(self.get_metrics())?;
            }
        }

        self.bpf.shutdown_and_report()
    }
}

impl Drop for Scheduler<'_> {
    fn drop(&mut self) {
        info!("Unregister {SCHEDULER_NAME} scheduler");
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();

    if opts.version {
        println!(
            "{} version {}",
            SCHEDULER_NAME,
            build_id::full_version(env!("CARGO_PKG_VERSION")),
        );
        return Ok(());
    }

    if opts.help_stats {
        stats::server_data().describe_meta(&mut std::io::stdout(), None)?;
        return Ok(());
    }

    let loglevel = simplelog::LevelFilter::Info;

    let mut lcfg = simplelog::ConfigBuilder::new();
    lcfg.set_time_offset_to_local()
        .expect("Failed to set local time offset")
        .set_time_level(simplelog::LevelFilter::Error)
        .set_location_level(simplelog::LevelFilter::Off)
        .set_target_level(simplelog::LevelFilter::Off)
        .set_thread_level(simplelog::LevelFilter::Off);
    simplelog::TermLogger::init(
        loglevel,
        lcfg.build(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )?;

    if let Some(intv) = opts.monitor.or(opts.stats) {
        let jh =
            std::thread::spawn(move || stats::monitor(Duration::from_secs_f64(intv)).unwrap());
        if opts.monitor.is_some() {
            let _ = jh.join();
            return Ok(());
        }
    }

    let mut open_object = MaybeUninit::uninit();
    loop {
        let mut sched = Scheduler::init(&opts, &mut open_object)?;
        if !sched.run()?.should_restart() {
            break;
        }
    }

    Ok(())
}
