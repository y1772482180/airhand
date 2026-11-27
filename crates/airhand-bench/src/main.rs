//! 无界面控制端：连上中转，按控制码连一台被控端，把每秒的统计写成 JSONL。
//!
//! 用来量网络层（直连建立、路径 RTT、卡顿率、码率），不渲染、不解码，只确认帧
//! （不确认的话被控端的在途窗口 32 帧就满了，量到的不是网络）。
//!
//! ```text
//! airhand-bench --server 1.2.3.4:4433 --code 123456 --seconds 60 --out run.jsonl
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use airhand_link::controller::{
    Command, ConnectOptions, ControllerService, Frontend, SessionEvent, SessionUi, UiClosed,
};
use airhand_link::net::{LinkConfig, ServerState, TrustStore};
use airhand_link::secure::{self, Identity};
use airhand_proto::media::Kind;
use airhand_proto::session::QualitySettings;
use anyhow::{Context, Result};
use serde_json::{Value, json};

struct Args {
    server: String,
    fp: String,
    code: String,
    seconds: u64,
    out: Option<PathBuf>,
    identity: PathBuf,
    pairing: Option<String>,
    host_key: Option<String>,
    fps: u8,
    kbps: u32,
    max_short: u16,
    audio: bool,
    label: String,
    /// 只看第 N 个显示器（0 = 主显示器）；不指定就是主显示器
    view: Option<u8>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        server: "127.0.0.1:4433".into(),
        fp: String::new(),
        code: String::new(),
        seconds: 30,
        out: None,
        identity: PathBuf::from("bench-identity.hex"),
        pairing: None,
        host_key: None,
        fps: 60,
        kbps: 8000,
        max_short: 1080,
        audio: true,
        label: "run".into(),
        view: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().context("missing value");
        match k.as_str() {
            "--server" => a.server = v()?,
            "--fp" => a.fp = v()?,
            "--code" => a.code = v()?,
            "--seconds" => a.seconds = v()?.parse()?,
            "--out" => a.out = Some(PathBuf::from(v()?)),
            "--identity" => a.identity = PathBuf::from(v()?),
            "--pairing" => a.pairing = Some(v()?),
            "--host-key" => a.host_key = Some(v()?),
            "--fps" => a.fps = v()?.parse()?,
            "--kbps" => a.kbps = v()?.parse()?,
            "--max-short" => a.max_short = v()?.parse()?,
            "--no-audio" => a.audio = false,
            "--view" => a.view = Some(v()?.parse()?),
            "--label" => a.label = v()?,
            "--print-key" => {
                let id = load_identity(&a.identity)?;
                println!("{}", secure::hex(id.public()));
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

/// 身份私钥存成一个文本文件（十六进制）。测试机专用，不做加密。
fn load_identity(path: &PathBuf) -> Result<Identity> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Some(raw) = secure::unhex32(text.trim()) {
            return Ok(Identity::from_private(raw));
        }
    }
    let id = Identity::generate();
    std::fs::write(path, secure::hex(id.private())).context("can't save the identity")?;
    Ok(id)
}

/// 服务器证书指纹：第一次连上记下来，之后只认它（和桌面版一样的 TOFU）
struct FileTrust(PathBuf);

impl TrustStore for FileTrust {
    fn load(&self, server: &str) -> Option<[u8; 32]> {
        let text = std::fs::read_to_string(&self.0).ok()?;
        let (s, fp) = text.split_once(' ')?;
        (s == server).then(|| secure::unhex32(fp.trim())).flatten()
    }

    fn save(&self, server: &str, fp: [u8; 32]) {
        let _ = std::fs::write(&self.0, format!("{server} {}\n", secure::hex(&fp)));
    }
}

/// 每秒的包间隔统计（画面帧到达到的间隔，超过 100 ms 算一次卡顿）
#[derive(Default)]
struct Gaps {
    last: Option<Instant>,
    gaps_ms: VecDeque<f64>,
}

struct Ui {
    out: Option<Arc<Mutex<std::fs::File>>>,
    label: String,
    started: Instant,
    service: OnceLock<Arc<ControllerService>>,
    sid: AtomicU64,
    packets: AtomicU64,
    gaps: Mutex<Gaps>,
    done: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Ui {
    fn emit(&self, v: Value) {
        let line = serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
        println!("{line}");
        if let Some(f) = &self.out {
            use std::io::Write;
            let mut f = f.lock().unwrap();
            let _ = writeln!(f, "{line}");
        }
    }
}

impl SessionUi for Ui {
    fn packet(&self, packet: Vec<u8>) -> Result<(), UiClosed> {
        let Some(kind) = Kind::of(&packet) else {
            return Ok(());
        };
        if kind != Kind::Video {
            return Ok(());
        }
        let seq = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        let stream = packet[12];
        self.packets.fetch_add(1, Ordering::Relaxed);
        {
            let mut g = self.gaps.lock().unwrap();
            let now = Instant::now();
            if let Some(prev) = g.last {
                g.gaps_ms.push_back(now.duration_since(prev).as_secs_f64() * 1000.0);
                if g.gaps_ms.len() > 600 {
                    g.gaps_ms.pop_front();
                }
            }
            g.last = Some(now);
        }
        if let Some(svc) = self.service.get()
            && let Ok(sid) = u32::try_from(self.sid.load(Ordering::Relaxed))
        {
            let _ = svc.command(Command::Ack { sid, seq, stream });
        }
        Ok(())
    }

    fn event(&self, event: SessionEvent) -> Result<(), UiClosed> {
        let t = self.started.elapsed().as_secs_f64();
        match event {
            SessionEvent::Hello {
                hostname,
                os,
                version,
                codecs,
                platform,
                ..
            } => self.emit(json!({
                "label": self.label, "t": t, "ev": "hello",
                "hostname": hostname, "os": os, "version": version,
                "codecs": format!("{codecs:?}"), "platform": format!("{platform:?}"),
            })),
            SessionEvent::Encoder {
                stream,
                codec,
                name,
                hardware,
                width,
                height,
            } => self.emit(json!({
                "label": self.label, "t": t, "ev": "encoder",
                "stream": stream, "codec": format!("{codec:?}"), "name": name,
                "hardware": hardware, "width": width, "height": height,
            })),
            SessionEvent::Stats {
                fps,
                kbps,
                rtt_ms,
                jitter_ms,
                stall_pct,
                width,
                height,
                frames,
                direct,
            } => {
                let (p95, max) = {
                    let mut g = self.gaps.lock().unwrap();
                    let mut v: Vec<f64> = g.gaps_ms.drain(..).collect();
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let p95 = if v.is_empty() {
                        0.0
                    } else {
                        v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)]
                    };
                    (p95, v.last().copied().unwrap_or(0.0))
                };
                self.emit(json!({
                    "label": self.label, "t": t, "ev": "stats",
                    "fps": fps, "kbps": kbps, "rtt_ms": rtt_ms, "jitter_ms": jitter_ms,
                    "stall_pct": stall_pct, "width": width, "height": height,
                    "frames": frames, "direct": direct,
                    "gap_p95_ms": (p95 * 10.0).round() / 10.0,
                    "gap_max_ms": (max * 10.0).round() / 10.0,
                    "packets": self.packets.load(Ordering::Relaxed),
                }));
            }
            SessionEvent::Notice { notice } => self.emit(json!({
                "label": self.label, "t": t, "ev": "notice", "notice": format!("{notice:?}"),
            })),
            SessionEvent::Closed { reason, retry } => {
                self.emit(json!({
                    "label": self.label, "t": t, "ev": "closed",
                    "reason": reason.to_string(), "retry": retry,
                }));
                let _ = self.done.send(());
            }
            other => self.emit(json!({
                "label": self.label, "t": t, "ev": "event", "event": format!("{other:?}").chars().take(200).collect::<String>(),
            })),
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,airhand_link=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let a = parse_args()?;
    let identity = load_identity(&a.identity)?;
    eprintln!(
        "bench: controller key {}",
        secure::hex(identity.public())
    );

    let trust_path = a.identity.with_extension("trust");
    let cfg = Arc::new(
        LinkConfig::new(&a.server, &a.fp, "airhand-bench", std::env::consts::OS)?
            .with_store(Arc::new(FileTrust(trust_path))),
    );

    let out = match &a.out {
        Some(p) => Some(Arc::new(Mutex::new(
            std::fs::File::create(p).with_context(|| format!("can't create {}", p.display()))?,
        ))),
        None => None,
    };

    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
    let ui = Arc::new(Ui {
        out,
        label: a.label.clone(),
        started: Instant::now(),
        service: OnceLock::new(),
        sid: AtomicU64::new(u32::MAX as u64),
        packets: AtomicU64::new(0),
        gaps: Mutex::default(),
        done: done_tx,
    });

    let connected = Arc::new(tokio::sync::Notify::new());
    let report_ui = ui.clone();
    let report_connected = connected.clone();
    let report = Arc::new(move |s: ServerState| {
        if matches!(s, ServerState::Connected) {
            report_connected.notify_waiters();
        }
        report_ui.emit(json!({
            "label": report_ui.label, "t": report_ui.started.elapsed().as_secs_f64(),
            "ev": "server", "state": format!("{s:?}"),
        }));
    });
    let online = Arc::new(|code: u32| eprintln!("bench: host {code} is online"));
    let service = Arc::new(ControllerService::start(
        cfg,
        tokio::runtime::Handle::current(),
        report,
        online,
        identity,
    ));
    let _ = ui.service.set(service.clone());

    let opts = ConnectOptions {
        resume: None,
        host_key: a.host_key.clone(),
        pairing: a.pairing.clone(),
        settings: QualitySettings {
            fps: a.fps,
            bitrate_kbps: a.kbps,
            max_short: a.max_short,
            adaptive: true,
            codec: airhand_proto::session::Codec::H264,
            draw_cursor: false,
        },
        audio: a.audio,
        view: a
            .view
            .map(|d| vec![airhand_proto::session::StreamView::display(d)])
            .unwrap_or_default(),
        privacy: false,
    };

    // 先等连上中转（服务在后台连，立刻 connect 会报"还没连上服务器"）
    if tokio::time::timeout(Duration::from_secs(15), connected.notified())
        .await
        .is_err()
    {
        ui.emit(json!({"label": a.label, "ev": "server_timeout"}));
        anyhow::bail!("the relay wasn't reachable within 15 s");
    }

    // 连上中转之前 connect 会报"还没连上服务器"：重试到超时，而不是一次就放弃
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(25);
    let info = loop {
        match service
            .connect(&a.code, opts.clone(), ui.clone(), Frontend::Browser)
            .await
        {
            Ok(i) => break i,
            Err(e) if Instant::now() < deadline => {
                ui.emit(json!({"label": a.label, "ev": "connect_retry", "error": e.to_string()}));
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            Err(e) => {
                ui.emit(json!({"label": a.label, "ev": "connect_failed", "error": e.to_string()}));
                anyhow::bail!("connect failed: {e}");
            }
        }
    };
    ui.sid.store(u64::from(info.sid), Ordering::Relaxed);
    ui.emit(json!({
        "label": a.label, "ev": "connected", "connect_ms": t0.elapsed().as_millis(),
        "sid": info.sid, "code": info.code, "host": info.host_name, "os": info.host_os,
        "safety": info.safety,
    }));

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(a.seconds)) => {}
        _ = done_rx.recv() => {}
    }

    let _ = service.command(Command::Disconnect { sid: info.sid });
    ui.emit(json!({
        "label": a.label, "ev": "done", "elapsed_s": ui.started.elapsed().as_secs_f64(),
        "packets": ui.packets.load(Ordering::Relaxed),
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}
