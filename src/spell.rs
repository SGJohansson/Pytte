//! Windows egen stavningskontroll (ISpellChecker) på en egen tråd.
//!
//! Använder samma ordlistor och språk som resten av Windows. Ett ord räknas som
//! felstavat bara om det är fel i alla språk som används (svenska och engelska
//! i samma text ger inga falsklarm). UI-tråden väntar aldrig: förfrågningar och
//! svar går via kanaler, svaren aviseras med ett fönstermeddelande.

use std::sync::mpsc::{self, Receiver, Sender};

use windows::Win32::Foundation::*;
use windows::Win32::Globalization::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayoutList, HKL};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::*;

use crate::spellcore::{Checked, Span, to_bytes, to_utf16};

/// Högst så många språk används samtidigt.
const LANGS_MAX: usize = 3;
const SUGGEST_MAX: usize = 8;
const ERRORS_MAX: usize = 4096;

pub enum Req {
    Check {
        tab: u64,
        seq: u64,
        span: Span,
        text: Vec<u8>,
    },
    Suggest(Ask),
    Add(String),
    Ignore(String),
}

/// Förslag för ett stavfel; skärmkoordinater för menyn.
pub struct Ask {
    pub tab: u64,
    pub seq: u64,
    pub range: (usize, usize),
    pub word: String,
    pub x: i32,
    pub y: i32,
}

pub enum Res {
    /// Språktaggar som används (tom = ingen stavningskontroll finns).
    Ready(Vec<String>),
    Checked {
        tab: u64,
        c: Checked,
    },
    Suggest(Ask, Vec<String>),
}

pub struct Spell {
    tx: Sender<Req>,
    pub rx: Receiver<Res>,
}

impl Spell {
    pub fn start(hwnd: HWND, msg: u32) -> Spell {
        let (tx, req_rx) = mpsc::channel::<Req>();
        let (res_tx, rx) = mpsc::channel::<Res>();
        let h = hwnd.0 as isize;
        std::thread::spawn(move || {
            let post = |r: Res| {
                if res_tx.send(r).is_ok() {
                    unsafe {
                        let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0));
                    }
                }
            };
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let (cs, tags) = unsafe { init() };
            post(Res::Ready(tags));
            while let Ok(r) = req_rx.recv() {
                if cs.is_empty() {
                    continue;
                }
                match r {
                    Req::Check {
                        tab,
                        seq,
                        span,
                        text,
                    } => {
                        let found = unsafe { check(&cs, &span, &text) };
                        post(Res::Checked {
                            tab,
                            c: Checked { seq, span, found },
                        });
                    }
                    Req::Suggest(a) => {
                        let list = unsafe { suggest(&cs, &a.word) };
                        post(Res::Suggest(a, list));
                    }
                    Req::Add(w) => unsafe {
                        let w = HSTRING::from(w.as_str());
                        let _ = cs[0].Add(&w);
                    },
                    Req::Ignore(w) => unsafe {
                        let w = HSTRING::from(w.as_str());
                        for c in &cs {
                            let _ = c.Ignore(&w);
                        }
                    },
                }
            }
            drop(cs);
            unsafe { CoUninitialize() };
        });
        Spell { tx, rx }
    }

    pub fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }
}

fn wstr(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Kandidatspråk: användarens språkområde, installerade tangentbordsspråk och
/// visningsspråk, i den ordningen.
unsafe fn langs() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    unsafe {
        let mut buf = [0u16; 85];
        if GetUserDefaultLocaleName(&mut buf) > 0 {
            v.push(wstr(&buf));
        }
        let n = GetKeyboardLayoutList(None);
        if n > 0 {
            let mut hkl = vec![HKL(std::ptr::null_mut()); n as usize];
            let n = GetKeyboardLayoutList(Some(&mut hkl)).max(0) as usize;
            for h in &hkl[..n] {
                let lang = (h.0 as usize & 0xffff) as u32;
                let mut buf = [0u16; 85];
                if LCIDToLocaleName(lang, Some(&mut buf), 0) > 0 {
                    v.push(wstr(&buf));
                }
            }
        }
        let (mut num, mut len) = (0u32, 0u32);
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut num, None, &mut len).is_ok()
            && len > 0
        {
            let mut buf = vec![0u16; len as usize];
            if GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut num,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
            )
            .is_ok()
            {
                v.extend(
                    buf.split(|&c| c == 0)
                        .filter(|s| !s.is_empty())
                        .map(String::from_utf16_lossy),
                );
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for s in v {
        if !s.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&s)) {
            out.push(s);
        }
    }
    out
}

unsafe fn init() -> (Vec<ISpellChecker>, Vec<String>) {
    let mut cs = Vec::new();
    let mut tags = Vec::new();
    unsafe {
        let f: ISpellCheckerFactory =
            match CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER) {
                Ok(f) => f,
                Err(_) => return (cs, tags),
            };
        for tag in langs() {
            let w = HSTRING::from(tag.as_str());
            if !f.IsSupported(&w).is_ok_and(|b| b.as_bool()) {
                continue;
            }
            if let Ok(c) = f.CreateSpellChecker(&w) {
                cs.push(c);
                tags.push(tag);
                if cs.len() == LANGS_MAX {
                    break;
                }
            }
        }
    }
    (cs, tags)
}

/// Fel (UTF-16-start, längd) som ska rättas. Upprepade ord ("och och") räknas inte.
unsafe fn errors(c: &ISpellChecker, text: PCWSTR) -> Vec<(u32, u32)> {
    let mut v = Vec::new();
    unsafe {
        let Ok(en) = c.Check(text) else { return v };
        loop {
            let mut e: Option<ISpellingError> = None;
            if en.Next(&mut e) != S_OK {
                break;
            }
            let Some(e) = e else { break };
            let act = e.CorrectiveAction().unwrap_or(CORRECTIVE_ACTION_NONE);
            if (act == CORRECTIVE_ACTION_GET_SUGGESTIONS || act == CORRECTIVE_ACTION_REPLACE)
                && let (Ok(s), Ok(n)) = (e.StartIndex(), e.Length())
            {
                v.push((s, n));
                if v.len() >= ERRORS_MAX {
                    break;
                }
            }
        }
    }
    v
}

unsafe fn check(cs: &[ISpellChecker], span: &Span, text: &[u8]) -> Vec<(usize, usize)> {
    let (mut w, off) = to_utf16(text);
    let n = w.len();
    w.push(0);
    let mut errs = unsafe { errors(&cs[0], PCWSTR(w.as_ptr())) };
    if cs.len() > 1 {
        errs.retain(|&(s, l)| {
            let (s, e) = (s as usize, s as usize + l as usize);
            if e > n {
                return false;
            }
            let mut word: Vec<u16> = w[s..e].to_vec();
            word.push(0);
            cs[1..]
                .iter()
                .all(|c| unsafe { !errors(c, PCWSTR(word.as_ptr())).is_empty() })
        });
    }
    to_bytes(span, &off, &errs)
}

unsafe fn suggest(cs: &[ISpellChecker], word: &str) -> Vec<String> {
    let w = HSTRING::from(word);
    let mut out: Vec<String> = Vec::new();
    for c in cs {
        unsafe {
            let Ok(en) = c.Suggest(&w) else { continue };
            loop {
                let mut item = [PWSTR::null()];
                let mut got = 0u32;
                if en.Next(&mut item, Some(&mut got)) != S_OK || got == 0 {
                    break;
                }
                let s = item[0].to_string().unwrap_or_default();
                CoTaskMemFree(Some(item[0].0 as *const _));
                if !s.is_empty() && s != word && !out.contains(&s) {
                    out.push(s);
                }
                if out.len() >= SUGGEST_MAX {
                    return out;
                }
            }
        }
    }
    out
}
