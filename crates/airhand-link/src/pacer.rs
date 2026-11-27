//! 发送端节流（pacing）：把一帧的字节按当前发送速率摊开发出去，不再一次性灌进链路。
//!
//! quinn 0.11 只把 BBR 算出来的 pacing_rate 当指标暴露，发送循环并不消费它（见 quinn-proto 的
//! congestion/mod.rs：pacing_rate 只出现在 metrics 里），所以一个窗口的字节会在一个 RTT 的开头整批打出去。
//! 这里补一个应用层令牌桶：
//!
//! * 速率是自适应的：按最近实际发出去的字节数做 EWMA，所以它不会比应用本来发的更慢，只把整批发摊成匀速发；
//! * 桶容量只留 BURST_MS 毫秒的字节，突发被削掉；
//! * 单帧最多等 MAX_WAIT：大关键帧不能因为节流被拖到几百毫秒后（那比突发更糟）。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 桶里最多存这么多毫秒的字节（突发上限）
const BURST_MS: f64 = 15.0;
/// 单帧最多等这么久，超了就直接发
const MAX_WAIT: Duration = Duration::from_millis(40);
/// 速率的初始值（字节/秒）：8 Mbps，第一秒内靠观测纠正
const INITIAL_RATE: f64 = 8.0 * 125_000.0;
/// 观测窗口：这么久更新一次速率
const WINDOW: Duration = Duration::from_millis(250);

struct State {
    tokens: f64,
    last: Instant,
    rate: f64,
    window_start: Instant,
    window_bytes: f64,
}

/// 一条会话的发送节流器（复制出来的是同一个）
#[derive(Clone)]
pub struct Pacer(std::sync::Arc<Mutex<State>>);

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Pacer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self(std::sync::Arc::new(Mutex::new(State {
            tokens: 0.0,
            last: now,
            rate: INITIAL_RATE,
            window_start: now,
            window_bytes: 0.0,
        })))
    }

    /// 发 bytes 字节之前调用：等到令牌够（最多等 MAX_WAIT）。
    pub async fn acquire(&self, bytes: usize) {
        self.acquire_limited(bytes, None).await
    }

    /// 同上，但不超过连接自己算出来的发送速率（quinn 的 `pacing_rate`，字节/秒）：
    /// 应用层的 EWMA 只能看到自己发了多少，链路真正能发多快是 BBR 的判断。
    pub async fn acquire_limited(&self, bytes: usize, limit_bps: Option<f64>) {
        let wait = {
            let mut s = self.0.lock().unwrap();
            let now = Instant::now();
            let dt = now.duration_since(s.last).as_secs_f64();
            s.last = now;
            s.tokens = (s.tokens + dt * s.rate).min(s.rate * BURST_MS / 1000.0);
            s.window_bytes += bytes as f64;
            if now.duration_since(s.window_start) >= WINDOW {
                let elapsed = now.duration_since(s.window_start).as_secs_f64();
                let observed = s.window_bytes / elapsed.max(0.001);
                s.rate = (s.rate * 0.6 + observed * 0.4).clamp(125_000.0, 1.0e9);
                s.window_start = now;
                s.window_bytes = 0.0;
            }
            if let Some(limit) = limit_bps.filter(|l| *l > 1.0) {
                // 别比链路自己估计的还快（留 5% 余量）
                s.rate = s.rate.min(limit * 0.95).max(125_000.0);
            }
            let need = bytes as f64;
            if s.tokens >= need {
                s.tokens -= need;
                Duration::ZERO
            } else {
                let missing = need - s.tokens;
                s.tokens = 0.0;
                Duration::from_secs_f64(missing / s.rate.max(1.0))
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait.min(MAX_WAIT)).await;
        }
    }

    /// 现在的速率（字节/秒），给日志用
    pub fn rate(&self) -> f64 {
        self.0.lock().unwrap().rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_big_frame_waits_at_most_max_wait() {
        let p = Pacer::new();
        let t0 = Instant::now();
        p.acquire(100_000).await;
        assert!(t0.elapsed() <= MAX_WAIT + Duration::from_millis(20), "{:?}", t0.elapsed());
    }

    #[tokio::test]
    async fn small_frames_do_not_wait() {
        let p = Pacer::new();
        let t0 = Instant::now();
        for _ in 0..10 {
            p.acquire(1_000).await;
        }
        assert!(t0.elapsed() < Duration::from_millis(30));
    }
}
