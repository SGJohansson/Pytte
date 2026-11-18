//! Direct2D + DirectWrite. Ritar bara synliga rader (virtualiserat från `top`).

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::*;

use crate::doc::TAB;
use crate::editor::Tab;

// VoidFlow / JOURNAL
const BG: u32 = 0x050505;
const HEADER: u32 = 0x0e0f13;
const PANEL: u32 = 0x0b0b0e;
const TEXT: u32 = 0xd6dbe0;
const MUTED: u32 = 0x7d8894;
const GREEN: u32 = 0x00ff41;
const RED: u32 = 0xd70a53;
const RED2: u32 = 0xff3d7f;

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

pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub tab_h: f32,
    pub status_h: f32,
    pub x0: f32,
    pub y0: f32,
    pub rows: usize,
    pub cols: usize,
}

struct Brushes {
    header: ID2D1SolidColorBrush,
    panel: ID2D1SolidColorBrush,
    text: ID2D1SolidColorBrush,
    muted: ID2D1SolidColorBrush,
    green: ID2D1SolidColorBrush,
    red: ID2D1SolidColorBrush,
    red2: ID2D1SolidColorBrush,
    line_g: ID2D1SolidColorBrush,
    scan: ID2D1SolidColorBrush,
    sel: ID2D1SolidColorBrush,
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
}

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

    pub fn layout(&self, hwnd: HWND) -> Layout {
        let (pw, ph) = Self::client_px(hwnd);
        let s = 96.0 / self.dpi;
        let (w, h) = (pw as f32 * s, ph as f32 * s);
        let tab_h = self.lh + 22.0;
        let status_h = self.lh + 14.0;
        let x0 = 22.0;
        let y0 = tab_h + 12.0;
        let rows = ((h - y0 - status_h - 8.0) / self.lh).floor().max(1.0) as usize;
        let cols = ((w - x0 * 2.0) / self.cw).floor().max(1.0) as usize;
        Layout {
            w,
            h,
            tab_h,
            status_h,
            x0,
            y0,
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
                text: br(TEXT, 1.0)?,
                muted: br(MUTED, 1.0)?,
                green: br(GREEN, 1.0)?,
                red: br(RED, 1.0)?,
                red2: br(RED2, 1.0)?,
                line_g: br(GREEN, 0.16)?,
                scan: br(0x000000, 0.22)?,
                sel: br(GREEN, 0.20)?,
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

    /// Ritar allt. Returnerar flikarnas x-intervall för hit-test.
    pub fn draw(
        &mut self,
        hwnd: HWND,
        tabs: &[Tab],
        active: usize,
        status: &str,
    ) -> Vec<(f32, f32, usize)> {
        if self.res.is_none() {
            match self.create_res(hwnd) {
                Ok(r) => self.res = Some(r),
                Err(_) => return Vec::new(),
            }
        }
        let l = self.layout(hwnd);
        let res = self.res.as_ref().unwrap();
        let (rt, b) = (&res.rt, &res.b);
        let mut hits = Vec::new();

        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&col(BG, 1.0)));

            // ---- flikrad (enda CRT-ytan)
            rt.FillRectangle(&rect(0.0, 0.0, l.w, l.tab_h), &b.header);
            let mut y = 0.0;
            while y < l.tab_h {
                rt.FillRectangle(&rect(0.0, y + 2.0, l.w, y + 3.0), &b.scan);
                y += 3.0;
            }
            rt.FillRectangle(&rect(0.0, l.tab_h - 1.0, l.w, l.tab_h), &b.line_g);
            rt.FillRectangle(&rect(0.0, l.tab_h - 2.0, l.w * 0.48, l.tab_h), &b.red);

            // logga
            let sub_w = self.cw * 9.0;
            let lx = l.w - l.x0 - self.logo_w - sub_w;
            let ty = (l.tab_h - self.lh) / 2.0;
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

            // flikar
            let mut x = l.x0 - 8.0;
            let limit = lx - 24.0;
            for (i, t) in tabs.iter().enumerate() {
                let title = t.title();
                let tw = (title.chars().count() as f32 + if t.dirty { 2.0 } else { 0.0 }) * self.cw
                    + 28.0;
                if x + tw > limit {
                    self.text(
                        rt,
                        &utf16(&format!("+{}", tabs.len() - i)),
                        x + 6.0,
                        ty,
                        &self.mono,
                        &b.muted,
                    );
                    break;
                }
                if i == active {
                    rt.FillRectangle(&rect(x, 6.0, x + tw, l.tab_h - 2.0), &b.panel);
                    rt.FillRectangle(&rect(x, l.tab_h - 3.0, x + tw, l.tab_h - 1.0), &b.green);
                }
                let fg = if i == active { &b.green } else { &b.muted };
                self.text(rt, &utf16(&title), x + 14.0, ty, &self.mono, fg);
                if t.dirty {
                    let dx = x + 14.0 + (title.chars().count() as f32 + 1.0) * self.cw;
                    self.text(rt, &utf16("\u{25CF}"), dx, ty, &self.mono, &b.red2);
                }
                hits.push((x, x + tw, i));
                x += tw + 4.0;
            }

            // ---- text
            let t = &tabs[active];
            let d = &t.doc;
            if t.loading {
                self.text(
                    rt,
                    &utf16(&format!("LADDAR {} ...", t.title())),
                    l.x0,
                    l.y0,
                    &self.mono,
                    &b.muted,
                );
            }
            let sel = t.sel();
            let mut ls = t.top;
            let mut cursor_row = None;
            let mut buf: Vec<u16> = Vec::with_capacity(l.cols * 2 + 2);
            let max_scan = (t.hscroll + l.cols) * 4 + 4096;
            for row in 0..if t.loading { 0 } else { l.rows } {
                let next = d.next_line_start(ls);
                if t.cursor >= ls && next.is_none_or(|n| t.cursor < n) {
                    cursor_row = Some(row);
                }
                buf.clear();
                let (mut c, mut p) = (0usize, ls);
                let (mut s0, mut s1): (Option<usize>, Option<usize>) = (None, None);
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
                if s0.is_some() && s1.is_none() {
                    // Markeringen fortsätter förbi radslutet (eller utanför skärmen).
                    s1 = Some(if eol { c + 1 } else { c });
                }
                let ry = l.y0 + row as f32 * self.lh;
                if let (Some(c0), Some(c1)) = (s0, s1)
                    && c1 > t.hscroll
                {
                    let x0 = l.x0 + c0.saturating_sub(t.hscroll) as f32 * self.cw;
                    let x1 = l.x0 + (c1 - t.hscroll) as f32 * self.cw;
                    rt.FillRectangle(&rect(x0, ry, x1, ry + self.lh), &b.sel);
                }
                self.text(rt, &buf, l.x0, ry, &self.mono, &b.text);
                match next {
                    Some(n) => ls = n,
                    None => break,
                }
            }

            // ---- markör
            if let Some(row) = cursor_row {
                let vc = d.vcol(t.cursor);
                if vc >= t.hscroll {
                    let cx = l.x0 + (vc - t.hscroll) as f32 * self.cw;
                    let cy = l.y0 + row as f32 * self.lh;
                    rt.FillRectangle(&rect(cx, cy + 2.0, cx + 2.0, cy + self.lh - 2.0), &b.green);
                }
            }

            // ---- statusrad
            let sy = l.h - l.status_h;
            rt.FillRectangle(&rect(0.0, sy, l.w, l.h), &b.panel);
            rt.FillRectangle(&rect(0.0, sy, l.w, sy + 1.0), &b.line_g);
            let ty = sy + (l.status_h - self.lh) / 2.0;
            let state = if t.loading {
                "LADDAR"
            } else if t.dirty {
                "ÄNDRAD"
            } else {
                "SPARAD"
            };
            let info = format!(
                "{}  //  KOL {}  //  {}/{} B  //  {}{}  //  ",
                t.path.as_deref().unwrap_or("NAMNLÖS"),
                d.vcol(t.cursor) + 1,
                t.cursor,
                d.len(),
                sel.map(|(a, z)| format!("SEL {} B  //  ", z - a))
                    .unwrap_or_default(),
                if t.crlf { "CRLF" } else { "LF" },
            );
            self.text(rt, &utf16(&info), l.x0, ty, &self.mono, &b.muted);
            let sx = l.x0 + info.chars().count() as f32 * self.cw;
            self.text(
                rt,
                &utf16(state),
                sx,
                ty,
                &self.mono,
                if t.dirty { &b.red2 } else { &b.green },
            );
            if !status.is_empty() {
                let rx = l.w - l.x0 - status.chars().count() as f32 * self.cw;
                self.text(
                    rt,
                    &utf16(status),
                    rx.max(sx + 10.0 * self.cw),
                    ty,
                    &self.mono,
                    &b.muted,
                );
            }

            if rt.EndDraw(None, None).is_err() {
                self.res = None;
            }
        }
        hits
    }
}
