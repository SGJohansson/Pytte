//! Ctrl+F: matchning + bakgrundssökning över en ögonblicksbild av dokumentet.
//!
//! UI-tråden tar en Snap (O(pieces), inga bytes kopieras) och startar en
//! sökartråd. Träffar strömmas tillbaka i batchar via kanal; UI väcks med en
//! notifiering högst ~60 ggr/s. Avbrott via AtomicBool, kontrolleras per 4 MiB.
//! Synliga träffar ritas från en egen viewport-sökning, så markeringarna är
//! korrekta även medan bakgrundsresultatet är inaktuellt efter en redigering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::doc::Snap;

/// Lagrade träffpositioner (8 B st). Därutöver räknas bara.
pub const MAX_HITS: usize = 4_000_000;
const BLK: usize = 4 << 20;
const SEND_EVERY: Duration = Duration::from_millis(16);

/// Skiftlägesvikning: ASCII samt Latin-1-tillägget i UTF-8 (C3 80–9E ↔ C3 A0–BE,
/// utom ×/÷). Täcker åäöÅÄÖ, éÉ, üÜ osv. Kontext = föregående råbyte.
#[inline]
fn fold(prev: u8, b: u8) -> u8 {
    match b {
        b'A'..=b'Z' => b + 32,
        0x80..=0x9e if prev == 0xc3 && b != 0x97 => b + 0x20,
        _ => b,
    }
}

pub struct Matcher {
    raw: Vec<u8>,
    pat: Vec<u8>,
    pub fold: bool,
    finder: memchr::memmem::Finder<'static>,
}

impl Matcher {
    /// None för tomt mönster.
    pub fn new(needle: &[u8], fold_case: bool) -> Option<Matcher> {
        if needle.is_empty() {
            return None;
        }
        let pat = if fold_case {
            let mut prev = 0;
            needle
                .iter()
                .map(|&b| {
                    let f = fold(prev, b);
                    prev = b;
                    f
                })
                .collect()
        } else {
            needle.to_vec()
        };
        Some(Matcher {
            raw: needle.to_vec(),
            finder: memchr::memmem::Finder::new(needle).into_owned(),
            pat,
            fold: fold_case,
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[inline]
    fn eq_at(&self, hay: &[u8], i: usize) -> bool {
        let mut prev = 0u8;
        for (j, &p) in self.pat.iter().enumerate() {
            let b = hay[i + j];
            if fold(prev, b) != p {
                return false;
            }
            prev = b;
        }
        true
    }

    /// Icke-överlappande träffar helt inom `hay`, i stigande ordning.
    /// `f` returnerar false för att avbryta.
    pub fn each(&self, hay: &[u8], mut f: impl FnMut(usize) -> bool) {
        let m = self.len();
        if hay.len() < m {
            return;
        }
        if !self.fold {
            for i in self.finder.find_iter(hay) {
                if !f(i) {
                    return;
                }
            }
            return;
        }
        let p0 = self.pat[0];
        let last = hay.len() - m;
        let mut from = 0;
        let mut verify = |i: usize, from: &mut usize| -> Option<bool> {
            if i > last {
                return Some(false);
            }
            if self.eq_at(hay, i) {
                *from = i + m;
                return Some(f(i));
            }
            *from = i + 1;
            None
        };
        // Kandidater på första byten (båda skiftlägen för ASCII-bokstäver).
        if p0.is_ascii_lowercase() {
            while let Some(k) = memchr::memchr2(p0, p0 - 32, &hay[from..]) {
                if verify(from + k, &mut from) == Some(false) {
                    return;
                }
            }
        } else {
            while let Some(k) = memchr::memchr(p0, &hay[from..]) {
                if verify(from + k, &mut from) == Some(false) {
                    return;
                }
            }
        }
    }

    /// Första träff i hay.
    pub fn first(&self, hay: &[u8]) -> Option<usize> {
        let mut r = None;
        self.each(hay, |i| {
            r = Some(i);
            false
        });
        r
    }

    /// Sista träff som börjar före `before`.
    pub fn last_before(&self, hay: &[u8], before: usize) -> Option<usize> {
        let mut r = None;
        self.each(hay, |i| {
            if i < before {
                r = Some(i);
                true
            } else {
                false
            }
        });
        r
    }
}

pub struct Batch {
    pub generation: u64,
    pub hits: Vec<usize>,
    /// Totalt antal träffar hittills (även de som inte lagras).
    pub total: u64,
    pub scanned: usize,
    pub done: bool,
}

pub struct Handle {
    cancel: Arc<AtomicBool>,
}

impl Handle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Startar en sökning. `notify` anropas (från sökartråden) när en batch skickats.
pub fn spawn(
    generation: u64,
    snap: Snap,
    m: Arc<Matcher>,
    tx: Sender<Batch>,
    notify: impl Fn() + Send + 'static,
) -> Handle {
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let _ = std::thread::Builder::new()
        .name("search".into())
        .spawn(move || run(generation, snap, &m, &c2, &tx, &notify));
    Handle { cancel }
}

/// Synkron variant för tester: alla träffar.
#[cfg(test)]
pub fn scan_all(snap: &Snap, m: &Matcher) -> Vec<usize> {
    let mut v = Vec::new();
    scan(snap, m, &AtomicBool::new(false), &mut |e| {
        if let Ev::Hit(h) = e {
            v.push(h)
        }
    });
    v
}

enum Ev {
    Hit(usize),
    /// Antal skannade bytes hittills.
    Progress(usize),
}

fn run(
    generation: u64,
    snap: Snap,
    m: &Matcher,
    cancel: &AtomicBool,
    tx: &Sender<Batch>,
    notify: &dyn Fn(),
) {
    let mut hits = Vec::new();
    let mut total = 0u64;
    let mut stored = 0usize;
    let mut last = Instant::now();
    let ok = scan(&snap, m, cancel, &mut |e| match e {
        Ev::Hit(h) => {
            total += 1;
            if stored < MAX_HITS {
                hits.push(h);
                stored += 1;
            }
        }
        Ev::Progress(scanned) => {
            if last.elapsed() >= SEND_EVERY {
                last = Instant::now();
                let b = Batch {
                    generation,
                    hits: std::mem::take(&mut hits),
                    total,
                    scanned,
                    done: false,
                };
                if tx.send(b).is_ok() {
                    notify();
                }
            }
        }
    });
    if ok {
        let _ = tx.send(Batch {
            generation,
            hits,
            total,
            scanned: snap.len,
            done: true,
        });
        notify();
    }
}

/// Sekventiell skanning i dokumentordning. Träffar som korsar piece-/blockgränser
/// hittas i en liten fogbuffert (svans + huvud). Returnerar false vid avbrott.
fn scan(snap: &Snap, m: &Matcher, cancel: &AtomicBool, on: &mut dyn FnMut(Ev)) -> bool {
    let n = m.len();
    let keep = n - 1;
    let mut tail: Vec<u8> = Vec::with_capacity(keep);
    let mut tail_off = 0usize;
    let mut off = 0usize;
    let mut last_end = 0usize;
    let mut joint = Vec::with_capacity(2 * keep);
    for part in &snap.parts {
        for blk in part.bytes().chunks(BLK) {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if keep > 0 && !tail.is_empty() {
                joint.clear();
                joint.extend_from_slice(&tail);
                joint.extend_from_slice(&blk[..blk.len().min(keep)]);
                let tl = tail.len();
                m.each(&joint, |i| {
                    if i < tl && i + n > tl && tail_off + i >= last_end {
                        on(Ev::Hit(tail_off + i));
                        last_end = tail_off + i + n;
                    }
                    i < tl
                });
            }
            m.each(blk, |i| {
                if off + i >= last_end {
                    on(Ev::Hit(off + i));
                    last_end = off + i + n;
                }
                true
            });
            if keep > 0 {
                if blk.len() >= keep {
                    tail.clear();
                    tail.extend_from_slice(&blk[blk.len() - keep..]);
                    tail_off = off + blk.len() - keep;
                } else {
                    tail.extend_from_slice(blk);
                    let drop = tail.len().saturating_sub(keep);
                    tail.drain(..drop);
                    tail_off = off + blk.len() - tail.len();
                }
            }
            off += blk.len();
            on(Ev::Progress(off));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use crate::orig;

    fn naive(hay: &[u8], pat: &[u8], fold_case: bool) -> Vec<usize> {
        let m = Matcher::new(pat, fold_case).unwrap();
        let mut v = Vec::new();
        let mut i = 0;
        while i + pat.len() <= hay.len() {
            if m.eq_at(hay, i) {
                v.push(i);
                i += pat.len();
            } else {
                i += 1;
            }
        }
        v
    }

    #[test]
    fn fold_swedish() {
        let m = Matcher::new("åÄö".as_bytes(), true).unwrap();
        let hay = "x ÅäÖ y åäö ÅÄÖ".as_bytes();
        let mut v = Vec::new();
        m.each(hay, |i| {
            v.push(i);
            true
        });
        assert_eq!(v.len(), 3);
        let m = Matcher::new(b"Hej", true).unwrap();
        assert_eq!(m.first(b"..hEJ.."), Some(2));
        let m = Matcher::new(b"Hej", false).unwrap();
        assert_eq!(m.first(b"..hEJ.."), None);
        // × och ÷ viks inte ihop
        let m = Matcher::new("×".as_bytes(), true).unwrap();
        assert_eq!(m.first("÷".as_bytes()), None);
    }

    #[test]
    fn across_pieces() {
        let mut d = Doc::new(orig::owned(b"abcdefghij".repeat(1000)));
        // Splittra i massor av små pieces mitt i mönster.
        for k in (1..900).rev().step_by(3) {
            d.insert(k * 10 + 4, b"");
            d.insert(k * 10 + 3, b"D");
            d.delete(k * 10 + 3, 1);
        }
        for (pat, fold_case) in [(&b"cdefg"[..], false), (b"JABC", true), (b"j", false)] {
            let m = Matcher::new(pat, fold_case).unwrap();
            let got = scan_all(&d.snapshot(), &m);
            let want = naive(&d.materialize(), pat, fold_case);
            assert_eq!(got, want, "{:?}", std::str::from_utf8(pat));
        }
    }

    #[test]
    fn worker_streams_and_cancels() {
        let d = Doc::new(orig::owned(b"needle ".repeat(3_000_000)));
        let m = Arc::new(Matcher::new(b"needle", false).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let h = spawn(7, d.snapshot(), m.clone(), tx, || {});
        let mut n = 0u64;
        loop {
            let b = rx.recv().unwrap();
            assert_eq!(b.generation, 7);
            n += b.hits.len() as u64;
            if b.done {
                assert_eq!(b.total, 3_000_000);
                break;
            }
        }
        assert_eq!(n, 3_000_000);
        drop(h);
        let (tx, rx) = std::sync::mpsc::channel();
        let h = spawn(8, d.snapshot(), m, tx, || {});
        h.cancel();
        // Avbruten sökning skickar aldrig done.
        while let Ok(b) = rx.recv() {
            assert!(!b.done || b.total == 3_000_000);
        }
    }
}
