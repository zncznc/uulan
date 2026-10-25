//! X11 剪贴板后端（被控端在麒麟 X11 上用；Linux 控制端也用它）。
//!
//! 直接声明 libX11 的符号，理由同 [`crate`] 里说的：**不引第三方 crate**，
//! 而这个文件用到的全是 1990 年代的核心符号，麒麟 V10 上的 libX11 一定有。
//!
//! ## 为什么不能「读一下就完了」
//!
//! X11 的剪贴板不是一块内存，而是一次**没有通知的跨进程协商**：
//! 所有者（比如浏览器）拿着 selection，你要数据就得 `XConvertSelection` 发请求，
//! 它把数据写成你窗口上的一个属性，再回一个 `SelectionNotify`。于是：
//!
//! - 读取必然是**轮询**（X 没有「剪贴板变了」的广播）；
//! - 所有者可能**根本不回**（进程卡死、Wayland 门面、被 `xclip` 抢走后退出）→ 必须有超时；
//! - 内容可能很大，所有者会用 `INCR` 协议分段送 → 必须按 `PropertyNotify` 一段段收。
//!
//! ## 写入必须「当上所有者」
//!
//! `set_text` 之后，别的程序来取数据时**由我们的进程回答** —— 所以必须有一个
//! 一直在跑的线程守着 X 事件。这就是本文件里那个 service 线程的作用：
//! 它独占第二条 `Display*` 连接，拥有一个 1x1 的隐藏窗口作为所有者，
//! 收到 `SelectionRequest` 就把文本填进请求者的属性里。
//!
//! 会话结束时线程退出、连接关闭，剪贴板内容随之失效 —— 这是 X11 的固有行为
//! （所有者进程死了内容就没了），不是我们的缺陷，文档里写清楚了。

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info};

use super::{Clipboard, MAX_TEXT_BYTES};

/// 读一次剪贴板最多等多久。所有者不回话（进程卡死）时必须能放弃，
/// 否则一次轮询就把会话的主循环拖住。
const READ_TIMEOUT: Duration = Duration::from_millis(400);
/// INCR 分段接收的整体超时。
const INCR_TIMEOUT: Duration = Duration::from_millis(1500);
/// service 线程的事件轮询间隔。20ms 是「人感觉不到」与「不空转 CPU」的折中。
const SERVICE_TICK: Duration = Duration::from_millis(20);

pub enum Display {}
type Window = c_ulong;
type Atom = c_ulong;

const CURRENT_TIME: c_ulong = 0;
const PROPERTY_CHANGE_MASK: c_long = 1 << 22;

const EV_PROPERTY_NOTIFY: c_int = 28;
const EV_SELECTION_CLEAR: c_int = 29;
const EV_SELECTION_REQUEST: c_int = 30;
const EV_SELECTION_NOTIFY: c_int = 31;

const XA_ATOM: Atom = 4;
const XA_STRING: Atom = 31;
const PROP_MODE_REPLACE: c_int = 0;

/// `XEvent` 在 64 位上是 24 个 `long`（192 字节）。按这个大小开缓冲区，
/// 再按事件类型解释成具体的结构体 —— 这是 Xlib 的常规用法。
#[repr(C, align(8))]
struct XEventBuf([u64; 24]);

impl XEventBuf {
    fn zeroed() -> Self {
        Self([0; 24])
    }

    fn kind(&self) -> c_int {
        self.0[0] as c_int
    }
}

/// `XSelectionRequestEvent` —— **服务端**（我们是所有者）收到的事件。
///
/// 比 `SelectionNotify` 多一个 `owner` 字段（在 `display` 之后）。
#[repr(C)]
struct SelectionRequestEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    owner: Window,
    requestor: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: c_ulong,
}

/// `XSelectionEvent`（`SelectionNotify`）—— **请求端**（我们去要数据）收到的事件。
///
/// ⚠ 这个结构体**没有** `owner` 字段。以前它和上面那个共用一个"owner_or_requestor"
/// 的坑位，结果在 notify 上所有字段整体错位一格：`selection` 读到的其实是 `target`、
/// `property` 读到的其实是 `time`。表现是**两头都像卡死**：
/// 读别人剪贴板永远超时（把 target 当 selection 一比就不等）、
/// 别人读我们的永远等不到回音（我们发出去的 notify 里 selection 是窗口号）。
/// 这个 bug 只有在真机上、且对面是**别的程序**时才会暴露 —— 自己读自己走的是
/// `own_text` 缓存，根本不经过 X 协议。
#[repr(C)]
struct SelectionNotifyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    requestor: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: c_ulong,
}

/// `XSelectionClearEvent` —— 别人把 selection 抢走了。
#[repr(C)]
struct SelectionClearEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: Window,
    selection: Atom,
    time: c_ulong,
}

#[repr(C)]
struct PropertyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: Window,
    atom: Atom,
    time: c_ulong,
    state: c_int,
}

#[link(name = "X11")]
extern "C" {
    fn XInitThreads() -> c_int;
    fn XOpenDisplay(name: *const c_char) -> *mut Display;
    fn XCloseDisplay(dpy: *mut Display) -> c_int;
    fn XFlush(dpy: *mut Display) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XDefaultRootWindow(dpy: *mut Display) -> Window;
    fn XCreateSimpleWindow(
        dpy: *mut Display,
        parent: Window,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        border_width: c_uint,
        border: c_ulong,
        background: c_ulong,
    ) -> Window;
    fn XDestroyWindow(dpy: *mut Display, w: Window) -> c_int;
    fn XSelectInput(dpy: *mut Display, w: Window, mask: c_long) -> c_int;
    fn XInternAtom(dpy: *mut Display, name: *const c_char, only_if_exists: c_int) -> Atom;
    fn XSetSelectionOwner(dpy: *mut Display, selection: Atom, owner: Window, time: c_ulong) -> c_int;
    fn XGetSelectionOwner(dpy: *mut Display, selection: Atom) -> Window;
    fn XConvertSelection(
        dpy: *mut Display,
        selection: Atom,
        target: Atom,
        property: Atom,
        requestor: Window,
        time: c_ulong,
    ) -> c_int;
    fn XNextEvent(dpy: *mut Display, event: *mut c_void) -> c_int;
    fn XPending(dpy: *mut Display) -> c_int;
    fn XSendEvent(
        dpy: *mut Display,
        w: Window,
        propagate: c_int,
        event_mask: c_long,
        event: *mut c_void,
    ) -> c_int;
    fn XChangeProperty(
        dpy: *mut Display,
        w: Window,
        property: Atom,
        type_: Atom,
        format: c_int,
        mode: c_int,
        data: *const c_uchar,
        nelements: c_int,
    ) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn XGetWindowProperty(
        dpy: *mut Display,
        w: Window,
        property: Atom,
        long_offset: c_long,
        long_length: c_long,
        delete: c_int,
        req_type: Atom,
        actual_type: *mut Atom,
        actual_format: *mut c_int,
        nitems: *mut c_ulong,
        bytes_after: *mut c_ulong,
        prop: *mut *mut c_uchar,
    ) -> c_int;
    fn XDeleteProperty(dpy: *mut Display, w: Window, property: Atom) -> c_int;
}

fn intern(dpy: *mut Display, name: &str) -> Atom {
    let c = CString::new(name).expect("atom 名不含 NUL");
    unsafe { XInternAtom(dpy, c.as_ptr(), 0) }
}

/// 让 Xlib 变成线程安全的（进程内只需一次，必须早于**任何** Xlib 调用）。
///
/// 为什么必须做：被控端允许并发会话。上一条会话在别的线程上收尾（`XCloseDisplay`、
/// `XChangeKeyboardMapping` 还原映射），下一条已经在开新连接（`XTestQueryExtension`）——
/// 没调 `XInitThreads` 时 Xlib 没有内部锁，两个线程同时进 Xlib 是未定义行为。
/// 真机实测过后果：`free(): invalid pointer` → SIGABRT，systemd 把被控端整个拉起来
/// （NRestarts +1，控制端那一头看到的就是「接回失败」）。
///
/// 放在 `uulan-clip` 里，是因为被控端与控制端都依赖它、且两边都要开 X 显示；
/// 每个 crate 各放一个 `Once` 的话，两个 `Once` 之间仍可能并发进 Xlib。
pub fn init_xlib() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 返回值 0 = 这个 Xlib 实现没有线程锁（老到不该出现在现场）。
        if unsafe { XInitThreads() } == 0 {
            info!("XInitThreads 返回 0：Xlib 未启用线程锁，并发会话下可能不安全");
        }
    });
}

struct Atoms {
    clipboard: Atom,
    primary: Atom,
    utf8: Atom,
    text: Atom,
    targets: Atom,
    incr: Atom,
    prop: Atom,
}

impl Atoms {
    fn intern_all(dpy: *mut Display) -> Self {
        Self {
            clipboard: intern(dpy, "CLIPBOARD"),
            primary: intern(dpy, "PRIMARY"),
            utf8: intern(dpy, "UTF8_STRING"),
            text: intern(dpy, "TEXT"),
            targets: intern(dpy, "TARGETS"),
            incr: intern(dpy, "INCR"),
            // 我们自己的属性名：别用通用的 "SELECTION" 之类，免得和别人的窗口撞名。
            prop: intern(dpy, "UULAN_CLIP"),
        }
    }
}

pub struct X11Clipboard {
    dpy: *mut Display,
    win: Window,
    atoms: Atoms,
    /// 本进程当前「应该」是剪贴板内容。读自己写的内容时直接返回它，
    /// 不去走一遍 X 协商（既快，也避免自己等自己）。
    own_text: Arc<Mutex<Option<String>>>,
    /// service 线程当所有者用的窗口号。0 表示「现在不是我们持有」。
    owner_win: Arc<AtomicU64>,
    tx: Sender<OwnerCmd>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum OwnerCmd {
    /// 当上所有者，并用 ack 告诉调用方「已经生效」。
    Own(Sender<()>),
}

// Xlib 的 `Display*` 不是线程安全的，但这个结构体**只被一个任务使用**（会话主循环），
// service 线程用的是它自己另开的连接。跨线程传递的只有 Arc<Mutex<String>> 与 channel。
unsafe impl Send for X11Clipboard {}

impl X11Clipboard {
    /// 连上 X 显示（`None` = `$DISPLAY`），准备读写剪贴板。
    pub fn open(display: Option<&str>) -> Result<Self, String> {
        init_xlib();
        let cname = match display {
            Some(s) if !s.is_empty() => {
                Some(CString::new(s).map_err(|e| format!("显示名 {s:?} 含非法字符：{e}"))?)
            }
            _ => None,
        };
        let dpy = unsafe {
            XOpenDisplay(match &cname {
                Some(c) => c.as_ptr(),
                None => std::ptr::null(),
            })
        };
        if dpy.is_null() {
            return Err(format!(
                "打不开 X 显示 {}（剪贴板同步需要 X11 会话）",
                display.unwrap_or("$DISPLAY")
            ));
        }

        let atoms = Atoms::intern_all(dpy);
        if atoms.clipboard == 0 || atoms.utf8 == 0 {
            unsafe { XCloseDisplay(dpy) };
            return Err("X 服务器没定义 CLIPBOARD/UTF8_STRING 原子".to_string());
        }

        let root = unsafe { XDefaultRootWindow(dpy) };
        // 1x1 且从不 map：只是拿来做 selection 的 requestor / 属性宿主。
        let win = unsafe { XCreateSimpleWindow(dpy, root, -10, -10, 1, 1, 0, 0, 0) };
        if win == 0 {
            unsafe { XCloseDisplay(dpy) };
            return Err("创建剪贴板窗口失败".to_string());
        }
        unsafe {
            XSelectInput(dpy, win, PROPERTY_CHANGE_MASK);
            XFlush(dpy);
        }

        let own_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let owner_win = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<OwnerCmd>();

        let thread = {
            let own_text = own_text.clone();
            let owner_win = owner_win.clone();
            let stop = stop.clone();
            let name = display.unwrap_or("$DISPLAY").to_string();
            std::thread::Builder::new()
                .name("uulan-x11-clip".to_string())
                .spawn(move || service_thread(name, own_text, owner_win, stop, rx))
                .map_err(|e| format!("起剪贴板 service 线程失败：{e}"))?
        };

        let shown_display = display.unwrap_or("$DISPLAY");
        info!(display_name = %shown_display, "X11 剪贴板后端已就绪");
        Ok(Self {
            dpy,
            win,
            atoms,
            own_text,
            owner_win,
            tx,
            stop,
            thread: Some(thread),
        })
    }

    /// 现在 CLIPBOARD 是不是我们持有。
    fn owns_clipboard(&self) -> bool {
        let ours = self.owner_win.load(Ordering::Relaxed);
        if ours == 0 {
            return false;
        }
        unsafe { XGetSelectionOwner(self.dpy, self.atoms.clipboard) == ours }
    }

    fn next_event(&self, buf: &mut XEventBuf) -> bool {
        unsafe { XNextEvent(self.dpy, buf as *mut XEventBuf as *mut c_void) == 0 }
    }

    /// 取走窗口上的属性（`delete` 决定是否顺手删掉），返回 `(实际类型, 字节)`。
    fn take_property(&self, delete: bool) -> Option<(Atom, Vec<u8>)> {
        let mut actual_type: Atom = 0;
        let mut actual_format: c_int = 0;
        let mut nitems: c_ulong = 0;
        let mut bytes_after: c_ulong = 0;
        let mut prop: *mut c_uchar = std::ptr::null_mut();
        let rc = unsafe {
            XGetWindowProperty(
                self.dpy,
                self.win,
                self.atoms.prop,
                0,
                0x1fff_ffff, // 让 Xlib 按属性真实大小截断
                if delete { 1 } else { 0 },
                0, // AnyPropertyType
                &mut actual_type,
                &mut actual_format,
                &mut nitems,
                &mut bytes_after,
                &mut prop,
            )
        };
        if rc != 0 || prop.is_null() {
            return None;
        }
        // format 8/16/32 都是「每单元位数」，字节数 = nitems * format / 8。
        let unit = (actual_format.max(0) as usize) / 8;
        let n = if unit == 0 { 0 } else { (nitems as usize) * unit };
        let data = if n == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(prop, n).to_vec() }
        };
        unsafe { XFree(prop as *mut c_void) };
        Some((actual_type, data))
    }

    /// 等一个针对我们窗口、属性为 `prop` 的 `PropertyNotify`（INCR 分段用）。
    fn wait_property_new_value(&self, deadline: Instant) -> bool {
        while Instant::now() < deadline {
            if unsafe { XPending(self.dpy) } <= 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut ev = XEventBuf::zeroed();
            if !self.next_event(&mut ev) {
                return false;
            }
            if ev.kind() != EV_PROPERTY_NOTIFY {
                continue;
            }
            let pe = unsafe { &*(ev.0.as_ptr() as *const PropertyEvent) };
            if pe.window == self.win && pe.atom == self.atoms.prop && pe.state == 0 {
                return true;
            }
        }
        false
    }

    /// 把 `SelectionNotify` 之后的数据读完（含 INCR 分段）。
    fn read_selection_data(&mut self) -> Option<String> {
        let (ty, data) = self.take_property(false)?;
        if ty != self.atoms.incr {
            return String::from_utf8(data).ok().filter(|s| !s.is_empty());
        }

        // INCR：先删掉属性表示「收到了，继续发」，然后一段段收，收到空段为止。
        unsafe { XDeleteProperty(self.dpy, self.win, self.atoms.prop) };
        let deadline = Instant::now() + INCR_TIMEOUT;
        let mut all: Vec<u8> = Vec::new();
        loop {
            if !self.wait_property_new_value(deadline) {
                debug!("INCR 分段接收超时，返回已收到的部分");
                break;
            }
            let Some((_ty, chunk)) = self.take_property(true) else { break };
            if chunk.is_empty() {
                break;
            }
            all.extend_from_slice(&chunk);
            if all.len() > MAX_TEXT_BYTES {
                // 超限就地停：上层 ClipSync 还会再判一次，这里只是别把内存吃光。
                debug!(bytes = all.len(), "剪贴板内容超过上限，停止接收");
                break;
            }
        }
        if all.is_empty() {
            return None;
        }
        String::from_utf8_lossy(&all).into_owned().into()
    }
}

impl Clipboard for X11Clipboard {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn available(&self) -> bool {
        true
    }

    fn get_text(&mut self) -> Option<String> {
        // 自己刚写进去的：直接给，不必绕 X 一圈。
        if self.owns_clipboard() {
            if let Some(t) = self.own_text.lock().unwrap().clone() {
                return Some(t);
            }
        }

        unsafe {
            XConvertSelection(
                self.dpy,
                self.atoms.clipboard,
                self.atoms.utf8,
                self.atoms.prop,
                self.win,
                CURRENT_TIME,
            );
            XFlush(self.dpy);
        }

        let deadline = Instant::now() + READ_TIMEOUT;
        let mut notified = false;
        while Instant::now() < deadline {
            if unsafe { XPending(self.dpy) } <= 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let mut ev = XEventBuf::zeroed();
            if !self.next_event(&mut ev) {
                continue;
            }
            if ev.kind() != EV_SELECTION_NOTIFY {
                continue;
            }
            let se = unsafe { &*(ev.0.as_ptr() as *const SelectionNotifyEvent) };
            // 不是我们要的那次转换（可能是 PRIMARY 或别的 target）就继续等。
            if se.selection != self.atoms.clipboard {
                continue;
            }
            if se.property == 0 {
                // 所有者拒绝了这个 target（比如它只有 text/html）。
                debug!("剪贴板所有者拒绝提供 UTF8_STRING");
                return None;
            }
            notified = true;
            break;
        }
        if !notified {
            // 超时。**绝不能当成「剪贴板空了」** —— 上层正是靠 None 区分「没读到」。
            debug!("读剪贴板超时（所有者未响应）");
            return None;
        }
        self.read_selection_data()
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        {
            let mut own = self.own_text.lock().unwrap();
            *own = Some(text.to_string());
        }
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(OwnerCmd::Own(ack_tx)).is_err() {
            return Err("剪贴板 service 线程已退出".to_string());
        }
        // 等 service 线程真的当上所有者：不等的话紧接着的 get_text 可能读到旧内容，
        // 于是「刚同步过来的文字」又被当成新内容发回去。
        match ack_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => Ok(()),
            Err(_) => Err("剪贴板 service 线程未在 500ms 内接管 selection".to_string()),
        }
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if !self.dpy.is_null() {
            unsafe {
                XDestroyWindow(self.dpy, self.win);
                XCloseDisplay(self.dpy);
            }
            self.dpy = std::ptr::null_mut();
            debug!("X11 剪贴板后端已关闭（剪贴板内容随所有者进程退出而失效）");
        }
    }
}

/// service 线程：独占一条 X 连接，当 owner，回答 `SelectionRequest`。
fn service_thread(
    display_name: String,
    own_text: Arc<Mutex<Option<String>>>,
    owner_win: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<OwnerCmd>,
) {
    init_xlib();
    let cname = match display_name.as_str() {
        "$DISPLAY" => None,
        s => CString::new(s).ok(),
    };
    let dpy = unsafe {
        XOpenDisplay(match &cname {
            Some(c) => c.as_ptr(),
            None => std::ptr::null(),
        })
    };
    if dpy.is_null() {
        info!(display = %display_name, "剪贴板 service 线程打不开 X 显示，剪贴板同步不可用");
        return;
    }
    let atoms = Atoms::intern_all(dpy);
    let root = unsafe { XDefaultRootWindow(dpy) };
    let win = unsafe { XCreateSimpleWindow(dpy, root, -10, -10, 1, 1, 0, 0, 0) };
    if win == 0 {
        unsafe { XCloseDisplay(dpy) };
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        // ① 处理「当上所有者」的请求
        while let Ok(OwnerCmd::Own(ack)) = rx.try_recv() {
            let text = own_text.lock().unwrap().clone().unwrap_or_default();
            unsafe {
                XSetSelectionOwner(dpy, atoms.clipboard, win, CURRENT_TIME);
                // PRIMARY 也一起拿下：X11 下鼠标中键粘贴用的是它，
                // 只同步 CLIPBOARD 会出现「Ctrl+V 可以、中键不行」的怪现象。
                XSetSelectionOwner(dpy, atoms.primary, win, CURRENT_TIME);
                XFlush(dpy);
            }
            owner_win.store(win, Ordering::Relaxed);
            debug!(bytes = text.len(), "已接管 X 剪贴板 selection");
            let _ = ack.send(());
        }

        // ② 处理 X 事件（有上限，免得事件洪水让这一轮永不返回）
        let mut handled = 0;
        while unsafe { XPending(dpy) } > 0 && handled < 64 {
            handled += 1;
            let mut ev = XEventBuf::zeroed();
            if unsafe { XNextEvent(dpy, ev.0.as_mut_ptr() as *mut c_void) } != 0 {
                continue;
            }
            match ev.kind() {
                EV_SELECTION_REQUEST => {
                    let req = unsafe { &*(ev.0.as_ptr() as *const SelectionRequestEvent) };
                    let text = own_text.lock().unwrap().clone().unwrap_or_default();
                    serve_request(dpy, &atoms, req, &text);
                }
                EV_SELECTION_CLEAR => {
                    // 别的程序把剪贴板抢走了：我们手上的 own_text 已经过期，
                    // 必须清掉，否则 get_text 会永远返回旧内容（而且还会把它再发一遍）。
                    let se = unsafe { &*(ev.0.as_ptr() as *const SelectionClearEvent) };
                    // CLIPBOARD 和 PRIMARY 我们是一起拿的，哪个被抢都要让位。
                    if se.selection == atoms.clipboard || se.selection == atoms.primary {
                        *own_text.lock().unwrap() = None;
                        owner_win.store(0, Ordering::Relaxed);
                        debug!("剪贴板 selection 被其他程序接管");
                    }
                }
                _ => {}
            }
        }

        std::thread::sleep(SERVICE_TICK);
    }

    unsafe {
        XDestroyWindow(dpy, win);
        XCloseDisplay(dpy);
    }
}

/// 回答一次 `SelectionRequest`。
fn serve_request(dpy: *mut Display, atoms: &Atoms, req: &SelectionRequestEvent, text: &str) {
    // 老客户端可能不发 property，那就按 target 的名字写。
    let prop = if req.property != 0 { req.property } else { req.target };
    // 注意这里是 SelectionNotifyEvent（**没有 owner 字段**）：
    // 以前用带 owner 的那个结构体填，发出去的 notify 里 selection 是窗口号，
    // 对方一概不理 —— 表现是「对面读我们的剪贴板永远卡住」。
    let mut reply = SelectionNotifyEvent {
        type_: EV_SELECTION_NOTIFY,
        serial: 0,
        send_event: 1,
        display: dpy,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: prop,
        time: req.time,
    };

    if req.target == atoms.targets {
        let list: [c_long; 4] = [
            atoms.targets as c_long,
            atoms.utf8 as c_long,
            atoms.text as c_long,
            XA_STRING as c_long,
        ];
        unsafe {
            XChangeProperty(
                dpy,
                req.requestor,
                prop,
                XA_ATOM,
                32,
                PROP_MODE_REPLACE,
                list.as_ptr() as *const c_uchar,
                list.len() as c_int,
            );
        }
    } else if req.target == atoms.utf8 || req.target == atoms.text || req.target == XA_STRING {
        // 单个 X 请求能写的数据约 256KB，我们自己的上限 192KB，一定一次写得完。
        unsafe {
            XChangeProperty(
                dpy,
                req.requestor,
                prop,
                atoms.utf8,
                8,
                PROP_MODE_REPLACE,
                text.as_ptr(),
                text.len() as c_int,
            );
        }
    } else {
        // 明确拒绝：property = 0，请求方据此判定失败（而不是傻等）。
        reply.property = 0;
    }

    unsafe {
        XSendEvent(
            dpy,
            req.requestor,
            0,
            0,
            &mut reply as *mut SelectionNotifyEvent as *mut c_void,
        );
        XFlush(dpy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这三个结构体必须和 Xlib 的 ABI 逐字节对齐 —— 真实教训：
    /// 曾经用「一个结构体塞下 request / notify 两种事件」（多一个 `owner` 字段靠命名兼容），
    /// 结果 notify 事件里 `requestor` 之后的字段整体错位一格，真机上表现为
    /// 「读别人剪贴板永远超时 + 别人读我们的永远没回音」两面夹死。
    /// 只靠真机才发现太贵了，这里用布局断言钉住。
    #[test]
    fn selection_event_layouts_match_xlib() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<SelectionNotifyEvent>(), 72);
            assert_eq!(std::mem::size_of::<SelectionRequestEvent>(), 80);
            assert_eq!(std::mem::size_of::<SelectionClearEvent>(), 56);
            assert_eq!(std::mem::offset_of!(SelectionNotifyEvent, requestor), 32);
            assert_eq!(std::mem::offset_of!(SelectionNotifyEvent, selection), 40);
            assert_eq!(std::mem::offset_of!(SelectionNotifyEvent, target), 48);
            assert_eq!(std::mem::offset_of!(SelectionNotifyEvent, property), 56);
            assert_eq!(std::mem::offset_of!(SelectionNotifyEvent, time), 64);
            assert_eq!(std::mem::offset_of!(SelectionRequestEvent, owner), 32);
            assert_eq!(std::mem::offset_of!(SelectionRequestEvent, requestor), 40);
            assert_eq!(std::mem::offset_of!(SelectionClearEvent, selection), 40);
            assert_eq!(std::mem::offset_of!(SelectionClearEvent, time), 48);
        }

        // 与指针宽度无关的结构性约束：notify 比 request 少一个字段，
        // 所以 request 的 owner/requestor 正好落在 notify 的 requestor/selection 上。
        // 这条断言正是「混用结构体」会破的那条。
        assert_eq!(
            std::mem::offset_of!(SelectionNotifyEvent, requestor),
            std::mem::offset_of!(SelectionRequestEvent, owner)
        );
        assert_eq!(
            std::mem::offset_of!(SelectionNotifyEvent, selection),
            std::mem::offset_of!(SelectionRequestEvent, requestor)
        );
        assert_eq!(
            std::mem::size_of::<SelectionRequestEvent>(),
            std::mem::size_of::<SelectionNotifyEvent>() + std::mem::size_of::<usize>()
        );
    }
}