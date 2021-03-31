// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use super::metrics::tls_collect_rate_limiter_request_wait;
use super::{IOOp, IOPriority, IOType};

#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use crossbeam_utils::CachePadded;
use parking_lot::{Mutex, MutexGuard};
use strum::EnumCount;
use tikv_util::time::Instant;

/// Record accumulated bytes through of different types.
/// Used for testing and metrics.
#[derive(Debug)]
pub struct IORateLimiterStatistics {
    read_bytes: [CachePadded<AtomicUsize>; IOType::COUNT],
    write_bytes: [CachePadded<AtomicUsize>; IOType::COUNT],
}

impl IORateLimiterStatistics {
    pub fn new() -> Self {
        IORateLimiterStatistics {
            read_bytes: Default::default(),
            write_bytes: Default::default(),
        }
    }

    pub fn fetch(&self, io_type: IOType, io_op: IOOp) -> usize {
        let io_type_idx = io_type as usize;
        match io_op {
            IOOp::Read => self.read_bytes[io_type_idx].load(Ordering::Relaxed),
            IOOp::Write => self.write_bytes[io_type_idx].load(Ordering::Relaxed),
        }
    }

    pub fn record(&self, io_type: IOType, io_op: IOOp, bytes: usize) {
        let io_type_idx = io_type as usize;
        match io_op {
            IOOp::Read => {
                self.read_bytes[io_type_idx].fetch_add(bytes, Ordering::Relaxed);
            }
            IOOp::Write => {
                self.write_bytes[io_type_idx].fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    pub fn reset(&self) {
        for i in 0..IOType::COUNT {
            self.read_bytes[i].store(0, Ordering::Relaxed);
            self.write_bytes[i].store(0, Ordering::Relaxed);
        }
    }
}

macro_rules! do_sleep {
    ($duration:expr, sync) => {
        std::thread::sleep($duration)
    };
    ($duration:expr, async) => {
        tokio::time::delay_for($duration).await
    };
}

const DEFAULT_REFILL_PERIOD: Duration = Duration::from_millis(50);

/// Limit total IO flow below provided threshold by throttling lower-priority IOs.
/// Rate limit is disabled when total IO threshold is set to zero.
#[derive(Debug)]
struct PriorityBasedIORateLimiter {
    // IO amount passed through within current epoch
    bytes_through: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    // Maximum IOs permitted within current epoch
    bytes_per_epoch: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    protected: Mutex<PriorityBasedIORateLimiterProtected>,
}

#[derive(Debug)]
struct PriorityBasedIORateLimiterProtected {
    next_refill_time: Instant,
    // IOs that are can't be fulfilled in current epoch
    pending_bytes: [usize; IOPriority::COUNT],
    // Used to smoothly update IO budgets
    history_epoch_count: usize,
    history_bytes: [usize; IOPriority::COUNT],
}

impl PriorityBasedIORateLimiterProtected {
    fn new() -> Self {
        PriorityBasedIORateLimiterProtected {
            next_refill_time: Instant::now_coarse() + DEFAULT_REFILL_PERIOD,
            pending_bytes: [0; IOPriority::COUNT],
            history_epoch_count: 0,
            history_bytes: [0; IOPriority::COUNT],
        }
    }
}

/// Actual implementation for requesting IOs from PriorityBasedIORateLimiter.
/// An attempt will be recorded first. If the attempted amount exceeds the available quotas of
/// current epoch, the requester will register itself and sleep until next epoch.
macro_rules! request_imp {
    ($limiter:ident, $priority:ident, $amount:ident, $mode:tt) => {{
        let priority_idx = $priority as usize;
        let cached_bytes_per_refill =
            $limiter.bytes_per_epoch[priority_idx].load(Ordering::Relaxed);
        if cached_bytes_per_refill == 0 {
            return $amount;
        }
        let amount = std::cmp::min($amount, cached_bytes_per_refill);
        let bytes_through =
            $limiter.bytes_through[priority_idx].fetch_add(amount, Ordering::AcqRel) + amount;
        if bytes_through <= cached_bytes_per_refill {
            return amount;
        }
        let now = Instant::now_coarse();
        let mut wait = Duration::from_millis(0);
        // hold a snapshot ticket of pending bytes
        let pending = {
            let mut locked = $limiter.protected.lock();
            locked.pending_bytes[priority_idx] += amount;
            if locked.next_refill_time <= now {
                $limiter.refill(&mut locked, now);
            } else {
                wait += locked.next_refill_time - now;
            }
            locked.pending_bytes[priority_idx]
        };
        // wait until our ticket can actually be served
        wait += DEFAULT_REFILL_PERIOD * (pending / cached_bytes_per_refill) as u32;
        tls_collect_rate_limiter_request_wait($priority.as_str(), wait);
        do_sleep!(wait, $mode);
        amount
    }};
}

impl PriorityBasedIORateLimiter {
    fn new() -> Self {
        PriorityBasedIORateLimiter {
            bytes_through: Default::default(),
            bytes_per_epoch: Default::default(),
            protected: Mutex::new(PriorityBasedIORateLimiterProtected::new()),
        }
    }

    /// Dynamically changes the total IO flow threshold.
    #[allow(dead_code)]
    fn set_bytes_per_sec(&self, bytes_per_sec: usize) {
        let now = (bytes_per_sec as f64 * DEFAULT_REFILL_PERIOD.as_secs_f64()) as usize;
        let before = self.bytes_per_epoch[IOPriority::High as usize].swap(now, Ordering::Relaxed);
        if before == 0 || now == 0 {
            // toggle on/off rate limit.
            // we hold this lock so a concurrent refill can't negate our effort.
            let _locked = self.protected.lock();
            for p in &[IOPriority::Medium, IOPriority::Low] {
                let pi = *p as usize;
                self.bytes_per_epoch[pi].store(now, Ordering::Relaxed);
            }
        }
    }

    fn request(&self, priority: IOPriority, amount: usize) -> usize {
        request_imp!(self, priority, amount, sync)
    }

    async fn async_request(&self, priority: IOPriority, amount: usize) -> usize {
        request_imp!(self, priority, amount, async)
    }

    /// Update and refill IO budgets for next epoch.
    fn refill(&self, locked: &mut MutexGuard<PriorityBasedIORateLimiterProtected>, now: Instant) {
        const UPDATE_BUDGETS_EVERY_N_EPOCHS: usize = 5;
        // keep in sync with a potentially skewed clock
        locked.next_refill_time = now + DEFAULT_REFILL_PERIOD;
        let mut limit = self.bytes_per_epoch[IOPriority::High as usize].load(Ordering::Relaxed);
        debug_assert!(limit > 0);
        let should_update_budgets =
            if locked.history_epoch_count == UPDATE_BUDGETS_EVERY_N_EPOCHS - 1 {
                locked.history_epoch_count = 0;
                true
            } else {
                locked.history_epoch_count += 1;
                false
            };

        debug_assert!(
            IOPriority::High as usize == IOPriority::Medium as usize + 1
                && IOPriority::Medium as usize == IOPriority::Low as usize + 1
        );
        for p in &[IOPriority::High, IOPriority::Medium] {
            let p = *p as usize;
            // calculate budgets from next epoch used to satisfy pending IOs
            let satisfied = if locked.pending_bytes[p] > limit {
                // preserve pending IOs that still can't be satisfied
                locked.pending_bytes[p] -= limit;
                limit
            } else {
                std::mem::replace(&mut locked.pending_bytes[p], 0)
            };
            locked.history_bytes[p] += std::cmp::min(
                self.bytes_through[p].swap(satisfied, Ordering::Release),
                limit,
            );
            if should_update_budgets {
                let estimated_bytes_through = std::mem::replace(&mut locked.history_bytes[p], 0)
                    / UPDATE_BUDGETS_EVERY_N_EPOCHS;
                limit = if limit > estimated_bytes_through {
                    limit - estimated_bytes_through
                } else {
                    1 // a small positive value
                };
                self.bytes_per_epoch[p - 1].store(limit, Ordering::Relaxed);
            } else {
                limit = self.bytes_per_epoch[p - 1].load(Ordering::Relaxed);
            }
        }
        let p = IOPriority::Low as usize;
        let satisfied = if locked.pending_bytes[p] > limit {
            locked.pending_bytes[p] -= limit;
            limit
        } else {
            std::mem::replace(&mut locked.pending_bytes[p], 0)
        };
        self.bytes_through[p].store(satisfied, Ordering::Release);
    }

    #[cfg(test)]
    fn critical_section(&self, now: Instant) {
        let mut locked = self.protected.lock();
        self.refill(&mut locked, now);
    }
}

/// An instance of `IORateLimiter` should be safely shared between threads.
#[derive(Debug)]
pub struct IORateLimiter {
    priority_map: [IOPriority; IOType::COUNT],
    throughput_limiter: Arc<PriorityBasedIORateLimiter>,
    stats: Option<Arc<IORateLimiterStatistics>>,
}

impl IORateLimiter {
    pub fn new(enable_statistics: bool) -> IORateLimiter {
        IORateLimiter {
            priority_map: [IOPriority::High; IOType::COUNT],
            throughput_limiter: Arc::new(PriorityBasedIORateLimiter::new()),
            stats: if enable_statistics {
                Some(Arc::new(IORateLimiterStatistics::new()))
            } else {
                None
            },
        }
    }

    pub fn set_io_priority(&mut self, io_type: IOType, io_priority: IOPriority) {
        self.priority_map[io_type as usize] = io_priority;
    }

    pub fn statistics(&self) -> Option<Arc<IORateLimiterStatistics>> {
        self.stats.clone()
    }

    pub fn set_io_rate_limit(&self, rate: usize) {
        self.throughput_limiter.set_bytes_per_sec(rate);
    }

    /// Requests for token for bytes and potentially update statistics. If this
    /// request can not be satisfied, the call is blocked. Granted token can be
    /// less than the requested bytes, but must be greater than zero.
    pub fn request(&self, io_type: IOType, io_op: IOOp, mut bytes: usize) -> usize {
        if io_op == IOOp::Write {
            let priority = self.priority_map[io_type as usize];
            if priority == IOPriority::Stop {
                do_sleep!(Duration::from_secs(1000), sync);
            }
            bytes = self.throughput_limiter.request(priority, bytes);
        }
        if let Some(stats) = &self.stats {
            stats.record(io_type, io_op, bytes);
        }
        bytes
    }

    /// Asynchronously requests for token for bytes and potentially update
    /// statistics. If this request can not be satisfied, the call is blocked.
    /// Granted token can be less than the requested bytes, but must be greater
    /// than zero.
    pub async fn async_request(&self, io_type: IOType, io_op: IOOp, mut bytes: usize) -> usize {
        if io_op == IOOp::Write {
            let priority = self.priority_map[io_type as usize];
            if priority == IOPriority::Stop {
                do_sleep!(Duration::from_secs(1000), async);
            }
            bytes = self.throughput_limiter.async_request(priority, bytes).await;
        }
        if let Some(stats) = &self.stats {
            stats.record(io_type, io_op, bytes);
        }
        bytes
    }
}

lazy_static! {
    static ref IO_RATE_LIMITER: Mutex<Option<Arc<IORateLimiter>>> = Mutex::new(None);
}

// Do NOT use this method in test environment.
pub fn set_io_rate_limiter(limiter: Option<Arc<IORateLimiter>>) {
    *IO_RATE_LIMITER.lock() = limiter;
}

pub fn get_io_rate_limiter() -> Option<Arc<IORateLimiter>> {
    if let Some(ref limiter) = *IO_RATE_LIMITER.lock() {
        Some(limiter.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approximate_eq(left: f64, right: f64) {
        assert!(left >= right * 0.9);
        assert!(left <= right * 1.1);
    }

    struct BackgroundContext {
        threads: Vec<std::thread::JoinHandle<()>>,
        stop: Option<Arc<AtomicBool>>,
    }

    impl Drop for BackgroundContext {
        fn drop(&mut self) {
            if let Some(stop) = &self.stop {
                stop.store(true, Ordering::Relaxed);
            }
            for t in self.threads.drain(..) {
                t.join().unwrap();
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct Request(IOType, IOOp, usize);

    fn start_background_jobs(
        limiter: &Arc<IORateLimiter>,
        job_count: usize,
        request: Request,
        interval: Option<Duration>,
    ) -> BackgroundContext {
        let mut threads = vec![];
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..job_count {
            let stop = stop.clone();
            let limiter = limiter.clone();
            let t = std::thread::spawn(move || {
                let Request(io_type, op, len) = request;
                while !stop.load(Ordering::Relaxed) {
                    limiter.request(io_type, op, len);
                    if let Some(interval) = interval {
                        std::thread::sleep(interval);
                    }
                }
            });
            threads.push(t);
        }
        BackgroundContext {
            threads,
            stop: Some(stop),
        }
    }

    fn verify_rate_limit(limiter: &Arc<IORateLimiter>, bytes_per_sec: usize) {
        let stats = limiter.statistics().unwrap();
        stats.reset();
        limiter.set_io_rate_limit(bytes_per_sec);
        let duration = {
            let begin = Instant::now();
            {
                let _context = start_background_jobs(
                    limiter,
                    10, /*job_count*/
                    Request(IOType::ForegroundWrite, IOOp::Write, 10),
                    None, /*interval*/
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        approximate_eq(
            stats.fetch(IOType::ForegroundWrite, IOOp::Write) as f64,
            bytes_per_sec as f64 * duration.as_secs_f64(),
        );
    }

    #[test]
    fn test_rate_limited_heavy_flow() {
        let low_bytes_per_sec = 2000;
        let high_bytes_per_sec = 10000;
        let limiter = Arc::new(IORateLimiter::new(true /*enable_statistics*/));
        verify_rate_limit(&limiter, low_bytes_per_sec);
        verify_rate_limit(&limiter, high_bytes_per_sec);
        verify_rate_limit(&limiter, low_bytes_per_sec);
    }

    #[test]
    fn test_rate_limited_light_flow() {
        let kbytes_per_sec = 3;
        let actual_kbytes_per_sec = 2;
        let limiter = Arc::new(IORateLimiter::new(true /*enable_statistics*/));
        limiter.set_io_rate_limit(kbytes_per_sec * 1000);
        let stats = limiter.statistics().unwrap();
        let duration = {
            let begin = Instant::now();
            {
                // each thread request at most 1000 bytes per second
                let _context = start_background_jobs(
                    &limiter,
                    actual_kbytes_per_sec, /*job_count*/
                    Request(IOType::Compaction, IOOp::Write, 1),
                    Some(Duration::from_millis(1)),
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        approximate_eq(
            stats.fetch(IOType::Compaction, IOOp::Write) as f64,
            actual_kbytes_per_sec as f64 * duration.as_secs_f64() * 1000.0,
        );
    }

    #[test]
    fn test_rate_limited_hybrid_flow() {
        let bytes_per_sec = 100000;
        let write_work = 50;
        let compaction_work = 60;
        let import_work = 10;
        let mut limiter = IORateLimiter::new(true /*enable_statistics*/);
        limiter.set_io_rate_limit(bytes_per_sec);
        limiter.set_io_priority(IOType::Compaction, IOPriority::Medium);
        limiter.set_io_priority(IOType::Import, IOPriority::Low);
        let stats = limiter.statistics().unwrap();
        let limiter = Arc::new(limiter);
        let duration = {
            let begin = Instant::now();
            {
                let _write = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::ForegroundWrite,
                        IOOp::Write,
                        write_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                let _compaction = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::Compaction,
                        IOOp::Write,
                        compaction_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                let _import = start_background_jobs(
                    &limiter,
                    2, /*job_count*/
                    Request(
                        IOType::Import,
                        IOOp::Write,
                        import_work * bytes_per_sec / 100 / 1000 / 2,
                    ),
                    Some(Duration::from_millis(1)),
                );
                std::thread::sleep(Duration::from_secs(2));
            }
            let end = Instant::now();
            end.duration_since(begin)
        };
        let write_bytes = stats.fetch(IOType::ForegroundWrite, IOOp::Write);
        approximate_eq(
            write_bytes as f64,
            (write_work * bytes_per_sec / 100) as f64 * duration.as_secs_f64(),
        );
        let compaction_bytes = stats.fetch(IOType::Compaction, IOOp::Write);
        approximate_eq(
            compaction_bytes as f64,
            ((100 - write_work) * bytes_per_sec / 100) as f64 * duration.as_secs_f64(),
        );
        let import_bytes = stats.fetch(IOType::Import, IOOp::Write);
        let total_bytes = write_bytes + import_bytes + compaction_bytes;
        approximate_eq(
            total_bytes as f64,
            bytes_per_sec as f64 * duration.as_secs_f64(),
        );
    }

    #[bench]
    fn bench_critical_section(b: &mut test::Bencher) {
        let inner_limiter = PriorityBasedIORateLimiter::new();
        inner_limiter.set_bytes_per_sec(1024);
        let now = Instant::now();
        b.iter(|| {
            inner_limiter.critical_section(now);
        });
    }
}
