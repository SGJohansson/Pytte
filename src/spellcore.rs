//! Stavningskontrollens plattformsoberoende del: stavfel som följer med
//! redigeringar, en kort ändringslogg för svar som kommer asynkront, vilka
//! byteintervall som ska kontrolleras efter en redigering och UTF-8 → UTF-16.
//!
//! Bara text som redigeras kontrolleras. Inget skannas retroaktivt, så
//! kostnaden beror på hur mycket som skrivs, aldrig på filens storlek.

use std::collections::VecDeque;

/// Mest som kontrolleras per redigering. Större inklistringar kontrolleras i
/// början och vid slutet (där markören hamnar).
pub const CHECK_MAX: usize = 64 << 10;
/// Hur långt ett ord får sträcka sig utanför den redigerade texten.
pub const WORD_REACH: usize = 128;
const LOG_MAX: usize = 4096;
const MARKS_MAX: usize = 20_000;

#[derive(Clone, Copy)]
struct Edit {
    seq: u64,
    a: usize,
    b: usize,
    ins: usize,
}

/// Flyttar ett stavfel förbi redigeringen. Ett stavfel som redigeringen rör
/// (även direkt intill, då ordet växer) försvinner.
fn shift((s, e): (usize, usize), x: &Edit) -> Option<(usize, usize)> {
    if e < x.a {
        Some((s, e))
    } else if s > x.b {
        Some((s - (x.b - x.a) + x.ins, e - (x.b - x.a) + x.ins))
    } else {
        None
    }
}

/// Flyttar ett intervall förbi redigeringen; det överlever alltid och växer
/// till att täcka infogad text som hamnar inuti det.
fn cover((s, e): (usize, usize), x: &Edit) -> (usize, usize) {
    let d = |p: usize| p - (x.b - x.a) + x.ins;
    let s2 = if s <= x.a {
        s
    } else if s >= x.b {
        d(s)
    } else {
        x.a
    };
    let e2 = if e < x.a {
        e
    } else if e >= x.b {
        d(e)
    } else {
        x.a + x.ins
    };
    (s2, e2.max(s2))
}

/// Ett intervall att kontrollera. `open_*`: ordet vid kanten kan fortsätta
/// utanför (inget blanksteg hittades inom WORD_REACH) och ska inte bedömas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub lo: usize,
    pub hi: usize,
    pub open_lo: bool,
    pub open_hi: bool,
}

/// Svar från stavningstråden: `span` kontrollerades vid ändring `seq`.
pub struct Checked {
    pub seq: u64,
    pub span: Span,
    pub found: Vec<(usize, usize)>,
}

#[derive(Default)]
pub struct Marks {
    /// Stavfel [s, e), sorterade och disjunkta.
    pub list: Vec<(usize, usize)>,
    /// Redigerade intervall som ännu inte skickats för kontroll.
    dirty: Vec<(usize, usize)>,
    seq: u64,
    log: VecDeque<Edit>,
}

impl Marks {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// [a, b) har ersatts med `ins` byte.
    pub fn edit(&mut self, a: usize, b: usize, ins: usize) {
        self.seq += 1;
        let x = Edit {
            seq: self.seq,
            a,
            b,
            ins,
        };
        if self.log.len() == LOG_MAX {
            self.log.pop_front();
        }
        self.log.push_back(x);
        let i0 = self.list.partition_point(|m| m.1 < a);
        let mut j = i0;
        while j < self.list.len() && self.list[j].0 <= b {
            j += 1;
        }
        self.list.drain(i0..j);
        for m in &mut self.list[i0..] {
            *m = shift(*m, &x).unwrap_or(*m);
        }
        for d in &mut self.dirty {
            *d = cover(*d, &x);
        }
        self.dirty.push((a, a + ins));
    }

    /// Tar de redigerade intervallen, sammanslagna.
    pub fn take_dirty(&mut self) -> Vec<(usize, usize)> {
        let mut v = std::mem::take(&mut self.dirty);
        v.sort_unstable();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(v.len());
        for (s, e) in v {
            match out.last_mut() {
                Some(l) if s <= l.1 => l.1 = l.1.max(e),
                _ => out.push((s, e)),
            }
        }
        out
    }

    /// Flyttar ett intervall från läget vid `seq` till nu. None om det
    /// redigerats sedan dess (eller loggen inte räcker tillbaka).
    pub fn map(&self, seq: u64, r: (usize, usize)) -> Option<(usize, usize)> {
        if self.log.front().is_some_and(|f| f.seq > seq + 1) {
            return None;
        }
        let mut r = r;
        for x in self.log.iter().filter(|x| x.seq > seq) {
            r = shift(r, x)?;
        }
        Some(r)
    }

    /// För in ett kontrollsvar. Returnerar true om listan ändrades.
    pub fn apply(&mut self, c: Checked) -> bool {
        if c.seq > self.seq || self.log.front().is_some_and(|f| f.seq > c.seq + 1) {
            return false;
        }
        let mut sp = (c.span.lo, c.span.hi);
        let mut found = c.found;
        for x in self.log.iter().filter(|x| x.seq > c.seq) {
            sp = cover(sp, x);
            found.retain_mut(|m| match shift(*m, x) {
                Some(n) => {
                    *m = n;
                    true
                }
                None => false,
            });
        }
        let before = self.list.len();
        self.list.retain(|m| m.1 <= sp.0 || m.0 >= sp.1);
        let changed = before != self.list.len() || !found.is_empty();
        if !found.is_empty() {
            self.list.extend(found);
            self.list.sort_unstable();
            self.list.dedup();
            // Överlapp kan bara uppstå om ett svar hunnit bli inaktuellt.
            let mut k = 0;
            for i in 0..self.list.len() {
                if k == 0 || self.list[i].0 >= self.list[k - 1].1 {
                    self.list[k] = self.list[i];
                    k += 1;
                }
            }
            self.list.truncate(k.min(MARKS_MAX));
        }
        changed
    }

    /// Stavfelet som innehåller `pos` (eller slutar precis där).
    pub fn at(&self, pos: usize) -> Option<(usize, usize)> {
        let i = self.list.partition_point(|m| m.1 < pos);
        self.list.get(i).copied().filter(|m| m.0 <= pos)
    }

    /// Släpper redigeringar som inte skickats (stavningskontrollen avstängd).
    pub fn discard_dirty(&mut self) {
        self.dirty.clear();
    }
}

fn blank(b: Option<u8>) -> bool {
    b.is_none_or(|b| b.is_ascii_whitespace() || b == 0)
}

fn cont(b: Option<u8>) -> bool {
    b.is_some_and(|b| b & 0xc0 == 0x80)
}

/// Växer [a, z) ut till omgivande blanksteg (högst WORD_REACH åt varje håll).
fn grow(len: usize, a: usize, z: usize, byte: &impl Fn(usize) -> Option<u8>) -> Span {
    let (a, z) = (a.min(len), z.min(len));
    let mut lo = a;
    while lo > 0 && a - lo < WORD_REACH && !blank(byte(lo - 1)) {
        lo -= 1;
    }
    let open_lo = lo > 0 && !blank(byte(lo - 1));
    while lo < a && cont(byte(lo)) {
        lo += 1;
    }
    let mut hi = z;
    while hi < len && hi - z < WORD_REACH && !blank(byte(hi)) {
        hi += 1;
    }
    let open_hi = hi < len && !blank(byte(hi));
    while hi > lo && cont(byte(hi)) {
        hi -= 1;
    }
    Span {
        lo,
        hi,
        open_lo,
        open_hi,
    }
}

/// Intervallen att kontrollera efter att [a, z) redigerats.
pub fn spans(len: usize, a: usize, z: usize, byte: impl Fn(usize) -> Option<u8>) -> Vec<Span> {
    if z - a <= CHECK_MAX {
        return vec![grow(len, a, z, &byte)];
    }
    let mut first = grow(len, a, a + CHECK_MAX, &byte);
    // Kapat mitt i inklistrad text: sista ordet där bedöms inte.
    first.open_hi |= first.hi < z;
    vec![first, grow(len, z, z, &byte)]
}

/// UTF-16 för stavningskontrollen och byteoffset per kodenhet (plus slutet).
/// Ogiltiga byte och NUL blir blanksteg så att offseten håller.
pub fn to_utf16(bytes: &[u8]) -> (Vec<u16>, Vec<usize>) {
    let mut w = Vec::with_capacity(bytes.len() + 1);
    let mut off = Vec::with_capacity(bytes.len() + 1);
    let mut p = 0;
    for ch in bytes.utf8_chunks() {
        for c in ch.valid().chars() {
            let c = if c == '\0' { ' ' } else { c };
            let mut u = [0u16; 2];
            for &x in c.encode_utf16(&mut u).iter() {
                w.push(x);
                off.push(p);
            }
            p += c.len_utf8();
        }
        for _ in ch.invalid() {
            w.push(b' ' as u16);
            off.push(p);
            p += 1;
        }
    }
    off.push(p);
    (w, off)
}

/// Stavfel (UTF-16-start, längd) inom `span` → byteintervall. Ord vid en
/// öppen kant bedöms inte.
pub fn to_bytes(span: &Span, off: &[usize], errs: &[(u32, u32)]) -> Vec<(usize, usize)> {
    let n = off.len() - 1;
    let mut v: Vec<(usize, usize)> = errs
        .iter()
        .filter_map(|&(s, l)| {
            let (s, e) = (s as usize, s as usize + l as usize);
            if l == 0 || e > n {
                return None;
            }
            let (bs, be) = (span.lo + off[s], span.lo + off[e]);
            if (span.open_lo && bs == span.lo) || (span.open_hi && be == span.hi) {
                return None;
            }
            Some((bs, be))
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(lo: usize, hi: usize) -> Span {
        Span {
            lo,
            hi,
            open_lo: false,
            open_hi: false,
        }
    }

    #[test]
    fn marks_follow_edits() {
        let mut m = Marks::default();
        m.list = vec![(0, 4), (10, 14), (20, 24)];
        // Infogning före ett stavfel flyttar det, ingen överlappning.
        m.edit(6, 6, 2);
        assert_eq!(m.list, vec![(0, 4), (12, 16), (22, 26)]);
        // Skrivning direkt efter ett ord: ordet ändras, stavfelet släpps.
        m.edit(4, 4, 1);
        assert_eq!(m.list, vec![(13, 17), (23, 27)]);
        // Radering över ett stavfel.
        m.edit(12, 18, 0);
        assert_eq!(m.list, vec![(17, 21)]);
        assert_eq!(m.at(17), Some((17, 21)));
        assert_eq!(m.at(21), Some((17, 21)));
        assert_eq!(m.at(22), None);
    }

    #[test]
    fn dirty_merges_and_follows() {
        let mut m = Marks::default();
        m.edit(5, 5, 1); // "x" vid 5
        m.edit(6, 6, 1); // "y" vid 6
        m.edit(0, 0, 3); // tre byte först: allt flyttas
        assert_eq!(m.take_dirty(), vec![(0, 3), (8, 10)]);
        assert!(m.take_dirty().is_empty());
    }

    #[test]
    fn late_answer_is_mapped() {
        let mut m = Marks::default();
        m.edit(0, 0, 10); // " helo wrld"
        let seq = m.seq();
        m.edit(0, 0, 2); // två byte till först innan svaret kommer
        let ok = m.apply(Checked {
            seq,
            span: sp(0, 10),
            found: vec![(1, 5), (6, 10)],
        });
        assert!(ok);
        assert_eq!(m.list, vec![(3, 7), (8, 12)]);
        // Ett nytt svar för samma intervall ersätter de gamla.
        let seq = m.seq();
        m.apply(Checked {
            seq,
            span: sp(2, 12),
            found: vec![(8, 12)],
        });
        assert_eq!(m.list, vec![(8, 12)]);
        // Svar för ett ord som hunnit redigeras släpps.
        m.edit(12, 12, 1);
        m.apply(Checked {
            seq,
            span: sp(2, 12),
            found: vec![(3, 7), (8, 12)],
        });
        assert_eq!(m.list, vec![(3, 7)]);
        assert_eq!(m.map(seq, (3, 7)), Some((3, 7)));
        assert_eq!(m.map(seq, (8, 12)), None);
    }

    #[test]
    fn spans_stop_at_blanks_and_char_bounds() {
        let t = "hej på dig".as_bytes();
        let b = |p: usize| t.get(p).copied();
        // Skrivet "å" (byte 5..7) i "på".
        assert_eq!(spans(t.len(), 5, 7, b), vec![sp(4, 7)]);
        let long = vec![b'a'; 300];
        let b = |p: usize| long.get(p).copied();
        let s = spans(300, 150, 151, b)[0];
        assert!(s.open_lo && s.open_hi);
        assert_eq!((s.lo, s.hi), (150 - WORD_REACH, 151 + WORD_REACH));
        // Kanten hamnar aldrig mitt i ett tecken.
        let ao = "åäöåäöåäöå".repeat(20);
        let ab = ao.as_bytes();
        let b = |p: usize| ab.get(p).copied();
        let s = spans(ab.len(), 200, 200, b)[0];
        assert!(std::str::from_utf8(&ab[s.lo..s.hi]).is_ok());
    }

    #[test]
    fn utf16_offsets() {
        let (w, off) = to_utf16(b"a\xc3\xa5\xf0\x9f\x98\x80\x00b\xff");
        assert_eq!(w.len(), 7);
        assert_eq!(off, vec![0, 1, 3, 3, 7, 8, 9, 10]);
        assert_eq!(w[4], b' ' as u16);
        assert_eq!(w[6], b' ' as u16);
        let (_, off) = to_utf16("xx åäk yy".as_bytes());
        let s = Span {
            lo: 100,
            hi: 111,
            open_lo: true,
            open_hi: false,
        };
        // "xx" vid öppen kant bedöms inte, "åäk" blir byte 103..108.
        assert_eq!(to_bytes(&s, &off, &[(0, 2), (3, 3)]), vec![(103, 108)]);
    }
}
