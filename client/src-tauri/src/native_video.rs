//! macOS 原生渲染层（只适配本机）：CAMetalLayer 盖在 WebView 之上，后续由 VideoToolbox 解码后直接上屏。
//!
//! 第一步只验证"视图 + 图层"：创建一个 CAMetalLayer 的 NSView 挂到窗口最上层，能按前端上报的矩形定位。
//! 解码（VTDecompressionSession）与 NV12 着色器在下一步接上。

#![cfg(target_os = "macos")]

use std::sync::atomic::{AtomicUsize, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, msg_send_id, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_foundation::{NSRect, NSSize, NSPoint};
use tauri::{AppHandle, Manager};

/// 原生视图的裸指针（0 = 没装）。只在主线程上用。
static VIEW: AtomicUsize = AtomicUsize::new(0);

/// 在窗口内容视图的最上层放一个 CAMetalLayer 视图。只做一次。
pub fn install(app: &AppHandle) -> Result<(), String> {
    // AppKit 的视图只能在主线程上创建
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = install_now(&handle) {
            tracing::warn!("native video: {e}");
        }
    })
    .map_err(|e| e.to_string())
}

fn install_now(app: &AppHandle) -> Result<(), String> {
    if VIEW.load(Ordering::Relaxed) != 0 {
        return Ok(());
    }
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let window = app.get_webview_window("main").ok_or("no main window")?;
    let ns_window = window.ns_window().map_err(|e| e.to_string())? as *mut AnyObject;
    if ns_window.is_null() {
        return Err("no NSWindow".into());
    }
    unsafe {
        let content: Retained<NSView> = msg_send_id![&*ns_window, contentView];
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let view: Retained<NSView> = msg_send_id![NSView::alloc(mtm), initWithFrame: frame];

        let cls = AnyClass::get(c"CAMetalLayer").ok_or("no CAMetalLayer class")?;
        let layer: Retained<AnyObject> = msg_send_id![cls, layer];
        let device: *mut AnyObject = MTLCreateSystemDefaultDevice();
        if device.is_null() {
            return Err("no Metal device".into());
        }
        let _: () = msg_send![&*layer, setDevice: device];
        let _: () = msg_send![&*layer, setPixelFormat: 80u64];
        let _: () = msg_send![&*layer, setFramebufferOnly: true];
        let _: () = msg_send![&*view, setWantsLayer: true];
        let _: () = msg_send![&*view, setLayer: &*layer];
        let _: () = msg_send![&*content, addSubview: &*view];

        // 超视图持有它；这里只记下裸指针
        VIEW.store(Retained::as_ptr(&view) as usize, Ordering::Relaxed);
    }
    Ok(())
}

/// 前端上报的画面矩形（窗口内 CSS 像素，左上原点）
pub fn set_rect(x: f64, y: f64, w: f64, h: f64, scale: f64) {
    let ptr = VIEW.load(Ordering::Relaxed);
    if ptr == 0 {
        return;
    }
    unsafe {
        let view = &*(ptr as *const NSView);
        let superview: Option<Retained<NSView>> = msg_send_id![view, superview];
        let height = superview.as_ref().map(|s| s.frame().size.height).unwrap_or(0.0);
        let frame = NSRect::new(NSPoint::new(x, height - (y + h)), NSSize::new(w, h));
        let _: () = msg_send![view, setFrame: frame];
        if let Some(layer) = view.layer() {
            let _: () = msg_send![&*layer, setDrawableSize: NSSize::new(w * scale, h * scale)];
        }
    }
}

/// 把原生视图拿掉（回退到 WebView 绘制）
pub fn remove() {
    let ptr = VIEW.swap(0, Ordering::Relaxed);
    if ptr != 0 {
        unsafe {
            let view = &*(ptr as *const NSView);
            let _: () = msg_send![view, removeFromSuperview];
        }
    }
}


// ───────────────────── 解码（VideoToolbox）─────────────────────

use std::ffi::c_void;
use std::os::raw::c_int;

type CFTypeRef = *const c_void;

#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    callback: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, u32, *mut c_void, *mut c_void),
    >,
    refcon: *mut c_void,
}

unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFTypeRef,
        count: usize,
        params: *const *const u8,
        sizes: *const usize,
        nal_len: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFTypeRef,
        memory: *mut c_void,
        block_len: usize,
        block_allocator: CFTypeRef,
        custom: CFTypeRef,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut *mut c_void,
    ) -> c_int;
    fn CMSampleBufferCreate(
        allocator: CFTypeRef,
        data_buffer: *mut c_void,
        data_ready: bool,
        make_data_ready: CFTypeRef,
        refcon: *mut c_void,
        format: *mut c_void,
        num_samples: usize,
        num_entries: usize,
        entries: *const c_void,
        sample_sizes: *const usize,
        sample_timing: *const c_void,
        out: *mut *mut c_void,
    ) -> c_int;
    fn VTDecompressionSessionCreate(
        allocator: CFTypeRef,
        fmt: *mut c_void,
        spec: CFTypeRef,
        attrs: CFTypeRef,
        cb: *const VTDecompressionOutputCallbackRecord,
        out: *mut *mut c_void,
    ) -> c_int;
    fn VTDecompressionSessionDecodeFrame(
        session: *mut c_void,
        sample: *mut c_void,
        flags: u32,
        info: *mut c_void,
        out_flags: *mut u32,
    ) -> c_int;
    fn CFRelease(obj: CFTypeRef);
}

static DECODER: std::sync::Mutex<Option<Decoder>> = std::sync::Mutex::new(None);

struct Decoder {
    session: *mut c_void,
    fmt: *mut c_void,
}

// VideoToolbox 的会话和格式描述可以从任意线程喂（解码回调在 VT 自己的线程上），
// 这里用 Mutex 串行化，指针本身不是线程安全的所以手动声明。
unsafe impl Send for Decoder {}

/// 把日志写到文件：以 .app 运行时看不到 stderr
pub fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/airhand-native.log")
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Annex-B → (SPS, PPS, 其余 NAL 按 4 字节长度前缀)
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > s {
                    nals.push(&data[s..end]);
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            nals.push(&data[s..]);
        }
    }
    nals
}

/// 喂一帧 Annex-B H.264 访问单元
pub fn feed_video(payload: &[u8]) {
    let nals = split_annexb(payload);
    if nals.is_empty() {
        return;
    }
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    let mut keep: Vec<&[u8]> = Vec::new();
    for n in &nals {
        match n[0] & 0x1f {
            7 => sps = Some(n),
            8 => pps = Some(n),
            9 => {}
            _ => keep.push(n),
        }
    }
    let mut guard = DECODER.lock().unwrap();
    if (sps.is_some() || pps.is_some()) && guard.is_none() {
        match unsafe { create_session(sps, pps) } {
            Ok(d) => {
                log(&format!("decoder created (sps={} pps={})", sps.map_or(0, |s| s.len()), pps.map_or(0, |s| s.len())));
                *guard = Some(d);
            }
            Err(e) => {
                log(&format!("can't create decoder: {e}"));
                return;
            }
        }
    }
    let Some(dec) = guard.as_mut() else { return };
    if keep.is_empty() {
        return;
    }
    let mut avcc = Vec::with_capacity(payload.len());
    for n in &keep {
        avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
        avcc.extend_from_slice(n);
    }
    unsafe {
        let mut block: *mut c_void = std::ptr::null_mut();
        let mut mem = avcc.clone().into_boxed_slice();
        let ptr = mem.as_mut_ptr();
        let len = mem.len();
        let status = CMBlockBufferCreateWithMemoryBlock(
            std::ptr::null(),
            ptr as *mut c_void,
            len,
            std::ptr::null(),
            std::ptr::null(),
            0,
            len,
            0,
            &mut block,
        );
        std::mem::forget(mem);
        if status != 0 {
            log(&format!("CMBlockBufferCreateWithMemoryBlock {status}"));
            return;
        }
        let mut sample: *mut c_void = std::ptr::null_mut();
        let sizes = [len];
        let status = CMSampleBufferCreate(
            std::ptr::null(),
            block,
            true,
            std::ptr::null(),
            std::ptr::null_mut(),
            dec.fmt,
            1,
            1,
            std::ptr::null(),
            sizes.as_ptr(),
            std::ptr::null(),
            &mut sample,
        );
        if status != 0 {
            log(&format!("CMSampleBufferCreate {status}"));
            CFRelease(block);
            return;
        }
        let mut flags: u32 = 0;
        let status = VTDecompressionSessionDecodeFrame(dec.session, sample, 0, std::ptr::null_mut(), &mut flags);
        if status != 0 {
            log(&format!("decode frame {status}"));
        }
        CFRelease(sample);
    }
}

unsafe fn create_session(sps: Option<&[u8]>, pps: Option<&[u8]>) -> Result<Decoder, String> {
    let (Some(sps), Some(pps)) = (sps, pps) else {
        return Err("missing parameter sets".into());
    };
    let params: [*const u8; 2] = [sps.as_ptr(), pps.as_ptr()];
    let sizes: [usize; 2] = [sps.len(), pps.len()];
    let mut fmt: *mut c_void = std::ptr::null_mut();
    let status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
        std::ptr::null(),
        2,
        params.as_ptr(),
        sizes.as_ptr(),
        4,
        &mut fmt,
    );
    if status != 0 {
        return Err(format!("format description {status}"));
    }
    let cb = VTDecompressionOutputCallbackRecord {
        callback: Some(output_callback),
        refcon: std::ptr::null_mut(),
    };
    let mut session: *mut c_void = std::ptr::null_mut();
    let status = VTDecompressionSessionCreate(
        std::ptr::null(),
        fmt,
        std::ptr::null(),
        std::ptr::null(),
        &cb,
        &mut session,
    );
    if status != 0 {
        CFRelease(fmt);
        return Err(format!("VTDecompressionSessionCreate {status}"));
    }
    Ok(Decoder { session, fmt })
}

/// 解码回调：拿到 CVPixelBuffer，下一步交给 Metal
unsafe extern "C" fn output_callback(
    _refcon: *mut c_void,
    _source: *mut c_void,
    status: c_int,
    _flags: u32,
    _image: *mut c_void,
    _pts: *mut c_void,
) {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 5 || n % 120 == 0 {
        log(&format!("decoded frame #{n} status={status}"));
    }
}

#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut AnyObject;
}
