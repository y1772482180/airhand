//! UDP 损伤代理：把 listen 上的包转发到 target，按参数注入延迟、抖动、丢包和带宽上限。
//! 两个方向都经过同一个管道，用来复现"弱网"（Wi-Fi、跨境、拥塞）。
//!
//! ```text
//! udp-impair --listen 0.0.0.0:4433 --target 127.0.0.1:4434 --delay-ms 40 --jitter-ms 10 --loss 0.01 --kbps 5000
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

struct Args {
    listen: SocketAddr,
    target: SocketAddr,
    delay: Duration,
    jitter: Duration,
    loss: f64,
    kbps: u64,
}

fn parse_args() -> Result<Args> {
    let mut listen: SocketAddr = "0.0.0.0:4433".parse()?;
    let mut target: SocketAddr = "127.0.0.1:4434".parse()?;
    let mut delay = Duration::ZERO;
    let mut jitter = Duration::ZERO;
    let mut loss = 0.0;
    let mut kbps = 0u64;
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().context("missing value");
        match k.as_str() {
            "--listen" => listen = v()?.parse()?,
            "--target" => target = v()?.parse()?,
            "--delay-ms" => delay = Duration::from_millis(v()?.parse()?),
            "--jitter-ms" => jitter = Duration::from_millis(v()?.parse()?),
            "--loss" => loss = v()?.parse()?,
            "--kbps" => kbps = v()?.parse()?,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(Args {
        listen,
        target,
        delay,
        jitter,
        loss,
        kbps,
    })
}

/// xorshift：够用的伪随机，避免为测试引一个 rand 依赖
struct Rng(AtomicU64);

impl Rng {
    fn new() -> Self {
        Self(AtomicU64::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
                | 1,
        ))
    }
    fn next(&self) -> u64 {
        let mut x = self.0.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0.store(x, Ordering::Relaxed);
        x
    }
    fn unit(&self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

struct Line {
    sock: Arc<UdpSocket>,
    bucket: Mutex<Bucket>,
}

impl Line {
    /// 令牌桶：每秒补 kbps*125 字节。放不下就丢（模拟带宽上限）
    fn admit(&self, len: usize, kbps: u64) -> bool {
        if kbps == 0 {
            return true;
        }
        let rate = kbps as f64 * 125.0;
        let mut b = self.bucket.lock().unwrap();
        let now = Instant::now();
        let dt = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * rate).min(rate);
        b.last = now;
        if b.tokens >= len as f64 {
            b.tokens -= len as f64;
            true
        } else {
            false
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = parse_args()?;
    let front = Arc::new(UdpSocket::bind(a.listen).await.context("bind listen")?);
    eprintln!(
        "impair: {} -> {} delay={:?} jitter={:?} loss={} kbps={}",
        a.listen, a.target, a.delay, a.jitter, a.loss, a.kbps
    );

    let forwarded = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let rng = Arc::new(Rng::new());
    let lines: Arc<tokio::sync::Mutex<HashMap<SocketAddr, Arc<Line>>>> = Arc::default();
    let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<u8>, Arc<UdpSocket>, SocketAddr, Instant)>();

    // 延迟队列
    tokio::spawn(async move {
        while let Some((data, sock, to, due)) = rx.recv().await {
            let now = Instant::now();
            if due > now {
                tokio::time::sleep(due - now).await;
            }
            let _ = sock.send_to(&data, to).await;
        }
    });

    // 前端 → target
    {
        let front = front.clone();
        let lines = lines.clone();
        let tx = tx.clone();
        let rng = rng.clone();
        let forwarded = forwarded.clone();
        let dropped = dropped.clone();
        let (target, loss, delay, jitter, kbps) = (a.target, a.loss, a.delay, a.jitter, a.kbps);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let Ok((n, from)) = front.recv_from(&mut buf).await else {
                    continue;
                };
                let line = {
                    let mut m = lines.lock().await;
                    match m.get(&from) {
                        Some(l) => l.clone(),
                        None => {
                            let s = Arc::new(UdpSocket::bind("0.0.0.0:0").await.unwrap());
                            s.connect(target).await.ok();
                            let l = Arc::new(Line {
                                sock: s,
                                bucket: Mutex::new(Bucket {
                                    tokens: kbps as f64 * 125.0,
                                    last: Instant::now(),
                                }),
                            });
                            m.insert(from, l.clone());
                            l
                        }
                    }
                };
                if !line.admit(n, kbps) || (loss > 0.0 && rng.unit() < loss) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                forwarded.fetch_add(1, Ordering::Relaxed);
                let extra = if jitter.is_zero() {
                    Duration::ZERO
                } else {
                    Duration::from_micros((rng.next() % (jitter.as_micros().max(1) as u64)) as u64)
                };
                let _ = tx.send((buf[..n].to_vec(), line.sock.clone(), target, Instant::now() + delay + extra));
            }
        });
    }

    // target → 前端（每个客户端 socket 上收）
    {
        let lines = lines.clone();
        let front = front.clone();
        let tx = tx.clone();
        let rng = rng.clone();
        let forwarded = forwarded.clone();
        let dropped = dropped.clone();
        let (loss, delay, jitter) = (a.loss, a.delay, a.jitter);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                let snapshot: Vec<(SocketAddr, Arc<Line>)> = {
                    let m = lines.lock().await;
                    m.iter().map(|(k, v)| (*k, v.clone())).collect()
                };
                if snapshot.is_empty() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    continue;
                }
                for (client, line) in snapshot {
                    let got = tokio::time::timeout(
                        Duration::from_millis(2),
                        line.sock.recv_from(&mut buf),
                    )
                    .await;
                    let Ok(Ok((n, _))) = got else { continue };
                    if loss > 0.0 && rng.unit() < loss {
                        dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    forwarded.fetch_add(1, Ordering::Relaxed);
                    let extra = if jitter.is_zero() {
                        Duration::ZERO
                    } else {
                        Duration::from_micros((rng.next() % (jitter.as_micros().max(1) as u64)) as u64)
                    };
                    let _ = tx.send((buf[..n].to_vec(), front.clone(), client, Instant::now() + delay + extra));
                }
            }
        });
    }

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        eprintln!(
            "impair: forwarded={} dropped={}",
            forwarded.load(Ordering::Relaxed),
            dropped.load(Ordering::Relaxed)
        );
    }
}
