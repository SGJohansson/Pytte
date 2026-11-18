#![windows_subsystem = "windows"]

mod doc;
mod editor;
mod render;
mod wal;
mod winfile;

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use doc::Doc;
use editor::{Job, Kind, Outcome, Tab};
use render::Gfx;
use wal::{Msg, Wal};
use winfile::{open_orig, wide};

const CLASS: PCWSTR = w!("PytteEditor");
const WM_LOADED: u32 = WM_APP + 1;
const CF_UNICODETEXT: u32 = 13;
const CLIP_MAX: usize = 256 << 20;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Kör `f` mot appen om den inte redan är lånad (modala dialoger re-entrar wndproc).
fn with_app<R: Default>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|c| match c.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f).unwrap_or_default(),
        Err(_) => R::default(),
    })
}

/// Kommandon som kräver modal UI körs utanför lånet av App.
enum Cmd {
    Open,
    SaveAs,
    Close(usize),
}

struct App {
    hwnd: HWND,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    wal: Wal,
    gfx: Gfx,
    status: String,
    man_dirty: bool,
    hits: Vec<(f32, f32, usize)>,
    t0: Option<Instant>,
    load_tx: Sender<(u64, Outcome)>,
    load_rx: Receiver<(u64, Outcome)>,
    dragging: bool,
    hi_surrogate: Option<u16>,
}

impl App {
    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn tab(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn rows_cols(&self) -> (usize, usize) {
        let l = self.gfx.layout(self.hwnd);
        (l.rows, l.cols)
    }

    fn follow(&mut self) {
        let (r, c) = self.rows_cols();
        self.tab().ensure_visible(r, c);
        self.man_dirty = true;
        self.invalidate();
    }

    fn update_title(&self) {
        let t = &self.tabs[self.active];
        let s = wide(&format!("{} — Pytte", t.title()));
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(s.as_ptr()));
        }
    }

    fn send_manifest(&mut self) {
        let active = self.tabs[self.active].id;
        let s = wal::manifest_string(active, self.tabs.iter().map(|t| (t.id, t.cursor, t.top)));
        self.wal.send(Msg::Manifest(s));
        self.man_dirty = false;
    }

    fn structural(&mut self) {
        self.tab().seal();
        self.send_manifest();
        self.update_title();
        self.follow();
    }

    fn persist(&mut self) {
        self.send_manifest();
        self.wal.flush();
    }

    // ------------------------------------------------------------ laddning

    fn spawn(&self, id: u64, job: Job) {
        let tx = self.load_tx.clone();
        let h = self.hwnd.0 as isize;
        std::thread::spawn(move || {
            let out = editor::load(job);
            let _ = tx.send((id, out));
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_LOADED, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn on_loaded(&mut self) {
        while let Ok((id, out)) = self.load_rx.try_recv() {
            let Some(i) = self.tabs.iter().position(|t| t.id == id) else {
                continue;
            };
            match out {
                Outcome::Ready {
                    doc,
                    ident,
                    dirty,
                    reset,
                    msg,
                } => {
                    let t = &mut self.tabs[i];
                    t.doc = doc;
                    t.crlf = t.doc.detect_crlf();
                    t.loading = false;
                    t.dirty = dirty;
                    t.clamp();
                    if reset {
                        self.wal.send(Msg::Reset(
                            id,
                            wal::rec_header(ident.0, ident.1, t.path.as_deref()),
                        ));
                    }
                    if let Some(m) = msg {
                        self.status = m;
                    }
                    if i == self.active {
                        self.follow();
                    }
                }
                Outcome::Failed(m) => {
                    if !m.is_empty() {
                        self.status = m;
                    }
                    self.drop_tab(i);
                }
            }
            self.send_manifest();
            self.invalidate();
        }
    }

    // ------------------------------------------------------------ flikar

    /// Ersätter en orörd tom startflik.
    fn take_blank(&mut self) {
        if let Some(cur) = self.tabs.get(self.active)
            && cur.path.is_none()
            && !cur.dirty
            && cur.doc.len() == 0
        {
            self.wal.send(Msg::Remove(cur.id));
            self.tabs.remove(self.active);
        }
    }

    fn new_tab(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.wal.send(Msg::Reset(id, wal::rec_header(0, 0, None)));
        self.tabs.push(Tab::new(id, None, Doc::empty()));
        self.active = self.tabs.len() - 1;
        self.structural();
    }

    fn open_path(&mut self, path: &str) {
        if let Some(i) = self.tabs.iter().position(|t| {
            t.path
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(path))
        }) {
            self.switch(i);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.take_blank();
        self.tabs.push(Tab::loading(id, path.to_string()));
        self.active = self.tabs.len() - 1;
        self.spawn(
            id,
            Job::Open {
                path: path.to_string(),
            },
        );
        self.structural();
    }

    fn switch(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.active {
            self.active = i;
            self.structural();
        }
    }

    fn drop_tab(&mut self, i: usize) {
        let t = self.tabs.remove(i);
        self.wal.send(Msg::Remove(t.id));
        if self.tabs.is_empty() {
            self.active = 0;
            self.new_tab();
            return;
        }
        if i < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.structural();
    }

    fn close(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.drop_tab(i);
        }
    }

    /// Sparar flik `i` till `path` (eller dess befintliga sökväg).
    fn save(&mut self, i: usize, path: Option<String>) -> bool {
        let Some(t) = self.tabs.get_mut(i) else {
            return false;
        };
        if t.loading {
            return false;
        }
        let Some(path) = path.or_else(|| t.path.clone()) else {
            return false;
        };
        let data = t.doc.materialize();
        // Släpp mappningen innan filen ersätts. Ångra-historiken pekar in i gamla Doc.
        t.doc = Doc::empty();
        t.clear_history();
        match winfile::write_file_atomic(&path, &data) {
            Ok(()) => {
                let n = data.len();
                let (doc, ident) = match open_orig(&path) {
                    Ok((o, ident)) if o.bytes().len() == n => (Doc::new(o), ident),
                    _ => (Doc::new(winfile::Orig::Owned(data)), (n as u64, 0)),
                };
                t.doc = doc;
                t.path = Some(path.clone());
                t.dirty = false;
                t.clamp();
                let id = t.id;
                self.wal.send(Msg::Reset(
                    id,
                    wal::rec_header(ident.0, ident.1, Some(&path)),
                ));
                self.status = format!("SPARAD {}", ident.0);
                self.structural();
                true
            }
            Err(e) => {
                // Innehållet == bas + WAL, så fortsatta WAL-poster förblir giltiga.
                t.doc = Doc::new(winfile::Orig::Owned(data));
                t.clamp();
                self.status = format!("SPARNING MISSLYCKADES: {e}");
                self.invalidate();
                false
            }
        }
    }

    // ------------------------------------------------------------ redigering

    /// Ersätter markeringen (eller [a, b) om ingen markering) med `ins`.
    fn edit(&mut self, range: Option<(usize, usize)>, ins: &[u8], kind: Kind) {
        let t = &mut self.tabs[self.active];
        let (a, b, kind) = match t.sel() {
            Some((a, b)) => (
                a,
                b,
                if kind == Kind::Typing {
                    Kind::Typing
                } else {
                    Kind::Other
                },
            ),
            None => match range {
                Some((a, b)) => (a, b, kind),
                None => (t.cursor, t.cursor, kind),
            },
        };
        t.replace(&self.wal, a, b, ins, kind);
        self.follow();
    }

    fn on_char(&mut self, u: u16) {
        let ch = match u {
            0xd800..=0xdbff => {
                self.hi_surrogate = Some(u);
                return;
            }
            0xdc00..=0xdfff => self
                .hi_surrogate
                .take()
                .and_then(|h| char::decode_utf16([h, u]).next())
                .and_then(|r| r.ok()),
            _ => {
                self.hi_surrogate = None;
                char::from_u32(u as u32)
            }
        };
        let Some(ch) = ch else { return };
        match ch {
            '\r' => {
                let nl: &[u8] = if self.tab().crlf { b"\r\n" } else { b"\n" };
                self.edit(None, nl, Kind::Typing);
            }
            '\t' => self.edit(None, b"\t", Kind::Typing),
            c if (c as u32) < 0x20 || ('\u{7f}'..='\u{9f}').contains(&c) => {}
            c => {
                let mut b = [0u8; 4];
                self.edit(None, c.encode_utf8(&mut b).as_bytes(), Kind::Typing);
            }
        }
    }

    fn copy(&mut self, cut: bool) {
        let t = &self.tabs[self.active];
        let Some((a, b)) = t.sel() else { return };
        if b - a > CLIP_MAX {
            self.status = format!(
                "MARKERINGEN ÄR FÖR STOR FÖR URKLIPP (> {} MB)",
                CLIP_MAX >> 20
            );
            self.invalidate();
            return;
        }
        let text = String::from_utf8_lossy(&t.doc.slice(a, b)).into_owned();
        if !clip_set(self.hwnd, &text) {
            self.status = "URKLIPPET ÄR UPPTAGET".into();
            self.invalidate();
            return;
        }
        if cut {
            self.edit(None, b"", Kind::Other);
        }
    }

    fn paste(&mut self) {
        let Some(s) = clip_get(self.hwnd) else { return };
        let s = s
            .replace('\0', "")
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let s = if self.tab().crlf {
            s.replace('\n', "\r\n")
        } else {
            s
        };
        self.edit(None, s.as_bytes(), Kind::Other);
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool) -> Option<Cmd> {
        let (rows, _) = self.rows_cols();
        let page = rows.saturating_sub(1).max(1);
        let t = &mut self.tabs[self.active];
        let cur = t.cursor;
        let sel = t.sel();
        let d = &t.doc;

        // Horisontella/absoluta förflyttningar.
        let target = match vk {
            VK_LEFT => Some(match sel {
                Some((a, _)) if !shift => a,
                _ if ctrl => d.word_left(cur),
                _ => d.left(cur),
            }),
            VK_RIGHT => Some(match sel {
                Some((_, b)) if !shift => b,
                _ if ctrl => d.word_right(cur),
                _ => d.right(cur),
            }),
            VK_HOME => Some(if ctrl { 0 } else { d.line_start(cur) }),
            VK_END => Some(if ctrl { d.len() } else { d.line_end(cur) }),
            _ => None,
        };
        if let Some(p) = target {
            t.begin_move(shift);
            t.cursor = p;
            t.want = None;
            self.follow();
            return None;
        }

        match vk {
            VK_UP | VK_DOWN => {
                t.begin_move(shift);
                t.vmove(vk == VK_DOWN);
            }
            VK_PRIOR | VK_NEXT => {
                let down = vk == VK_NEXT;
                t.begin_move(shift);
                t.scroll(if down {
                    page as isize
                } else {
                    -(page as isize)
                });
                for _ in 0..page {
                    t.vmove(down);
                }
            }
            VK_ESCAPE => t.begin_move(false),
            VK_BACK => {
                let a = if ctrl { d.word_left(cur) } else { d.left(cur) };
                self.edit(Some((a, cur)), b"", Kind::Back);
                return None;
            }
            VK_DELETE => {
                let b = if ctrl {
                    d.word_right(cur)
                } else {
                    d.right(cur)
                };
                self.edit(Some((cur, b)), b"", Kind::Fwd);
                return None;
            }
            VK_A if ctrl => {
                t.anchor = Some(0);
                t.cursor = d.len();
                t.seal();
                self.invalidate();
                return None;
            }
            VK_C | VK_INSERT if ctrl => {
                self.copy(false);
                return None;
            }
            VK_X if ctrl => {
                self.copy(true);
                return None;
            }
            VK_V if ctrl => {
                self.paste();
                return None;
            }
            VK_Z if ctrl => {
                if if shift {
                    t.redo(&self.wal)
                } else {
                    t.undo(&self.wal)
                } {
                    self.follow();
                }
                return None;
            }
            VK_Y if ctrl => {
                if t.redo(&self.wal) {
                    self.follow();
                }
                return None;
            }
            VK_TAB if ctrl => {
                let n = self.tabs.len();
                let i = if shift {
                    (self.active + n - 1) % n
                } else {
                    (self.active + 1) % n
                };
                self.switch(i);
                return None;
            }
            VK_N if ctrl => {
                self.new_tab();
                return None;
            }
            VK_O if ctrl => return Some(Cmd::Open),
            VK_S if ctrl => {
                if shift || t.path.is_none() {
                    return Some(Cmd::SaveAs);
                }
                self.save(self.active, None);
                return None;
            }
            VK_W | VK_F4 if ctrl => return Some(Cmd::Close(self.active)),
            _ => return None,
        }
        self.follow();
        None
    }

    // ------------------------------------------------------------ mus

    fn to_dip(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        let s = 96.0 / self.gfx.dpi;
        (x * s, y * s)
    }

    fn tab_at(&self, x: f32) -> Option<usize> {
        self.hits.iter().find(|h| x >= h.0 && x < h.1).map(|h| h.2)
    }

    /// Byteposition under (x, y); y kläms till textytan.
    fn hit(&self, x: f32, y: f32) -> usize {
        let l = self.gfx.layout(self.hwnd);
        let row = ((y - l.y0) / self.gfx.lh).clamp(0.0, (l.rows - 1) as f32) as usize;
        let col = ((x - l.x0) / self.gfx.cw + 0.5).max(0.0) as usize;
        let t = &self.tabs[self.active];
        let mut ls = t.top;
        for _ in 0..row {
            match t.doc.next_line_start(ls) {
                Some(n) => ls = n,
                None => break,
            }
        }
        t.doc.pos_at_vcol(ls, col + t.hscroll)
    }

    fn on_down(&mut self, lp: LPARAM, shift: bool) {
        let (x, y) = self.to_dip(lp);
        let l = self.gfx.layout(self.hwnd);
        if y < l.tab_h {
            if let Some(i) = self.tab_at(x) {
                self.switch(i);
            }
            return;
        }
        if y < l.y0 || y >= l.h - l.status_h || self.tabs[self.active].loading {
            return;
        }
        let pos = self.hit(x, y);
        let t = self.tab();
        t.seal();
        if shift {
            t.anchor.get_or_insert(t.cursor);
        } else {
            t.anchor = Some(pos);
        }
        t.cursor = pos;
        t.want = None;
        self.dragging = true;
        unsafe {
            SetCapture(self.hwnd);
        }
        self.follow();
    }

    fn on_drag(&mut self, lp: LPARAM) {
        if !self.dragging {
            return;
        }
        let (x, y) = self.to_dip(lp);
        let l = self.gfx.layout(self.hwnd);
        if y < l.y0 {
            self.tab().scroll(-1);
        } else if y >= l.h - l.status_h {
            self.tab().scroll(1);
        }
        let pos = self.hit(x, y);
        let t = self.tab();
        t.cursor = pos;
        t.want = None;
        self.follow();
    }

    fn on_up(&mut self) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        unsafe {
            let _ = ReleaseCapture();
        }
        let t = self.tab();
        if t.anchor == Some(t.cursor) {
            t.anchor = None;
        }
        self.invalidate();
    }

    fn on_wheel(&mut self, wp: WPARAM) {
        let delta = ((wp.0 >> 16) & 0xffff) as i16 as isize;
        self.tab().scroll(-delta * 3 / 120);
        self.man_dirty = true;
        self.invalidate();
    }

    fn paint(&mut self) {
        let mut status = self.status.clone();
        if self.wal.has_failed() {
            status = format!("WAL-FEL: SESSIONEN SKRIVS INTE  {status}");
        }
        self.hits = self.gfx.draw(self.hwnd, &self.tabs, self.active, &status);
        if let Some(t0) = self.t0.take()
            && self.status.is_empty()
        {
            self.status = format!("FÖRSTA FRAME {:.1} MS", t0.elapsed().as_secs_f64() * 1e3);
            self.invalidate();
        }
    }
}

// ---------------------------------------------------------------- urklipp

fn clip_set(hwnd: HWND, s: &str) -> bool {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = match GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            Ok(h) => {
                let p = GlobalLock(h) as *mut u16;
                if p.is_null() {
                    let _ = GlobalFree(Some(h));
                    false
                } else {
                    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                    let _ = GlobalUnlock(h);
                    if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))).is_ok() {
                        true
                    } else {
                        let _ = GlobalFree(Some(h));
                        false
                    }
                }
            }
            Err(_) => false,
        };
        let _ = CloseClipboard();
        ok
    }
}

fn clip_get(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let out = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(g);
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}

// ---------------------------------------------------------------- dialoger

fn split_multi(buf: &[u16]) -> Vec<String> {
    let parts: Vec<String> = buf
        .split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    match parts.len() {
        0 => vec![],
        1 => parts,
        _ => parts[1..]
            .iter()
            .map(|f| format!("{}\\{}", parts[0].trim_end_matches('\\'), f))
            .collect(),
    }
}

const FILTER: &str =
    "Text\0*.txt;*.log;*.md;*.csv;*.ini;*.cfg;*.conf;*.json;*.xml;*.yml;*.toml\0Alla filer\0*.*\0";

fn open_dialog(hwnd: HWND) -> Vec<String> {
    let mut buf = vec![0u16; 65536];
    let filter = wide(FILTER);
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_ALLOWMULTISELECT | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        split_multi(&buf)
    } else {
        vec![]
    }
}

fn save_dialog(hwnd: HWND, suggest: &str) -> Option<String> {
    let mut buf = vec![0u16; 32768];
    for (i, c) in suggest.encode_utf16().take(buf.len() - 1).enumerate() {
        buf[i] = c;
    }
    let filter = wide(FILTER);
    let ext = wide("txt");
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrDefExt: PCWSTR(ext.as_ptr()),
        Flags: OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if unsafe { GetSaveFileNameW(&mut ofn) }.as_bool() {
        split_multi(&buf).into_iter().next()
    } else {
        None
    }
}

fn run_cmd(hwnd: HWND, cmd: Cmd) {
    match cmd {
        Cmd::Open => {
            let files = open_dialog(hwnd);
            with_app(|a| files.iter().for_each(|f| a.open_path(f)));
        }
        Cmd::SaveAs => {
            let name = with_app(|a| a.tabs[a.active].title());
            if let Some(p) = save_dialog(hwnd, &name) {
                with_app(|a| a.save(a.active, Some(p)));
            }
        }
        Cmd::Close(i) => {
            let Some((name, dirty, has_path)) = with_app(|a| {
                a.tabs
                    .get(i)
                    .map(|t| (t.title(), t.dirty, t.path.is_some()))
            }) else {
                return;
            };
            if dirty {
                let q = wide(&format!("Spara ändringar i {name}?"));
                let r = unsafe {
                    MessageBoxW(
                        Some(hwnd),
                        PCWSTR(q.as_ptr()),
                        w!("Pytte"),
                        MB_YESNOCANCEL | MB_ICONWARNING,
                    )
                };
                if r == IDCANCEL {
                    return;
                }
                if r == IDYES {
                    let path = if has_path {
                        None
                    } else {
                        match save_dialog(hwnd, &name) {
                            Some(p) => Some(p),
                            None => return,
                        }
                    };
                    if !with_app(|a| a.save(i, path)) {
                        return;
                    }
                }
            }
            with_app(|a| a.close(i));
        }
    }
}

// ---------------------------------------------------------------- fönster

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            with_app(|a| a.paint());
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            return LRESULT(0);
        }
        WM_SIZE => {
            with_app(|a| {
                a.gfx.resize(hwnd);
                a.invalidate();
            });
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xffff) as f32;
            let r = unsafe { &*(lp.0 as *const RECT) };
            with_app(|a| a.gfx.set_dpi(dpi));
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            return LRESULT(0);
        }
        WM_CHAR => {
            // AltGr = Ctrl+Alt ger tecken (@ { } [ ] \ € på svenskt tangentbord).
            if !(down(VK_CONTROL) && !down(VK_MENU)) {
                with_app(|a| a.on_char(wp.0 as u16));
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            let cmd =
                with_app(|a| a.on_key(VIRTUAL_KEY(wp.0 as u16), down(VK_CONTROL), down(VK_SHIFT)));
            if let Some(c) = cmd {
                run_cmd(hwnd, c);
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            with_app(|a| a.on_down(lp, down(VK_SHIFT)));
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            with_app(|a| a.on_drag(lp));
            return LRESULT(0);
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            with_app(|a| a.on_up());
            return LRESULT(0);
        }
        WM_MBUTTONDOWN => {
            let i = with_app(|a| {
                let (x, y) = a.to_dip(lp);
                if y < a.gfx.layout(hwnd).tab_h {
                    a.tab_at(x)
                } else {
                    None
                }
            });
            if let Some(i) = i {
                run_cmd(hwnd, Cmd::Close(i));
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            with_app(|a| a.on_wheel(wp));
            return LRESULT(0);
        }
        WM_TIMER => {
            with_app(|a| {
                if a.man_dirty {
                    a.send_manifest();
                }
            });
            return LRESULT(0);
        }
        WM_LOADED => {
            with_app(|a| a.on_loaded());
            return LRESULT(0);
        }
        WM_COPYDATA => {
            let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
            let bytes =
                unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            let s = String::from_utf8_lossy(bytes).into_owned();
            with_app(|a| {
                s.lines()
                    .filter(|l| !l.is_empty())
                    .for_each(|p| a.open_path(p))
            });
            return LRESULT(1);
        }
        WM_QUERYENDSESSION => {
            with_app(|a| a.persist());
            return LRESULT(1);
        }
        WM_ENDSESSION => {
            if wp.0 != 0 {
                with_app(|a| a.persist());
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            // Ingen fråga: allt osparat ligger redan i WAL och återställs vid nästa start.
            with_app(|a| a.persist());
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn session_dir() -> String {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    format!("{base}\\Pytte\\session")
}

fn args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// En instans äger sessionen. Senare instanser skickar filerna vidare och avslutar.
fn forward_to_running() -> bool {
    unsafe {
        let _m = CreateMutexW(None, false, w!("Local\\PytteSession"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            std::mem::forget(_m);
            return false;
        }
        if let Ok(h) = FindWindowW(CLASS, PCWSTR::null()) {
            let payload = args()
                .iter()
                .map(|a| {
                    std::fs::canonicalize(a)
                        .map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
                        .unwrap_or(a.clone())
                })
                .collect::<Vec<_>>()
                .join("\n");
            let cds = COPYDATASTRUCT {
                dwData: 0,
                cbData: payload.len() as u32,
                lpData: payload.as_ptr() as *mut _,
            };
            SendMessageW(
                h,
                WM_COPYDATA,
                None,
                Some(LPARAM(&cds as *const _ as isize)),
            );
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
        true
    }
}

fn main() -> Result<()> {
    let t0 = Instant::now();
    if forward_to_running() {
        return Ok(());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let dir = session_dir();
    let _ = std::fs::create_dir_all(&dir);
    let rec = editor::recover(&dir);
    let wal = Wal::start(dir);

    let hwnd = unsafe {
        let inst = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            hCursor: LoadCursorW(None, IDC_IBEAM)?,
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            CLASS,
            w!("Pytte"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            820,
            None,
            None,
            Some(inst.into()),
            None,
        )?;
        let dark = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            4,
        );
        hwnd
    };

    let (load_tx, load_rx) = mpsc::channel();
    let mut app = App {
        hwnd,
        tabs: rec.tabs,
        active: rec.active,
        next_id: rec.next_id,
        wal,
        gfx: Gfx::new(hwnd)?,
        status: String::new(),
        man_dirty: false,
        hits: Vec::new(),
        t0: Some(t0),
        load_tx,
        load_rx,
        dragging: false,
        hi_surrogate: None,
    };
    for (id, job) in rec.jobs {
        app.spawn(id, job);
    }
    for a in args() {
        let p = std::fs::canonicalize(&a)
            .map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
            .unwrap_or(a);
        app.open_path(&p);
    }
    if app.tabs.is_empty() {
        app.new_tab();
    }
    app.active = app.active.min(app.tabs.len() - 1);
    app.structural();
    APP.with(|c| *c.borrow_mut() = Some(app));

    unsafe {
        SetTimer(Some(hwnd), 1, 250, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // Droppar Wal → tråden tömmer kön och joinas.
    APP.with(|c| c.borrow_mut().take());
    Ok(())
}
