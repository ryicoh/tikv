// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use super::metrics::RATE_LIMITER_REQUEST_WAIT_DURATION;
use super::{IOOp, IOPriority, IOType};

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use crossbeam_utils::CachePadded;
use parking_lot::Mutex;
use strum::EnumCount;
use tikv_util::time::Instant;
use tikv_util::worker::Worker;

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

const DEFAULT_REFILL_PERIOD: Duration = Duration::from_millis(30);

#[inline]
fn calculate_ios_per_refill(ios_per_sec: usize, refill_period: Duration) -> usize {
    (ios_per_sec as f64 * refill_period.as_secs_f64()) as usize
}

/// Limit total IO flow below provided threshold by throttling low-priority IOs.
/// Rate limit is disabled when total IO threshold is set to zero.
#[derive(Debug)]
struct RawIORateLimiter {
    // IO amount passed through within current epoch
    ios_through: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    // Maximum IOs permitted within current epoch
    ios_per_epoch: [CachePadded<AtomicUsize>; IOPriority::COUNT],
    protected: Mutex<RawIORateLimiterProtected>,
}

#[derive(Debug, Copy, Clone)]
struct IOThroughputEstimator {
    count: u64,
    long_max: usize,
    short_sum: usize,
}

impl IOThroughputEstimator {
    fn new() -> IOThroughputEstimator {
        IOThroughputEstimator {
            count: 0,
            long_max: 0,
            short_sum: 0,
        }
    }

    fn maybe_update_estimation(&mut self, v: usize) -> Option<usize> {
        self.count += 1;
        self.short_sum += v;
        // smoothen raw samples
        if self.count % 5 == 0 {
            self.long_max = std::cmp::max(self.long_max, self.short_sum / 5);
            self.short_sum = 0;
        }
        // use maximum throughput of history refills to update limits
        // second predicate makes unit tests happy
        if self.count % 200 == 0 || (self.count < 200 && self.count % 5 == 0) {
            let max = self.long_max;
            self.long_max = 0;
            self.short_sum = 0;
            Some(max)
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct RawIORateLimiterProtected {
    next_refill_time: Instant,
    // IO amount that is drew from the next epoch in advance
    pending_ios: [usize; IOPriority::COUNT],
    estimated_ios_through: [IOThroughputEstimator; IOPriority::COUNT],
}

impl RawIORateLimiterProtected {
    fn new() -> Self {
        RawIORateLimiterProtected {
            next_refill_time: Instant::now_coarse() + DEFAULT_REFILL_PERIOD,
            pending_ios: [0; IOPriority::COUNT],
            estimated_ios_through: [IOThroughputEstimator::new(); IOPriority::COUNT],
        }
    }
}

macro_rules! sleep_impl {
    ($duration:expr, "sync") => {
        std::thread::sleep($duration)
    };
    ($duration:expr, "async") => {
        tokio::time::delay_for($duration).await
    };
}

/// Actual implementation for requesting IOs from RawIORateLimiter.
/// An attempt will be recorded first. If the attempted amount exceeds the available quotas of
/// current epoch, the requester will register itself for next epoch and sleep until next epoch.
macro_rules! request_impl {
    ($self:ident, $priority:ident, $amount:ident, $mode:tt) => {{
        let priority_idx = $priority as usize;
        loop {
            let cached_ios_per_refill = $self.ios_per_epoch[priority_idx].load(Ordering::Relaxed);
            if cached_ios_per_refill == 0 {
                return $amount;
            }
            let amount = std::cmp::min($amount, cached_ios_per_refill);
            let ios_through =
                $self.ios_through[priority_idx].fetch_add(amount, Ordering::AcqRel) + amount;
            if ios_through <= cached_ios_per_refill {
                return amount;
            }
            let now = Instant::now_coarse();
            let (next_refill_time, pending) = {
                let mut locked = $self.protected.lock();
                // a small delay in case a refill slips in after `ios_per_epoch` was fetched.
                if locked.next_refill_time + Duration::from_millis(1) >= now + DEFAULT_REFILL_PERIOD
                {
                    continue;
                }
                locked.pending_ios[priority_idx] += amount;
                (locked.next_refill_time, locked.pending_ios[priority_idx])
            };
            if next_refill_time > now {
                let wait = next_refill_time - now;
                RATE_LIMITER_REQUEST_WAIT_DURATION
                    .with_label_values(&[$priority.as_str()])
                    .observe(wait.as_secs_f64());
                sleep_impl!(wait, $mode);
            }
            // our request will be granted in `refill()`
            if pending <= cached_ios_per_refill {
                return amount;
            }
        }
    }};
}

impl RawIORateLimiter {
    fn new() -> Self {
        RawIORateLimiter {
            ios_through: Default::default(),
            ios_per_epoch: Default::default(),
            protected: Mutex::new(RawIORateLimiterProtected::new()),
        }
    }

    /// Dynamically changes the total IO flow threshold, effective after at most
    /// `DEFAULT_REFILL_PERIOD`.
    #[allow(dead_code)]
    fn set_ios_per_sec(&self, ios_per_sec: usize) {
        let now = calculate_ios_per_refill(ios_per_sec, DEFAULT_REFILL_PERIOD);
        let before = self.ios_per_epoch[IOPriority::High as usize].swap(now, Ordering::Relaxed);
        if before == 0 || now == 0 {
            // toggle on/off rate limit.
            // we hold this lock so a concurrent refill can't negate our effort.
            let _locked = self.protected.lock();
            for p in &[IOPriority::Medium, IOPriority::Low] {
                let pi = *p as usize;
                self.ios_per_epoch[pi].store(now, Ordering::Relaxed);
            }
        }
    }

    fn request(&self, priority: IOPriority, amount: usize) -> usize {
        request_impl!(self, priority, amount, "sync")
    }

    async fn async_request(&self, priority: IOPriority, amount: usize) -> usize {
        request_impl!(self, priority, amount, "async")
    }

    /// Called by a daemon thread every `DEFAULT_REFILL_PERIOD`.
    /// It is done so because the algorithm correctness relies on refill epoch being
    /// faithful to physical time.
    fn refill(&self) {
        let mut locked = self
            .protected
            .try_lock_for(DEFAULT_REFILL_PERIOD)
            .expect("Can't acquire lock to refill IO rate limiter");

        if self.ios_per_epoch[IOPriority::High as usize].load(Ordering::Relaxed) == 0 {
            return;
        }
        locked.next_refill_time = Instant::now_coarse() + DEFAULT_REFILL_PERIOD;

        assert!(IOPriority::High as usize > IOPriority::Medium as usize);
        for p in &[IOPriority::High, IOPriority::Medium] {
            let pi = *p as usize;
            let limit = self.ios_per_epoch[pi].load(Ordering::Relaxed);
            let ios_through = std::cmp::min(
                self.ios_through[pi].swap(locked.pending_ios[pi], Ordering::Release),
                limit,
            );
            if let Some(ios_through) =
                locked.estimated_ios_through[pi].maybe_update_estimation(ios_through)
            {
                self.ios_per_epoch[pi - 1].store(
                    if limit > ios_through {
                        limit - ios_through
                    } else {
                        1 // a small positive value
                    },
                    Ordering::Relaxed,
                );
            }
            locked.pending_ios[pi] = 0;
        }
    }
}

/// An instance of `IORateLimiter` should be safely shared between threads.
#[derive(Debug)]
pub struct IORateLimiter {
    priority_map: [IOPriority; IOType::COUNT],
    throughput_limiter: Arc<RawIORateLimiter>,
    stats: Option<Arc<IORateLimiterStatistics>>,
}

impl IORateLimiter {
    pub fn new(enable_statistics: bool) -> IORateLimiter {
        IORateLimiter {
            priority_map: [IOPriority::High; IOType::COUNT],
            throughput_limiter: Arc::new(RawIORateLimiter::new()),
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

    #[allow(dead_code)]
    pub fn set_io_rate_limit(&self, rate: usize) {
        self.throughput_limiter.set_ios_per_sec(rate);
    }

    pub fn refill(&self) {
        self.throughput_limiter.refill();
    }

    /// Requests for token for bytes and potentially update statistics. If this
    /// request can not be satisfied, the call is blocked. Granted token can be
    /// less than the requested bytes, but must be greater than zero.
    pub fn request(&self, io_type: IOType, io_op: IOOp, mut bytes: usize) -> usize {
        if io_op == IOOp::Write || io_type == IOType::Export {
            let priority = self.priority_map[io_type as usize];
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
        if io_op == IOOp::Write || io_type == IOType::Export {
            let priority = self.priority_map[io_type as usize];
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

// Do NOT use this method in multi-threaded test environment.
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

pub fn start_io_rate_limiter_daemon(worker: &Worker) {
    worker.spawn_interval_task(DEFAULT_REFILL_PERIOD, move || {
        if let Some(limiter) = get_io_rate_limiter() {
            limiter.refill();
        }
    });
}

pub struct IORateLimiterDaemon {
    _thread: std::thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl Drop for IORateLimiterDaemon {
    fn drop(&mut self) {
        self.stop.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub fn new_io_rate_limiter_daemon(limiter: Arc<IORateLimiter>) -> IORateLimiterDaemon {
    let stop = Arc::new(AtomicBool::new(false));
    let stop1 = stop.clone();
    IORateLimiterDaemon {
        _thread: std::thread::spawn(move || {
            while !stop1.load(Ordering::Relaxed) {
                limiter.refill();
                std::thread::sleep(DEFAULT_REFILL_PERIOD);
            }
        }),
        stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

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

    #[derive(Clone, Copy)]
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
        let _deamon = new_io_rate_limiter_daemon(limiter.clone());
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
        let _deamon = new_io_rate_limiter_daemon(limiter.clone());
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
        let _deamon = new_io_rate_limiter_daemon(limiter.clone());
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
}
