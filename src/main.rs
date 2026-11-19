#![windows_subsystem = "windows"]

mod doc;
mod editor;
mod orig;
mod render;
mod search;
mod wal;
mod winfile;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use render::{BarView, Gfx, Layout, View};
use search::{Batch, Matcher};
use wal::{Msg, Wal};
use winfile::{open_orig, wide};

const CLASS: PCWSTR = w!("PytteEditor");
const WM_LOADED: u32 = WM_APP + 1;
const WM_SEARCH: u32 = WM_APP + 2;
/// Synkront sökfönster runt markören (för omedelbart svar innan bakgrundssökningen hunnit dit).
const LOCAL_WIN: usize = 1 << 20;
const CLOSED_MAX: usize = 50;
const FIND_HINT: &str = "ENTER/F3 NÄSTA  SHIFT+ENTER FÖRRA  ALT+C Aa  ESC STÄNG";
const GOTO_HINT: &str = "ENTER GÅ  ESC STÄNG";
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum BarMode {
    Find,
    Goto,
}

struct Bar {
    mode: BarMode,
    input: String,
    focus: bool,
    /// Hela inmatningen markerad: nästa tecken ersätter den.
    fresh: bool,
}

/// Bakgrundssökningens tillstånd. Resultatet gäller (tab, version).
struct Search {
    query: String,
    /// Skiftlägeskänslig.
    case: bool,
    matcher: Option<Arc<Matcher>>,
    generation: u64,
    tab: u64,
    version: u64,
    hits: Vec<usize>,
    total: u64,
    scanned: usize,
    len: usize,
    done: bool,
    handle: Option<search::Handle>,
    tx: Sender<Batch>,
    rx: Receiver<Batch>,
    /// Hoppa när resultatet når fram: Some(framåt).
    pending: Option<bool>,
    origin: usize,
}

struct App {
    hwnd: HWND,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    wal: Wal,
    gfx: Gfx,
    status: String,
    status_bad: bool,
    man_dirty: bool,
    hits: Vec<(f32, f32, usize)>,
    t0: Option<Instant>,
    load_tx: Sender<(u64, Outcome)>,
    load_rx: Receiver<(u64, Outcome)>,
    dragging: bool,
    hi_surrogate: Option<u16>,
    bar: Option<Bar>,
    search: Search,
    /// Sökartråden har postat WM_SEARCH som UI ännu inte hämtat.
    posted: Arc<AtomicBool>,
    /// Stängda flikar med sökväg: (sökväg, markör, top). Senaste sist.
    closed: Vec<(String, usize, usize)>,
    /// Klickräkning för dubbel-/trippelklick: (tid ms, x, y, antal).
    clicks: (u32, f32, f32, u8),
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

    fn layout(&self) -> Layout {
        self.gfx
            .layout(self.hwnd, self.tabs[self.active].gutter, self.bar.is_some())
    }

    fn rows_cols(&self) -> (usize, usize) {
        let l = self.layout();
        (l.rows, l.cols)
    }

    fn set_status(&mut self, msg: impl Into<String>, bad: bool) {
        self.status = msg.into();
        self.status_bad = bad;
        self.invalidate();
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
                        let bad = m.contains("ÄNDRAD UTANFÖR");
                        self.set_status(m, bad);
                    }
                    if i == self.active {
                        self.follow();
                    }
                }
                Outcome::Failed(m) => {
                    if !m.is_empty() {
                        self.set_status(m, true);
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

    /// Lägsta lediga visningsnummer bland öppna namnlösa flikar.
    fn free_no(&self) -> u64 {
        let mut used: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.path.is_none())
            .map(|t| t.no)
            .collect();
        used.sort_unstable();
        let mut n = 1;
        for u in used {
            if u == n {
                n += 1;
            } else if u > n {
                break;
            }
        }
        n
    }

    fn new_tab(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.wal.send(Msg::Reset(id, wal::rec_header(0, 0, None)));
        let mut t = Tab::new(id, None, Doc::empty());
        t.no = self.free_no();
        self.tabs.push(t);
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
            if self.search.matcher.is_some() {
                self.search_restart();
            }
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
            let t = &self.tabs[i];
            // Sista fliken som redan är tom och namnlös: att stänga den skulle bara
            // skapa en likadan (ny WAL-fil och nytt nummer per tangentrepetition).
            if self.tabs.len() == 1
                && t.path.is_none()
                && !t.dirty
                && !t.loading
                && t.doc.len() == 0
            {
                return;
            }
            if let Some(p) = &t.path {
                self.closed.push((p.clone(), t.cursor, t.top));
                if self.closed.len() > CLOSED_MAX {
                    self.closed.remove(0);
                }
            }
            self.drop_tab(i);
        }
    }

    /// Ctrl+Shift+T: återöppnar senast stängda flik på samma markörposition.
    fn reopen_closed(&mut self) {
        let Some((path, cursor, top)) = self.closed.pop() else {
            self.set_status("INGA STÄNGDA FLIKAR ATT ÅTERÖPPNA", false);
            return;
        };
        let before = self.tabs.len();
        self.open_path(&path);
        // Ny flik laddas asynkront; clamp() vid laddning håller positionerna giltiga.
        if self.tabs.len() > before {
            let t = self.tab();
            t.cursor = cursor;
            t.top = top;
        }
    }

    /// Ctrl+Shift+PgUp/PgDn: flyttar aktiv flik ett steg (ingen wrap).
    fn move_tab(&mut self, right: bool) {
        let a = self.active;
        let j = if right {
            if a + 1 >= self.tabs.len() {
                return;
            }
            a + 1
        } else {
            if a == 0 {
                return;
            }
            a - 1
        };
        self.tabs.swap(a, j);
        self.active = j;
        self.structural();
    }

    fn cycle_tab(&mut self, forward: bool) {
        let n = self.tabs.len();
        let i = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        self.switch(i);
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
        // Index- och söktrådar håller egna referenser: avbryt och vänta in dem.
        let weak = Arc::downgrade(t.doc.orig());
        t.doc = Doc::empty();
        t.clear_history();
        self.search.handle = None;
        self.search.version = u64::MAX; // startas om av timern
        orig::wait_released(&weak, 500);
        let t = &mut self.tabs[i];
        match winfile::write_file_atomic(&path, &data) {
            Ok(()) => {
                let n = data.len();
                let (doc, ident) = match open_orig(&path) {
                    Ok((o, ident)) if o.bytes().len() == n => (Doc::new(o), ident),
                    _ => (Doc::new(orig::owned(data)), (n as u64, 0)),
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
                self.set_status(format!("SPARAD {}", render::human(ident.0 as usize)), false);
                self.structural();
                true
            }
            Err(e) => {
                // Innehållet == bas + WAL, så fortsatta WAL-poster förblir giltiga.
                t.doc = Doc::new(orig::owned(data));
                t.clamp();
                self.set_status(format!("SPARNING MISSLYCKADES: {e}"), true);
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
            self.set_status(
                format!(
                    "MARKERINGEN ÄR FÖR STOR FÖR URKLIPP (> {} MB)",
                    CLIP_MAX >> 20
                ),
                true,
            );
            return;
        }
        let text = String::from_utf8_lossy(&t.doc.slice(a, b)).into_owned();
        if !clip_set(self.hwnd, &text) {
            self.set_status("URKLIPPET ÄR UPPTAGET", true);
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

    fn on_key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool, alt: bool) -> Option<Cmd> {
        // Globala: flikar, sök och gå till rad fungerar oavsett fokus.
        match vk {
            VK_PRIOR | VK_NEXT if ctrl => {
                if shift {
                    self.move_tab(vk == VK_NEXT);
                } else {
                    self.cycle_tab(vk == VK_NEXT);
                }
                return None;
            }
            VK_T if ctrl && shift => {
                self.reopen_closed();
                return None;
            }
            // Ctrl+1–8: flik 1–8, Ctrl+9: sista. Inte med Alt: AltGr = Ctrl+Alt ger
            // @ £ $ { [ ] } på svenskt tangentbord.
            v if ctrl && !alt && !shift && (0x31..=0x39).contains(&v.0) => {
                let k = (v.0 - 0x31) as usize;
                let n = self.tabs.len();
                let i = if k == 8 { n - 1 } else { k };
                if i < n {
                    self.switch(i);
                }
                return None;
            }
            VK_F if ctrl => {
                self.open_bar(BarMode::Find);
                return None;
            }
            VK_G if ctrl => {
                self.open_bar(BarMode::Goto);
                return None;
            }
            VK_F3 => {
                self.find_step(!shift);
                return None;
            }
            _ => {}
        }
        if self.bar.as_ref().is_some_and(|b| b.focus) {
            if self.bar_key(vk, ctrl, shift) {
                return None;
            }
            // Redigeringstangenter når inte texten medan sökraden har fokus.
            if matches!(vk, VK_BACK | VK_DELETE | VK_RETURN | VK_TAB)
                || (ctrl && matches!(vk, VK_Z | VK_Y | VK_X | VK_V | VK_D | VK_K | VK_A))
            {
                return None;
            }
        }
        if ctrl && !shift && vk == VK_D {
            let r = self.tabs[self.active].duplicate_lines(&self.wal);
            self.line_op_done(r);
            return None;
        }
        if ctrl && shift && vk == VK_K {
            self.tabs[self.active].delete_lines(&self.wal);
            self.follow();
            return None;
        }
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
            VK_ESCAPE => {
                if sel.is_none() && self.bar.is_some() {
                    self.close_bar();
                    return None;
                }
                t.begin_move(false)
            }
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
                self.cycle_tab(!shift);
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

    // ------------------------------------------------------------ radoperationer

    fn line_op_done(&mut self, r: std::result::Result<(), String>) {
        if let Err(e) = r {
            self.set_status(e, true);
        }
        self.follow();
    }

    /// Alt+tangent. Returnerar true om den hanterades.
    fn on_syskey(&mut self, vk: VIRTUAL_KEY) -> bool {
        match vk {
            VK_UP | VK_DOWN => {
                if self.bar.as_ref().is_some_and(|b| b.focus) {
                    return true;
                }
                let r = self.tabs[self.active].move_lines(&self.wal, vk == VK_DOWN);
                self.line_op_done(r);
                true
            }
            VK_C if self.bar.as_ref().is_some_and(|b| b.mode == BarMode::Find) => {
                self.search.case = !self.search.case;
                self.query_changed(false);
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ sökrad

    fn open_bar(&mut self, mode: BarMode) {
        let t = &self.tabs[self.active];
        // Ctrl+F med kort enradsmarkering förifyller sökningen.
        let prefill = (mode == BarMode::Find)
            .then(|| t.sel())
            .flatten()
            .filter(|(a, z)| z - a <= 256)
            .map(|(a, z)| t.doc.slice(a, z))
            .filter(|b| !b.contains(&b'\n') && !b.contains(&b'\r'))
            .and_then(|b| String::from_utf8(b).ok());
        self.search.origin = t.sel().map_or(t.cursor, |s| s.0);
        let same = self.bar.as_ref().is_some_and(|b| b.mode == mode);
        let input = match (prefill, same, mode) {
            (Some(p), _, _) => p,
            (None, true, _) => self.bar.as_ref().unwrap().input.clone(),
            (None, false, BarMode::Find) => self.search.query.clone(),
            (None, false, BarMode::Goto) => String::new(),
        };
        self.bar = Some(Bar {
            mode,
            fresh: !input.is_empty(),
            input,
            focus: true,
        });
        if mode == BarMode::Find {
            self.query_changed(false);
        } else {
            self.search.handle = None;
            self.search.matcher = None;
        }
        self.follow();
    }

    fn close_bar(&mut self) {
        self.bar = None;
        self.search.handle = None;
        self.search.matcher = None;
        self.search.pending = None;
        self.follow();
    }

    fn bar_input_changed(&mut self) {
        let Some(b) = &self.bar else { return };
        if b.mode == BarMode::Find {
            self.search.origin = {
                let t = &self.tabs[self.active];
                t.sel().map_or(t.cursor, |s| s.0)
            };
            self.query_changed(true);
        }
        self.invalidate();
    }

    fn on_bar_char(&mut self, c: char) {
        let Some(b) = &mut self.bar else { return };
        if (c as u32) < 0x20 || ('\u{7f}'..='\u{9f}').contains(&c) {
            return;
        }
        if b.mode == BarMode::Goto && !c.is_ascii_digit() {
            return;
        }
        if b.fresh {
            b.input.clear();
            b.fresh = false;
        }
        if b.input.len() < 1024 {
            b.input.push(c);
        }
        self.bar_input_changed();
    }

    /// Tangent när sökraden har fokus. true = hanterad.
    fn bar_key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool) -> bool {
        let Some(b) = &mut self.bar else { return false };
        match vk {
            VK_ESCAPE => self.close_bar(),
            VK_RETURN => match b.mode {
                BarMode::Find => self.find_step(!shift),
                BarMode::Goto => self.goto_line(),
            },
            VK_BACK => {
                if ctrl || b.fresh {
                    b.input.clear();
                } else {
                    b.input.pop();
                }
                b.fresh = false;
                self.bar_input_changed();
            }
            VK_A if ctrl => {
                b.fresh = !b.input.is_empty();
                self.invalidate();
            }
            VK_V if ctrl => {
                let mode = b.mode;
                if let Some(c) = clip_get(self.hwnd) {
                    let line: String = c
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .filter(|c| mode == BarMode::Find || c.is_ascii_digit())
                        .take(1024)
                        .collect();
                    let b = self.bar.as_mut().unwrap();
                    if b.fresh {
                        b.input.clear();
                        b.fresh = false;
                    }
                    b.input.push_str(&line);
                    self.bar_input_changed();
                }
            }
            _ => return false,
        }
        true
    }

    fn goto_line(&mut self) {
        let Some(b) = &self.bar else { return };
        let Ok(n) = b.input.trim().parse::<u64>() else {
            self.set_status("ANGE ETT RADNUMMER", true);
            return;
        };
        let t = &mut self.tabs[self.active];
        if t.loading {
            return;
        }
        t.doc.resolve();
        let line = n.max(1) - 1;
        let pos = match (t.doc.pos_of_line(line), t.doc.lines()) {
            (Some(p), _) => p,
            (None, Some(total)) => t.doc.pos_of_line(total - 1).unwrap_or(0),
            (None, None) => {
                let pct = t.doc.index_progress().map_or(100.0, |p| p * 100.0);
                self.set_status(
                    format!("RADINDEXET NÅR INTE DIT ÄN ({pct:.0} %) – FÖRSÖK IGEN STRAX"),
                    true,
                );
                return;
            }
        };
        t.begin_move(false);
        t.cursor = pos;
        t.want = None;
        self.bar = None;
        self.center_cursor();
    }

    /// Lägger markörens rad ungefär i mitten av vyn (sök/gå till rad).
    fn center_cursor(&mut self) {
        let (rows, cols) = self.rows_cols();
        let t = self.tab();
        let cl = t.doc.line_start(t.cursor);
        let mut top = cl;
        for _ in 0..rows / 3 {
            match t.doc.prev_line_start(top) {
                Some(p) => top = p,
                None => break,
            }
        }
        t.top = top;
        t.ensure_visible(rows, cols);
        self.man_dirty = true;
        self.invalidate();
    }

    // ------------------------------------------------------------ sökning

    /// Ny fråga eller nytt skiftläge. `jump` = välj första träff från origin.
    fn query_changed(&mut self, jump: bool) {
        let q = self
            .bar
            .as_ref()
            .filter(|b| b.mode == BarMode::Find)
            .map(|b| b.input.clone())
            .unwrap_or_default();
        self.search.query = q.clone();
        self.search.matcher = Matcher::new(q.as_bytes(), !self.search.case).map(Arc::new);
        self.search.pending = None;
        self.search_restart();
        if jump && self.search.matcher.is_some() {
            let origin = self.search.origin;
            match self.local_find(origin, true) {
                Some(h) => self.select_hit(h, false),
                None => {
                    self.search.pending = Some(true);
                }
            }
        }
        self.invalidate();
    }

    fn search_fresh(&self) -> bool {
        let t = &self.tabs[self.active];
        self.search.tab == t.id && self.search.version == t.doc.version() && !t.loading
    }

    fn search_restart(&mut self) {
        let s = &mut self.search;
        s.handle = None;
        s.generation += 1;
        s.hits.clear();
        s.total = 0;
        s.scanned = 0;
        s.done = false;
        let t = &self.tabs[self.active];
        s.tab = t.id;
        s.version = t.doc.version();
        s.len = t.doc.len();
        let Some(m) = s.matcher.clone() else { return };
        if t.loading {
            // Startas om från timern när fliken laddats.
            s.version = u64::MAX;
            return;
        }
        let posted = self.posted.clone();
        let h = self.hwnd.0 as isize;
        s.handle = Some(search::spawn(
            s.generation,
            t.doc.snapshot(),
            m,
            s.tx.clone(),
            move || {
                if !posted.swap(true, Ordering::AcqRel) {
                    unsafe {
                        let _ =
                            PostMessageW(Some(HWND(h as *mut _)), WM_SEARCH, WPARAM(0), LPARAM(0));
                    }
                }
            },
        ));
    }

    fn on_search(&mut self) {
        self.posted.store(false, Ordering::Release);
        let s = &mut self.search;
        while let Ok(b) = s.rx.try_recv() {
            if b.generation != s.generation {
                continue;
            }
            s.hits.extend_from_slice(&b.hits);
            s.total = b.total;
            s.scanned = b.scanned;
            s.done |= b.done;
        }
        if let Some(fwd) = self.search.pending
            && self.search_fresh()
        {
            let s = &self.search;
            let o = s.origin;
            let i = s.hits.partition_point(|&h| h < o);
            let pick = if fwd {
                s.hits
                    .get(i)
                    .copied()
                    .map(|h| (h, false))
                    .or_else(|| (s.done && !s.hits.is_empty()).then(|| (s.hits[0], true)))
            } else if s.scanned >= o.saturating_add(LOCAL_WIN) || s.done {
                if i > 0 {
                    Some((s.hits[i - 1], false))
                } else {
                    s.hits.last().filter(|_| s.done).map(|&h| (h, true))
                }
            } else {
                None
            };
            if let Some((h, wrapped)) = pick {
                self.search.pending = None;
                self.select_hit(h, wrapped);
            } else if self.search.done {
                self.search.pending = None;
            }
        }
        self.invalidate();
    }

    /// Synkron sökning i ett begränsat fönster från `from`.
    fn local_find(&self, from: usize, forward: bool) -> Option<usize> {
        let m = self.search.matcher.as_ref()?;
        let d = &self.tabs[self.active].doc;
        let n = m.len();
        if forward {
            let bytes = d.slice(from, from.saturating_add(LOCAL_WIN + n));
            m.first(&bytes).map(|i| from + i)
        } else {
            let a = from.saturating_sub(LOCAL_WIN);
            let bytes = d.slice(a, from.saturating_add(n - 1));
            m.last_before(&bytes, from - a).map(|i| a + i)
        }
    }

    fn select_hit(&mut self, h: usize, wrapped: bool) {
        let n = self.search.matcher.as_ref().map_or(0, |m| m.len());
        let t = self.tab();
        t.begin_move(false);
        t.anchor = Some(h);
        t.cursor = h + n;
        t.want = None;
        t.seal();
        if wrapped {
            self.set_status("SÖKNINGEN BÖRJADE OM", false);
        }
        self.center_cursor();
    }

    /// F3/Enter: nästa (framåt) eller föregående träff.
    fn find_step(&mut self, forward: bool) {
        if self.search.matcher.is_none() {
            self.open_bar(BarMode::Find);
            return;
        }
        let t = &self.tabs[self.active];
        if t.loading {
            return;
        }
        let from = match (t.sel(), forward) {
            (Some((a, _)), true) => a + 1,
            (Some((a, _)), false) => a,
            (None, _) => t.cursor,
        };
        if self.search_fresh() {
            let s = &self.search;
            let i = s.hits.partition_point(|&h| h < from);
            if forward && i < s.hits.len() {
                self.select_hit(s.hits[i], false);
                return;
            }
            if !forward && i > 0 && (i < s.hits.len() || s.done || s.scanned >= from) {
                self.select_hit(s.hits[i - 1], false);
                return;
            }
        }
        if let Some(h) = self.local_find(from, forward) {
            self.select_hit(h, false);
            return;
        }
        if self.search_fresh() && self.search.done {
            let s = &self.search;
            let wrap = if forward {
                s.hits.first()
            } else {
                s.hits.last()
            };
            match wrap.copied() {
                Some(h) => self.select_hit(h, true),
                None => self.set_status("INGA TRÄFFAR", true),
            }
            return;
        }
        self.search.origin = from;
        self.search.pending = Some(forward);
        if !self.search_fresh() {
            self.search_restart();
        }
        self.invalidate();
    }

    /// Fri funktion över enskilda fält så att Gfx kan lånas mutabelt samtidigt.
    fn bar_view<'a>(
        bar: &'a Option<Bar>,
        search: &'a Search,
        t: &'a Tab,
        fresh: bool,
    ) -> Option<BarView<'a>> {
        let b = bar.as_ref()?;
        let (label, hint, case) = match b.mode {
            BarMode::Find => ("SÖK", FIND_HINT, Some(search.case)),
            BarMode::Goto => ("GÅ TILL RAD", GOTO_HINT, None),
        };
        let (info, bad) = match b.mode {
            BarMode::Goto => match t.doc.lines() {
                Some(n) => (format!("AV {n}"), false),
                None => (format!("AV ~{}", t.doc.est_lines()), false),
            },
            BarMode::Find => {
                let s = search;
                if s.matcher.is_none() {
                    (String::new(), false)
                } else if !fresh {
                    ("…".into(), false)
                } else if s.done && s.total == 0 {
                    ("INGA TRÄFFAR".into(), true)
                } else {
                    let cur = t.sel().and_then(|(a, _)| {
                        let i = s.hits.partition_point(|&h| h < a);
                        (s.hits.get(i) == Some(&a)).then_some(i + 1)
                    });
                    let total = if s.done {
                        s.total.to_string()
                    } else {
                        let pct = if s.len == 0 {
                            100.0
                        } else {
                            s.scanned as f64 * 100.0 / s.len as f64
                        };
                        format!("{}+ ({pct:.0} %)", s.total)
                    };
                    match cur {
                        Some(i) => (format!("{i} / {total}"), false),
                        None => (format!("{total} TRÄFFAR"), false),
                    }
                }
            }
        };
        Some(BarView {
            label,
            input: &b.input,
            focus: b.focus,
            fresh: b.fresh,
            case,
            info,
            bad,
            hint,
        })
    }

    /// Timer (250 ms): debouncad omstart av sökning efter redigering,
    /// samt omritning medan radindexet växer.
    fn on_tick(&mut self) {
        if self.man_dirty {
            self.send_manifest();
        }
        if self.search.matcher.is_some() && !self.search_fresh() && !self.tabs[self.active].loading
        {
            self.search_restart();
        }
        let t = &mut self.tabs[self.active];
        if !t.loading && (t.doc.index_progress().is_some() | t.doc.resolve()) {
            self.invalidate();
        }
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
        let l = self.layout();
        let row = ((y - l.y0) / self.gfx.lh).clamp(0.0, (l.rows - 1) as f32) as usize;
        let col = ((x - l.tx) / self.gfx.cw + 0.5).max(0.0) as usize;
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
        let l = self.layout();
        if y < l.tab_h {
            if let Some(i) = self.tab_at(x) {
                self.switch(i);
            }
            return;
        }
        if let Some(b) = &mut self.bar {
            // Klick i sökraden ger den fokus, klick i texten tar tillbaka det.
            let in_bar = y >= l.bar_y && y < l.bar_y + l.bar_h;
            if b.focus != in_bar {
                b.focus = in_bar;
                self.invalidate();
            }
            if in_bar {
                return;
            }
        }
        if y < l.y0 || y >= l.bar_y || x < l.sep || self.tabs[self.active].loading {
            return;
        }
        let pos = self.hit(x, y);
        // Dubbel-/trippelklick: samma ställe inom systemets dubbelklickstid.
        let now = unsafe { GetMessageTime() } as u32;
        let (t0, cx, cy, n) = self.clicks;
        let near = (x - cx).abs() <= 4.0 && (y - cy).abs() <= 4.0;
        let count = if !shift && near && now.wrapping_sub(t0) <= unsafe { GetDoubleClickTime() } {
            n % 3 + 1
        } else {
            1
        };
        self.clicks = (now, x, y, count);
        let t = self.tab();
        t.seal();
        t.want = None;
        match count {
            2 => {
                let (a, z) = t.doc.word_at(pos);
                t.anchor = Some(a);
                t.cursor = z;
                self.follow();
                return;
            }
            3 => {
                let a = t.doc.line_start(pos);
                let z = t.doc.next_line_start(pos).unwrap_or(t.doc.len());
                t.anchor = Some(a);
                t.cursor = z;
                self.follow();
                return;
            }
            _ => {}
        }
        if shift {
            t.anchor.get_or_insert(t.cursor);
        } else {
            t.anchor = Some(pos);
        }
        t.cursor = pos;
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
        let l = self.layout();
        if y < l.y0 {
            self.tab().scroll(-1);
        } else if y >= l.bar_y {
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
        // Gutterbredd: exakt när indexet är klart, annars uppskattad. Växer bara,
        // så layouten hoppar aldrig när indexet blir färdigt.
        let t = &mut self.tabs[self.active];
        if !t.loading {
            t.doc.resolve();
            let need = render::digits(t.doc.lines().unwrap_or_else(|| t.doc.est_lines()));
            if need > t.gutter {
                t.gutter = need;
                let (r, c) = self.rows_cols();
                self.tab().ensure_visible(r, c);
            }
        }
        let fresh = self.search_fresh();
        let bar = Self::bar_view(&self.bar, &self.search, &self.tabs[self.active], fresh);
        let v = View {
            status: &self.status,
            status_bad: self.status_bad,
            wal_failed: self.wal.has_failed(),
            bar,
            matcher: self.search.matcher.as_deref(),
        };
        self.hits = self.gfx.draw(self.hwnd, &self.tabs, self.active, &v);
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
                with_app(|a| {
                    if a.bar.as_ref().is_some_and(|b| b.focus) {
                        if let Some(c) = char::from_u32(wp.0 as u32) {
                            a.on_bar_char(c);
                        }
                    } else {
                        a.on_char(wp.0 as u16)
                    }
                });
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            let cmd = with_app(|a| {
                a.on_key(
                    VIRTUAL_KEY(wp.0 as u16),
                    down(VK_CONTROL),
                    down(VK_SHIFT),
                    down(VK_MENU),
                )
            });
            if let Some(c) = cmd {
                run_cmd(hwnd, c);
            }
            return LRESULT(0);
        }
        WM_SYSKEYDOWN => {
            // Alt+Upp/Ned (flytta rader), Alt+C (skiftläge). Övrigt (Alt+F4 …) till Windows.
            if with_app(|a| a.on_syskey(VIRTUAL_KEY(wp.0 as u16))) {
                return LRESULT(0);
            }
        }
        WM_SYSCHAR => {
            // Utan meny ger Alt+bokstav bara ett pip; Alt+Mellanslag (systemmeny) släpps igenom.
            if wp.0 as u32 != ' ' as u32 {
                return LRESULT(0);
            }
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
                if y < a.layout().tab_h {
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
            with_app(|a| a.on_tick());
            return LRESULT(0);
        }
        WM_SEARCH => {
            with_app(|a| a.on_search());
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
        // Ikonresurs 1 bäddas in av build.rs. Saknas den blir det Windows standardikon.
        let icon = PCWSTR(std::ptr::without_provenance(1)); // MAKEINTRESOURCE(1)
        let small = LoadImageW(
            Some(inst.into()),
            icon,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: inst.into(),
            hIcon: LoadIconW(Some(inst.into()), icon).unwrap_or_default(),
            hIconSm: small,
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
    let (s_tx, s_rx) = mpsc::channel();
    let mut app = App {
        hwnd,
        tabs: rec.tabs,
        active: rec.active,
        next_id: rec.next_id,
        wal,
        gfx: Gfx::new(hwnd)?,
        status: String::new(),
        status_bad: false,
        man_dirty: false,
        hits: Vec::new(),
        t0: Some(t0),
        load_tx,
        load_rx,
        dragging: false,
        hi_surrogate: None,
        bar: None,
        search: Search {
            query: String::new(),
            case: false,
            matcher: None,
            generation: 0,
            tab: 0,
            version: 0,
            hits: Vec::new(),
            total: 0,
            scanned: 0,
            len: 0,
            done: false,
            handle: None,
            tx: s_tx,
            rx: s_rx,
            pending: None,
            origin: 0,
        },
        posted: Arc::new(AtomicBool::new(false)),
        closed: Vec::new(),
        clicks: (0, 0.0, 0.0, 0),
    };
    for (id, job) in rec.jobs {
        app.spawn(id, job);
    }
    // Återställda namnlösa flikar numreras 1, 2, 3 … i flikordning.
    for (k, t) in app.tabs.iter_mut().filter(|t| t.path.is_none()).enumerate() {
        t.no = k as u64 + 1;
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
