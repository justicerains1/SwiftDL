//! 全局令牌桶限速：允许短暂“欠账”，按欠账时长休眠，开销极低。
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Limiter {
    rate: AtomicU64, // 字节/秒，0 = 不限速
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), state: Mutex::new((0.0, Instant::now())) }
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
        *self.state.lock().unwrap() = (0.0, Instant::now());
    }

    pub fn limited(&self) -> bool {
        self.rate.load(Ordering::Relaxed) != 0
    }

    pub async fn acquire(&self, n: usize) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 || n == 0 {
            return;
        }
        let wait = {
            let mut s = self.state.lock().unwrap();
            let now = Instant::now();
            let rate = rate as f64;
            // 补充令牌，最多积攒 0.25 秒的突发量
            s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * rate).min(rate / 4.0);
            s.1 = now;
            s.0 -= n as f64;
            if s.0 >= 0.0 { 0.0 } else { -s.0 / rate }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}
