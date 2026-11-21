//! Direct2D + DirectWrite. Ritar bara synliga rader (virtualiserat från `top`).
//!
//! Layoutregel: inget UI-element ritas ovanpå text. Gutter, sökrad och statusrad
//! tar egen yta ur layouten; segment som inte får plats kortas eller döljs
//! i prioritetsordning istället för att överlappa.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::*;

use crate::doc::{Next, SCAN_BUDGET, TAB};
use crate::editor::Tab;
use crate::search::Matcher;

// VoidFlow / JOURNAL
const BG: u32 = 0x050505;
const HEADER: u32 = 0x0e0f13;
const PANEL: u32 = 0x0b0b0e;
const TEXT: u32 = 0xd6dbe0;
const MUTED: u32 = 0x7d8894;
const GREEN: u32 = 0x00ff41;
const RED: u32 = 0xd70a53;
const RED2: u32 = 0xff3d7f;

/// Minsta gutterbredd i siffror.
pub const MIN_DIGITS: usize = 3;

fn col(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

pub fn digits(n: u64) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// Visar slutet av `s` i högst `n` tecken ("…" först om avkortad).
fn tail(s: &str, n: usize) -> String {
    let k = s.chars().count();
    if k <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    std::iter::once('…')
        .chain(s.chars().skip(k - (n - 1)))
        .collect()
}

pub fn human(n: usize) -> String {
    const U: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub tab_h: f32,
    pub status_h: f32,
    /// Sidpadding.
    pub x0: f32,
    /// Översta textradens y.
    pub y0: f32,
    /// Guttersiffrornas vänsterkant, separatorlinjen, textens vänsterkant.
    pub gx: f32,
    pub sep: f32,
    pub tx: f32,
    pub digits: usize,
    /// Sökraden (bar_h = 0 när stängd).
    pub bar_y: f32,
    pub bar_h: f32,
    pub rows: usize,
    pub cols: usize,
}

struct Brushes {
    header: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    bg: ID2D1SolidColorBrush,
    text: ID2D1SolidColorBrush,
    muted: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
    red2: ID2D1SolidColorBrush,
    line_g: ID2D1SolidColorBrush,
    focus: ID2D1SolidColorBrush,
    scan: ID2D1SolidColorBrush,
    sel: ID2D1SolidColorBrush,
    hit: ID2D1SolidColorBrush,
    badge_g: ID2D1SolidColorBrush,
    badge_r: ID2D1SolidColorBrush,
    badge_t: ID2D1SolidColorBrush,
    cur_line: ID2D1SolidColorBrush,
}

struct Res {
    rt: ID2D1HwndRenderTarget,
    b: Brushes,
}

pub struct Gfx {
    factory: ID2D1Factory,
    mono: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    pub cw: f32,
    pub lh: f32,
    text_off: f32,
    logo_w: f32,
    pub dpi: f32,
    res: Option<Res>,
    /// Första synliga flik i flikraden (klistrig: ändras bara när aktiv flik annars döljs).
    tab_first: usize,
}

/// Sökradens innehåll (byggs av App).
pub struct BarView<'a> {
    pub label: &'a str,
    pub input: &'a str,
    pub focus: bool,
    /// Hela inmatningen markerad (nästa tecken ersätter).
    pub fresh: bool,
    /// Some(skiftlägeskänslig) i sökläge.
    pub case: Option<bool>,
    pub info: String,
    pub bad: bool,
    pub hint: &'a str,
}

pub struct View<'a> {
    pub status: &'a str,
    pub status_bad: bool,
    pub wal_failed: bool,
    pub bar: Option<BarView<'a>>,
    pub matcher: Option<&'a Matcher>,
    /// Rita stavfel.
    pub spell: bool,
    /// Statusradens stavningssegment ("STAV SV+EN", "STAV AV" …).
    pub spell_label: &'a str,
}

/// Statusradssegment. Lägre prio behålls längst.
struct StatSeg<'b> {
    text: String,
    fg: &'b ID2D1SolidColorBrush,
    badge: Option<&'b ID2D1SolidColorBrush>,
    prio: u8,
}

impl StatSeg<'_> {
    /// Bredd i teckenceller (badge = +2 för padding).
    fn cells(&self) -> usize {
        self.text.chars().count() + if self.badge.is_some() { 2 } else { 0 }
    }
}

const SEP: &str = "  //  ";

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn measure(dw: &IDWriteFactory, fmt: &IDWriteTextFormat, s: &str) -> Result<DWRITE_TEXT_METRICS> {
    let mut m = DWRITE_TEXT_METRICS::default();
    unsafe {
        let l = dw.CreateTextLayout(&utf16(s), fmt, 4096.0, 4096.0)?;
        l.GetMetrics(&mut m)?;
    }
    Ok(m)
}

fn pick_family(dw: &IDWriteFactory) -> PCWSTR {
    unsafe {
        let mut coll: Option<IDWriteFontCollection> = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_ok()
            && let Some(c) = coll
        {
            let (mut idx, mut exists) = (0u32, BOOL(0));
            if c.FindFamilyName(w!("JetBrains Mono"), &mut idx, &mut exists)
                .is_ok()
                && exists.as_bool()
            {
                return w!("JetBrains Mono");
            }
        }
    }
    w!("Consolas")
}

impl Gfx {
    pub fn new(hwnd: HWND) -> Result<Gfx> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let fam = pick_family(&dw);
            let mono = dw.CreateTextFormat(
                fam,
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                14.0,
                w!("sv-se"),
            )?;
            mono.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let bold = dw.CreateTextFormat(
                fam,
                None,
                DWRITE_FONT_WEIGHT_EXTRA_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                w!("sv-se"),
            )?;
            bold.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

            let m = measure(&dw, &mono, "0000000000")?;
            let cw = m.widthIncludingTrailingWhitespace / 10.0;
            let lh = (m.height * 1.35).ceil();
            let logo_w = measure(&dw, &bold, "VoidFlow")?.widthIncludingTrailingWhitespace;
            Ok(Gfx {
                factory,
                mono,
                bold,
                cw,
                lh,
                text_off: ((lh - m.height) / 2.0).floor(),
                logo_w,
                dpi: GetDpiForWindow(hwnd).max(96) as f32,
                res: None,
                tab_first: 0,
            })
        }
    }

    fn client_px(hwnd: HWND) -> (u32, u32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        (
            (rc.right - rc.left).max(1) as u32,
            (rc.bottom - rc.top).max(1) as u32,
        )
    }

    /// `digits` = gutterns sifferbredd, `bar` = sökraden öppen.
    pub fn layout(&self, hwnd: HWND, digits: usize, bar: bool) -> Layout {
        let (pw, ph) = Self::client_px(hwnd);
        let s = 96.0 / self.dpi;
        let (w, h) = (pw as f32 * s, ph as f32 * s);
        let tab_h = self.lh + 22.0;
        let status_h = self.lh + 14.0;
        let x0 = 22.0;
        let y0 = tab_h + 12.0;
        let digits = digits.max(MIN_DIGITS);
        let gx = x0;
        let sep = gx + digits as f32 * self.cw + 14.0;
        let tx = sep + 15.0;
        let bar_h = if bar { self.lh + 18.0 } else { 0.0 };
        let bar_y = h - status_h - bar_h;
        let rows = ((bar_y - y0 - 8.0) / self.lh).floor().max(1.0) as usize;
        let cols = ((w - tx - x0) / self.cw).floor().max(1.0) as usize;
        Layout {
            w,
            h,
            tab_h,
            status_h,
            x0,
            y0,
            gx,
            sep,
            tx,
            digits,
            bar_y,
            bar_h,
            rows,
            cols,
        }
    }

    pub fn resize(&mut self, hwnd: HWND) {
        if let Some(r) = &self.res {
            let (w, h) = Self::client_px(hwnd);
            if unsafe {
                r.rt.Resize(&D2D_SIZE_U {
                    width: w,
                    height: h,
                })
            }
            .is_err()
            {
                self.res = None;
            }
        }
    }

    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
        if let Some(r) = &self.res {
            unsafe { r.rt.SetDpi(dpi, dpi) };
        }
    }

    fn create_res(&self, hwnd: HWND) -> Result<Res> {
        let (w, h) = Self::client_px(hwnd);
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_UNKNOWN,
            },
            dpiX: self.dpi,
            dpiY: self.dpi,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U {
                width: w,
                height: h,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        unsafe {
            let rt = self.factory.CreateHwndRenderTarget(&props, &hp)?;
            let br = |hex: u32, a: f32| rt.CreateSolidColorBrush(&col(hex, a), None);
            let b = Brushes {
                header: br(HEADER, 1.0)?,
                panel: br(PANEL, 1.0)?,
                bg: br(BG, 1.0)?,
                text: br(TEXT, 1.0)?,
                muted: br(MUTED, 1.0)?,
                green: br(GREEN, 1.0)?,
                red: br(RED, 1.0)?,
                red2: br(RED2, 1.0)?,
                line_g: br(GREEN, 0.16)?,
                focus: br(GREEN, 0.45)?,
                scan: br(0x000000, 0.22)?,
                sel: br(GREEN, 0.20)?,
                hit: br(RED2, 0.28)?,
                badge_g: br(GREEN, 0.13)?,
                badge_r: br(RED2, 0.18)?,
                badge_t: br(TEXT, 0.10)?,
                cur_line: br(GREEN, 0.05)?,
            };
            Ok(Res { rt, b })
        }
    }

    fn text(
        &self,
        rt: &ID2D1HwndRenderTarget,
        s: &[u16],
        x: f32,
        y: f32,
        fmt: &IDWriteTextFormat,
        b: &ID2D1SolidColorBrush,
    ) {
        if s.is_empty() {
            return;
        }
        unsafe {
            rt.DrawText(
                s,
                fmt,
                &rect(x, y + self.text_off, x + 8192.0, y + self.lh),
                b,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Text klippt till [x, right) – skydd mot fallback-glyfer som är bredare än cw.
    #[allow(clippy::too_many_arguments)]
    fn text_clip(
        &self,
        rt: &ID2D1HwndRenderTarget,
        s: &str,
        x: f32,
        y: f32,
        right: f32,
        fmt: &IDWriteTextFormat,
        b: &ID2D1SolidColorBrush,
    ) {
        if s.is_empty() || right <= x {
            return;
        }
        unsafe {
            rt.PushAxisAlignedClip(&rect(x, y, right, y + self.lh), D2D1_ANTIALIAS_MODE_ALIASED);
            self.text(rt, &utf16(s), x, y, fmt, b);
            rt.PopAxisAlignedClip();
        }
    }

    /// Ritar allt. Returnerar flikarnas x-intervall för hit-test.
    pub fn draw(
        &mut self,
        hwnd: HWND,
        tabs: &[Tab],
        active: usize,
        v: &View,
    ) -> Vec<(f32, f32, usize)> {
        if self.res.is_none() {
            match self.create_res(hwnd) {
                Ok(r) => self.res = Some(r),
                Err(_) => return Vec::new(),
            }
        }
        let t = &tabs[active];
        let l = self.layout(hwnd, t.gutter, v.bar.is_some());
        self.tab_first = self.tab_window(&l, tabs, active);
        let res = self.res.as_ref().unwrap();
        let rt = &res.rt;
        let hits;
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&col(BG, 1.0)));
            hits = self.draw_tabs(rt, &res.b, &l, tabs, active);
            self.draw_text(rt, &res.b, &l, t, v.matcher, v.spell);
            if let Some(bar) = &v.bar {
                self.draw_bar(rt, &res.b, &l, bar);
            }
            self.draw_status(rt, &res.b, &l, t, v);
            if rt.EndDraw(None, None).is_err() {
                self.res = None;
            }
        }
        hits
    }

    // ------------------------------------------------------------ flikrad

    /// Fliktitel, kapad så att en enskild flik aldrig kan tränga undan resten.
    fn tab_title(t: &Tab) -> String {
        const MAX: usize = 40;
        let s = t.title();
        if s.chars().count() <= MAX {
            s
        } else {
            s.chars()
                .take(MAX - 1)
                .chain(std::iter::once('…'))
                .collect()
        }
    }

    fn tab_w(&self, t: &Tab) -> f32 {
        (Self::tab_title(t).chars().count() as f32 + if t.dirty { 2.0 } else { 0.0 }) * self.cw
            + 28.0
    }

    fn more_w(&self, s: &str) -> f32 {
        (s.chars().count() as f32 + 1.0) * self.cw + 12.0
    }

    fn show_logo(&self, l: &Layout) -> (bool, f32) {
        let lx = l.w - l.x0 - self.logo_w - self.cw * 9.0;
        (lx > l.x0 + self.cw * 16.0, lx)
    }

    fn tab_limit(&self, l: &Layout) -> f32 {
        match self.show_logo(l) {
            (true, lx) => lx - 24.0,
            (false, _) => l.w - l.x0,
        }
    }

    /// Första synliga flik så att den aktiva alltid syns. Behåller föregående läge
    /// om det räcker; annars läggs aktiv flik längst till höger (eller vänster när
    /// man går bakåt). Kostar O(synliga flikar) oavsett antal flikar.
    fn tab_window(&self, l: &Layout, tabs: &[Tab], active: usize) -> usize {
        let n = tabs.len();
        if n == 0 {
            return 0;
        }
        let start = l.x0 - 8.0;
        let limit = self.tab_limit(l);
        let right = |i: usize| {
            if i + 1 < n {
                self.more_w(&format!("+{}", n - i - 1))
            } else {
                0.0
            }
        };
        let left = |f: usize| {
            if f > 0 {
                self.more_w(&format!("‹{f}"))
            } else {
                0.0
            }
        };
        let prev = self.tab_first;
        if prev <= active {
            let mut x = start + left(prev);
            let mut ok = true;
            for t in &tabs[prev..=active] {
                x += self.tab_w(t) + 4.0;
                if x > limit {
                    ok = false;
                    break;
                }
            }
            if ok && x - 4.0 + right(active) <= limit {
                return prev;
            }
        } else {
            return active;
        }
        // Bakåt från aktiv flik tills raden är full.
        let mut used = self.tab_w(&tabs[active]) + right(active);
        let mut first = active;
        while first > 0 {
            let w = self.tab_w(&tabs[first - 1]) + 4.0;
            if start + left(first - 1) + used + w > limit {
                break;
            }
            used += w;
            first -= 1;
        }
        first
    }

    unsafe fn draw_tabs(
        &self,
        rt: &ID2D1HwndRenderTarget,
        b: &Brushes,
        l: &Layout,
        tabs: &[Tab],
        active: usize,
    ) -> Vec<(f32, f32, usize)> {
        let mut hits = Vec::new();
        unsafe {
            // enda CRT-ytan
            rt.FillRectangle(&rect(0.0, 0.0, l.w, l.tab_h), &b.header);
            let mut y = 0.0;
            while y < l.tab_h {
                rt.FillRectangle(&rect(0.0, y + 2.0, l.w, y + 3.0), &b.scan);
                y += 3.0;
            }
            rt.FillRectangle(&rect(0.0, l.tab_h - 1.0, l.w, l.tab_h), &b.line_g);
            rt.FillRectangle(&rect(0.0, l.tab_h - 2.0, l.w * 0.48, l.tab_h), &b.red);
        }

        // logga (döljs hellre än att krocka med flikarna i smala fönster)
        let ty = (l.tab_h - self.lh) / 2.0;
        let (show_logo, lx) = self.show_logo(l);
        if show_logo {
            self.text(rt, &utf16("VoidFlow"), lx, ty - 1.0, &self.bold, &b.green);
            self.text(rt, &utf16(" //"), lx + self.logo_w, ty, &self.mono, &b.red2);
            self.text(
                rt,
                &utf16(" pytte"),
                lx + self.logo_w + self.cw * 3.0,
                ty,
                &self.mono,
                &b.muted,
            );
        }

        let first = self.tab_first.min(tabs.len().saturating_sub(1));
        let mut x = l.x0 - 8.0;
        let limit = self.tab_limit(l);
        // Dolda flikar till vänster: "‹N", klick väljer närmaste dolda.
        if first > 0 {
            let s = format!("‹{first}");
            let w = self.more_w(&s);
            self.text(rt, &utf16(&s), x + 6.0, ty, &self.mono, &b.muted);
            hits.push((x, x + w, first - 1));
            x += w;
        }
        for (i, t) in tabs.iter().enumerate().skip(first) {
            let title = Self::tab_title(t);
            let tw = self.tab_w(t);
            // Reservera plats för "+N" så att räknaren aldrig hamnar under loggan.
            let more = format!("+{}", tabs.len() - i);
            let reserve = if i + 1 < tabs.len() {
                self.more_w(&more)
            } else {
                0.0
            };
            if x + tw + reserve > limit && i != active {
                self.text(rt, &utf16(&more), x + 6.0, ty, &self.mono, &b.muted);
                hits.push((x, x + self.more_w(&more), i));
                break;
            }
            unsafe {
                if i == active {
                    rt.FillRectangle(&rect(x, 6.0, x + tw, l.tab_h - 2.0), &b.panel);
                    rt.FillRectangle(&rect(x, l.tab_h - 3.0, x + tw, l.tab_h - 1.0), &b.green);
                }
            }
            let fg = if i == active { &b.green } else { &b.muted };
            self.text_clip(rt, &title, x + 14.0, ty, x + tw - 4.0, &self.mono, fg);
            if t.dirty {
                let dx = x + 14.0 + (title.chars().count() as f32 + 1.0) * self.cw;
                if dx + self.cw <= x + tw {
                    self.text(rt, &utf16("\u{25CF}"), dx, ty, &self.mono, &b.red2);
                }
            }
            hits.push((x, x + tw, i));
            x += tw + 4.0;
        }
        hits
    }

    // ------------------------------------------------------------ text + gutter

    unsafe fn draw_text(
        &self,
        rt: &ID2D1HwndRenderTarget,
        b: &Brushes,
        l: &Layout,
        t: &Tab,
        matcher: Option<&Matcher>,
        spell: bool,
    ) {
        let d = &t.doc;
        let text_bottom = l.y0 + l.rows as f32 * self.lh;
        unsafe {
            rt.FillRectangle(
                &rect(l.sep, l.y0 - 4.0, l.sep + 1.0, text_bottom + 4.0),
                &b.line_g,
            );
        }
        if t.loading {
            self.text(
                rt,
                &utf16(&format!("LADDAR {} ...", t.title())),
                l.tx,
                l.y0,
                &self.mono,
                &b.muted,
            );
            return;
        }

        let top_line = d.line_of(t.top);
        let cur_line = d.line_of(t.cursor);
        let sel = t.sel();
        // Matchande parenteser (endast utan markering), ritas som ram runt tecknet.
        let br = if sel.is_none() {
            d.match_bracket(t.cursor)
        } else {
            None
        };
        let mut br_cells: Vec<(usize, usize)> = Vec::with_capacity(2);
        let mlen = matcher.map_or(0, |m| m.len());
        let mut ls = t.top;
        let mut cursor_row = None;
        let mut buf: Vec<u16> = Vec::with_capacity(l.cols * 2 + 2);
        let mut mr: Vec<(usize, usize)> = Vec::new();
        let mut mcols: Vec<(usize, usize, bool)> = Vec::new();
        let mut sr: Vec<(usize, usize)> = Vec::new();
        let mut scols: Vec<(usize, usize)> = Vec::new();
        let max_scan = (t.hscroll + l.cols) * 4 + 4096;
        // Rå radslutsskanning (oindexerat område) delar en budget per frame.
        let mut budget = 2 * SCAN_BUDGET;
        for row in 0..l.rows {
            let ry = l.y0 + row as f32 * self.lh;
            let nx = d.next_line_bounded(ls, budget.max(4096));
            if let Next::At(n) = nx {
                budget = budget.saturating_sub(n - ls);
            }
            let next = match nx {
                Next::At(n) => Some(n),
                _ => None,
            };
            let unknown = matches!(nx, Next::Unknown);
            if t.cursor >= ls && next.is_none_or(|n| t.cursor < n) {
                cursor_row = Some(row);
                if sel.is_none() {
                    unsafe {
                        rt.FillRectangle(&rect(l.sep + 1.0, ry, l.w, ry + self.lh), &b.cur_line);
                    }
                }
            }

            // ---- radnummer
            let num = top_line.map(|tl| tl + row as u64 + 1);
            let is_cur = num.is_some() && num.map(|n| n - 1) == cur_line;
            let s = num.map_or_else(|| "~".to_string(), |n| n.to_string());
            let nx_x = l.gx + (l.digits as f32 - s.len() as f32) * self.cw;
            self.text(
                rt,
                &utf16(&s),
                nx_x,
                ry,
                &self.mono,
                if is_cur { &b.green } else { &b.muted },
            );

            // ---- träffar på raden (byteintervall)
            mr.clear();
            mcols.clear();
            if let Some(m) = matcher {
                let lim = next.unwrap_or(if unknown { usize::MAX } else { d.len() });
                let end = lim.min(ls + max_scan + mlen).min(d.len());
                let bytes = d.slice(ls, end);
                m.each(&bytes, |i| {
                    mr.push((ls + i, ls + i + mlen));
                    mr.len() < 4096
                });
            }

            // ---- stavfel på raden. Ordet som skrivs just nu (markören i slutet) väntar.
            sr.clear();
            scols.clear();
            if spell && !t.spell.list.is_empty() {
                let lim = next.unwrap_or(ls + max_scan);
                let i0 = t.spell.list.partition_point(|m| m.1 <= ls);
                for &(s, e) in &t.spell.list[i0..] {
                    if s >= lim || sr.len() >= 512 {
                        break;
                    }
                    if sel.is_none() && e == t.cursor {
                        continue;
                    }
                    sr.push((s, e));
                }
            }

            // ---- tecken → kolumner
            buf.clear();
            let (mut c, mut p) = (0usize, ls);
            let (mut s0, mut s1): (Option<usize>, Option<usize>) = (None, None);
            let mut mi = 0usize;
            let mut m0: Option<usize> = None;
            let mut si = 0usize;
            let mut sp0: Option<usize> = None;
            let mut eol = false;
            let mut chars = d.chars(ls);
            loop {
                if let Some((a, z)) = sel {
                    if s0.is_none() && p >= a && p < z {
                        s0 = Some(c);
                    }
                    if s0.is_some() && s1.is_none() && p >= z {
                        s1 = Some(c);
                    }
                }
                if let Some((p0, p1)) = br
                    && (p == p0 || p == p1)
                    && c >= t.hscroll
                {
                    br_cells.push((row, c));
                }
                while mi < mr.len() {
                    let (ms, me) = mr[mi];
                    if m0.is_none() {
                        if p >= ms {
                            m0 = Some(c);
                        } else {
                            break;
                        }
                    }
                    if p >= me {
                        mcols.push((m0.take().unwrap(), c, sel == Some((ms, me))));
                        mi += 1;
                        continue;
                    }
                    break;
                }
                while si < sr.len() {
                    let (ss, se) = sr[si];
                    if sp0.is_none() {
                        if p >= ss {
                            sp0 = Some(c);
                        } else {
                            break;
                        }
                    }
                    if p >= se {
                        scols.push((sp0.take().unwrap(), c));
                        si += 1;
                        continue;
                    }
                    break;
                }
                let Some((ch, n)) = chars.next() else {
                    eol = true;
                    break;
                };
                if p - ls > max_scan {
                    break;
                }
                let ch = match ch {
                    Some('\n') => {
                        eol = true;
                        break;
                    }
                    Some('\r') if d.byte(p + 1) == Some(b'\n') => {
                        eol = true;
                        break;
                    }
                    Some('\t') => '\t',
                    Some(ch) if (ch as u32) < 0x20 || ('\u{7f}'..='\u{9f}').contains(&ch) => {
                        '\u{b7}'
                    }
                    Some(ch) => ch,
                    None => '\u{b7}',
                };
                let (w, glyph) = if ch == '\t' {
                    (TAB - c % TAB, ' ')
                } else {
                    (1, ch)
                };
                let mut u = [0u16; 2];
                for _ in 0..w {
                    if c >= t.hscroll {
                        buf.extend_from_slice(glyph.encode_utf16(&mut u));
                    }
                    c += 1;
                }
                p += n;
                if c >= t.hscroll + l.cols {
                    break;
                }
            }
            if let Some(c0) = m0 {
                let cur = mr.get(mi).is_some_and(|&r| sel == Some(r));
                mcols.push((c0, c, cur));
            }
            if let Some(c0) = sp0 {
                scols.push((c0, c));
            }
            if s0.is_some() && s1.is_none() {
                // Markeringen fortsätter förbi radslutet (eller utanför skärmen).
                s1 = Some(if eol { c + 1 } else { c });
            }

            let xc = |c: usize| l.tx + c.saturating_sub(t.hscroll) as f32 * self.cw;
            unsafe {
                for &(c0, c1, cur) in &mcols {
                    if c1 <= t.hscroll {
                        continue;
                    }
                    let (x0, x1) = (xc(c0), xc(c1));
                    rt.FillRectangle(&rect(x0, ry + 1.0, x1, ry + self.lh - 1.0), &b.hit);
                    let ul = if cur { &b.green } else { &b.red2 };
                    rt.FillRectangle(&rect(x0, ry + self.lh - 3.0, x1, ry + self.lh - 1.0), ul);
                }
                if let (Some(c0), Some(c1)) = (s0, s1)
                    && c1 > t.hscroll
                {
                    rt.FillRectangle(&rect(xc(c0), ry, xc(c1), ry + self.lh), &b.sel);
                }
                for &(c0, c1) in &scols {
                    if c1 > t.hscroll {
                        self.squiggle(rt, xc(c0), xc(c1), ry + self.lh - 1.0, &b.red2);
                    }
                }
            }
            self.text(rt, &buf, l.tx, ry, &self.mono, &b.text);

            match nx {
                Next::At(n) => ls = n,
                Next::End => break,
                Next::Unknown => {
                    // Raden fortsätter bortom skanningsbudgeten och indexet har
                    // inte nått fram än: rita inga gissade rader.
                    if row + 1 < l.rows {
                        let pct = d.index_progress().map_or(100.0, |p| p * 100.0);
                        self.text(
                            rt,
                            &utf16(&format!("… LÅNG RAD, INDEXERAR {pct:.0} %")),
                            l.tx,
                            ry + self.lh,
                            &self.mono,
                            &b.muted,
                        );
                    }
                    break;
                }
            }
        }

        // ---- parentesramar (fyllning + 1 px kant; tecknet syns igenom)
        for &(row, c) in &br_cells {
            let x0 = l.tx + (c - t.hscroll) as f32 * self.cw;
            let y0 = l.y0 + row as f32 * self.lh + 1.0;
            let (x1, y1) = (x0 + self.cw, y0 + self.lh - 2.0);
            unsafe {
                rt.FillRectangle(&rect(x0, y0, x1, y1), &b.badge_g);
                rt.FillRectangle(&rect(x0, y0, x1, y0 + 1.0), &b.focus);
                rt.FillRectangle(&rect(x0, y1 - 1.0, x1, y1), &b.focus);
                rt.FillRectangle(&rect(x0, y0, x0 + 1.0, y1), &b.focus);
                rt.FillRectangle(&rect(x1 - 1.0, y0, x1, y1), &b.focus);
            }
        }

        // ---- markör
        if let Some(row) = cursor_row {
            let vc = d.vcol(t.cursor);
            if vc >= t.hscroll {
                let cx = l.tx + (vc - t.hscroll) as f32 * self.cw;
                let cy = l.y0 + row as f32 * self.lh;
                unsafe {
                    rt.FillRectangle(&rect(cx, cy + 2.0, cx + 2.0, cy + self.lh - 2.0), &b.green);
                }
            }
        }
    }

    /// Vågig understrykning [x0, x1) med nederkant `y`. Ritas före texten så
    /// att den aldrig täcker ett tecken.
    unsafe fn squiggle(
        &self,
        rt: &ID2D1HwndRenderTarget,
        x0: f32,
        x1: f32,
        y: f32,
        br: &ID2D1SolidColorBrush,
    ) {
        const STEP: f32 = 2.0;
        let mut x = x0;
        let mut up = false;
        while x < x1 {
            let xe = (x + STEP).min(x1);
            let yt = if up { y - 2.5 } else { y - 1.5 };
            unsafe {
                rt.FillRectangle(&rect(x, yt, xe, yt + 1.25), br);
            }
            up = !up;
            x = xe;
        }
    }

    // ------------------------------------------------------------ sökrad

    unsafe fn draw_bar(&self, rt: &ID2D1HwndRenderTarget, b: &Brushes, l: &Layout, v: &BarView) {
        let y = l.bar_y;
        let ty = y + (l.bar_h - self.lh) / 2.0;
        unsafe {
            rt.FillRectangle(&rect(0.0, y, l.w, y + l.bar_h), &b.header);
            rt.FillRectangle(&rect(0.0, y, l.w, y + 1.0), &b.line_g);
        }
        let cw = self.cw;
        let avail = ((l.w - 2.0 * l.x0) / cw).floor().max(0.0) as usize;

        // Bredder i teckenceller.
        let label = format!("{} ›", v.label);
        let label_n = label.chars().count() + 2;
        let case_n = if v.case.is_some() { 4 + 2 } else { 0 };
        let info_n = v.info.chars().count().max(10) + 2;
        let hint_n = v.hint.chars().count() + 2;
        let fixed = label_n + case_n + info_n;
        let field_min = 12;
        let show_hint = avail >= fixed + 30 + hint_n;
        let field_n = avail
            .saturating_sub(fixed + if show_hint { hint_n } else { 0 })
            .max(field_min);

        let mut x = l.x0;
        self.text(rt, &utf16(v.label), x, ty, &self.mono, &b.green);
        self.text(
            rt,
            &utf16(" ›"),
            x + v.label.chars().count() as f32 * cw,
            ty,
            &self.mono,
            &b.red2,
        );
        x += label_n as f32 * cw;

        // Fält
        let fx0 = x;
        let fx1 = x + field_n as f32 * cw;
        unsafe {
            rt.FillRectangle(&rect(fx0, y + 5.0, fx1, y + l.bar_h - 5.0), &b.bg);
            let edge = if v.focus { &b.focus } else { &b.line_g };
            rt.FillRectangle(&rect(fx0, y + l.bar_h - 6.0, fx1, y + l.bar_h - 5.0), edge);
        }
        let inner = field_n.saturating_sub(2);
        let shown = tail(v.input, inner);
        let ix = fx0 + cw;
        let sw = shown.chars().count() as f32 * cw;
        if v.fresh && !shown.is_empty() {
            unsafe { rt.FillRectangle(&rect(ix, ty + 2.0, ix + sw, ty + self.lh - 2.0), &b.sel) };
        }
        self.text_clip(rt, &shown, ix, ty, fx1 - cw * 0.5, &self.mono, &b.text);
        if v.focus {
            let cx = (ix + sw).min(fx1 - cw);
            unsafe {
                rt.FillRectangle(&rect(cx, ty + 2.0, cx + 2.0, ty + self.lh - 2.0), &b.green)
            };
        }
        x = fx1 + 2.0 * cw;

        // Skiftlägesindikator: tänd = skiftlägeskänslig.
        if let Some(sensitive) = v.case {
            let (fg, bg) = if sensitive {
                (&b.green, &b.badge_g)
            } else {
                (&b.muted, &b.badge_t)
            };
            unsafe { rt.FillRectangle(&rect(x, ty + 1.0, x + 4.0 * cw, ty + self.lh - 1.0), bg) };
            self.text(rt, &utf16("Aa"), x + cw, ty, &self.mono, fg);
            x += case_n as f32 * cw;
        }

        let fg = if v.bad { &b.red2 } else { &b.text };
        self.text_clip(
            rt,
            &v.info,
            x,
            ty,
            x + (info_n - 2) as f32 * cw + 1.0,
            &self.mono,
            fg,
        );
        x += info_n as f32 * cw;

        if show_hint {
            let hx = l.w - l.x0 - v.hint.chars().count() as f32 * cw;
            self.text(rt, &utf16(v.hint), hx.max(x), ty, &self.mono, &b.muted);
        }
    }

    // ------------------------------------------------------------ statusrad

    unsafe fn draw_status(
        &self,
        rt: &ID2D1HwndRenderTarget,
        b: &Brushes,
        l: &Layout,
        t: &Tab,
        v: &View,
    ) {
        let sy = l.h - l.status_h;
        unsafe {
            rt.FillRectangle(&rect(0.0, sy, l.w, l.h), &b.panel);
            rt.FillRectangle(&rect(0.0, sy, l.w, sy + 1.0), &b.line_g);
        }
        let ty = sy + (l.status_h - self.lh) / 2.0;
        let d = &t.doc;

        let mut segs: Vec<StatSeg> = Vec::new();
        let (state, fg, bg) = if t.loading {
            ("LADDAR", &b.text, &b.badge_t)
        } else if t.dirty {
            ("ÄNDRAD", &b.red2, &b.badge_r)
        } else {
            ("SPARAD", &b.green, &b.badge_g)
        };
        segs.push(StatSeg {
            text: state.into(),
            fg,
            badge: Some(bg),
            prio: 0,
        });
        if v.wal_failed {
            segs.push(StatSeg {
                text: "WAL-FEL: SESSIONEN SKRIVS INTE".into(),
                fg: &b.red2,
                badge: Some(&b.badge_r),
                prio: 0,
            });
        }
        if !t.loading {
            let line = d
                .line_of(t.cursor)
                .map_or_else(|| "~".to_string(), |n| (n + 1).to_string());
            let total = d
                .lines()
                .map_or_else(|| format!("~{}", d.est_lines()), |n| n.to_string());
            segs.push(StatSeg {
                text: format!("RAD {line} / {total}"),
                fg: &b.text,
                badge: None,
                prio: 1,
            });
            segs.push(StatSeg {
                text: format!("KOL {}", d.vcol(t.cursor) + 1),
                fg: &b.text,
                badge: None,
                prio: 1,
            });
            if let Some((a, z)) = t.sel() {
                let rows = match (d.line_of(a), d.line_of(z)) {
                    (Some(x), Some(y)) if y > x => format!(" · {} RADER", y - x + 1),
                    _ => String::new(),
                };
                segs.push(StatSeg {
                    text: format!("SEL {}{rows}", human(z - a)),
                    fg: &b.green,
                    badge: None,
                    prio: 2,
                });
            }
            if let Some(p) = d.index_progress() {
                segs.push(StatSeg {
                    text: format!("RADINDEX {:.0} %", p * 100.0),
                    fg: &b.muted,
                    badge: None,
                    prio: 2,
                });
            }
            segs.push(StatSeg {
                text: if t.crlf { "CRLF" } else { "LF" }.into(),
                fg: &b.muted,
                badge: None,
                prio: 3,
            });
            segs.push(StatSeg {
                text: v.spell_label.into(),
                fg: &b.muted,
                badge: None,
                prio: 4,
            });
            segs.push(StatSeg {
                text: "UTF-8".into(),
                fg: &b.muted,
                badge: None,
                prio: 5,
            });
            segs.push(StatSeg {
                text: human(d.len()),
                fg: &b.muted,
                badge: None,
                prio: 4,
            });
        }
        let path = t.path.as_deref().unwrap_or("NAMNLÖS").to_string();

        // ---- passning: inget segment får överlappa ett annat
        let avail = ((l.w - 2.0 * l.x0) / self.cw).floor().max(0.0) as usize;
        let sep_n = SEP.chars().count();
        let left_n = |segs: &[StatSeg]| -> usize {
            segs.iter().map(|s| s.cells()).sum::<usize>() + sep_n * segs.len().saturating_sub(1)
        };
        let mut msg = v.status.to_string();
        let msg_n = |m: &str| {
            if m.is_empty() {
                0
            } else {
                m.chars().count() + 4
            }
        };
        // Felmeddelanden får företräde framför sökvägen.
        let mut path_room = avail.saturating_sub(left_n(&segs) + sep_n + msg_n(&msg));
        if path_room < 8 && !msg.is_empty() && !v.status_bad {
            let room = avail.saturating_sub(left_n(&segs) + sep_n + 8 + 4);
            msg = if room >= 12 {
                tail(&msg, room)
            } else {
                String::new()
            };
            path_room = avail.saturating_sub(left_n(&segs) + sep_n + msg_n(&msg));
        }
        if path_room >= 8 {
            segs.push(StatSeg {
                text: tail(&path, path_room),
                fg: &b.muted,
                badge: None,
                prio: 6,
            });
        }
        if left_n(&segs) + msg_n(&msg) > avail && !msg.is_empty() {
            let room = avail.saturating_sub(left_n(&segs) + 4);
            msg = if room >= 12 {
                tail(&msg, room)
            } else {
                String::new()
            };
        }
        while left_n(&segs) + msg_n(&msg) > avail && segs.len() > 1 {
            // Släpp segmentet med högst prio-nummer (sist vid lika).
            let (i, _) = segs
                .iter()
                .enumerate()
                .max_by_key(|(i, s)| (s.prio, *i))
                .unwrap();
            segs.remove(i);
        }

        let cw = self.cw;
        let mut x = l.x0;
        for (i, s) in segs.iter().enumerate() {
            if i > 0 {
                self.text(rt, &utf16(SEP), x, ty, &self.mono, &b.muted);
                x += sep_n as f32 * cw;
            }
            if let Some(bg) = s.badge {
                let w = s.cells() as f32 * cw;
                unsafe { rt.FillRectangle(&rect(x, sy + 5.0, x + w, l.h - 5.0), bg) };
                self.text(rt, &utf16(&s.text), x + cw, ty, &self.mono, s.fg);
            } else {
                self.text(rt, &utf16(&s.text), x, ty, &self.mono, s.fg);
            }
            x += s.cells() as f32 * cw;
        }
        if !msg.is_empty() {
            let mx = l.w - l.x0 - msg.chars().count() as f32 * cw;
            let fg = if v.status_bad { &b.red2 } else { &b.muted };
            self.text_clip(
                rt,
                &msg,
                mx.max(x + 4.0 * cw),
                ty,
                l.w - l.x0 + cw,
                &self.mono,
                fg,
            );
        }
    }
}
