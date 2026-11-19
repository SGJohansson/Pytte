//! Piece table över mmap:ad källfil + append-only add-buffert.
//! Allt är byteoffset, så öppning är O(1) oavsett filstorlek.
//! Varje piece cachar sitt antal '\n' (känt för add-data direkt, för källdata
//! så fort bakgrundsindexet nått dit) → radnummer är O(pieces) utan filskanning.
//! Texten tolkas som UTF-8; ogiltiga bytes lämnas orörda och stegas en i taget.

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;
use std::sync::Arc;

use crate::orig::{self, CH, Orig, count_nl};

pub const TAB: usize = 4;
const UNK: u64 = u64::MAX;
/// Under denna storlek räknas/sökes direkt istället för via index.
const DIRECT: usize = 2 * CH;
/// Budget för rå radslutsskanning när indexet ännu inte nått fram (render).
pub const SCAN_BUDGET: usize = 4 << 20;
/// Dubbelklick markerar högst så här långt åt vardera håll.
const WORD_SCAN: usize = 4096;
/// Parentesmatchning söker högst så här långt åt vardera håll.
pub const BRACKET_SCAN: usize = 64 << 10;

#[derive(Clone, Copy)]
struct Piece {
    add: bool,
    start: usize,
    len: usize,
    /// Antal '\n' i piecen, eller UNK.
    nl: u64,
}

/// Bortklippt intervall som piece-referenser (O(pieces), inte O(bytes)).
/// Giltigt så länge Doc lever: orig är oföränderlig och add är append-only.
pub struct Removed {
    pieces: Vec<Piece>,
    pub len: usize,
}

// ---------------------------------------------------------------- add-buffert

const ACH_SHIFT: u32 = 12;
const ACH: usize = 1 << ACH_SHIFT;
const SEG_MAX: usize = 1 << 20;

/// Nollställt minne som skrivs en gång per byte och aldrig skrivs om.
/// Delas med söktrådar via Arc: de läser bara redan skrivna intervall.
pub struct Seg {
    ptr: NonNull<u8>,
    cap: usize,
}

unsafe impl Send for Seg {}
unsafe impl Sync for Seg {}

impl Seg {
    fn new(cap: usize) -> Seg {
        let layout = Layout::array::<u8>(cap).unwrap();
        let p = unsafe { alloc_zeroed(layout) };
        match NonNull::new(p) {
            Some(ptr) => Seg { ptr, cap },
            None => handle_alloc_error(layout),
        }
    }

    #[inline]
    pub fn get(&self, a: usize, n: usize) -> &[u8] {
        assert!(a + n <= self.cap);
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(a), n) }
    }

    /// Endast Doc (UI-tråden) skriver, och bara i ännu oskrivna intervall.
    unsafe fn write(&self, a: usize, src: &[u8]) {
        assert!(a + src.len() <= self.cap);
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.as_ptr().add(a), src.len()) };
    }
}

impl Drop for Seg {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), Layout::array::<u8>(self.cap).unwrap()) };
    }
}

/// Globalt offsetrum uppdelat i ACH-justerade segment; oanvända segmentsvansar
/// är nollor (inga '\n'). En piece ligger alltid inom ett segment.
struct AddBuf {
    segs: Vec<(usize, Arc<Seg>)>,
    end: usize,
    /// nl[k] = antal '\n' under k*ACH. Invariant: nl.len() == end/ACH + 1.
    nl: Vec<u64>,
    nl_total: u64,
}

impl AddBuf {
    fn new() -> AddBuf {
        AddBuf {
            segs: Vec::new(),
            end: 0,
            nl: vec![0],
            nl_total: 0,
        }
    }

    #[inline]
    fn seg(&self, x: usize) -> &(usize, Arc<Seg>) {
        let i = self.segs.partition_point(|s| s.0 <= x);
        &self.segs[i - 1]
    }

    /// Bytes [a, a+n) – måste ligga inom ett segment.
    #[inline]
    fn get(&self, a: usize, n: usize) -> &[u8] {
        if n == 0 {
            return &[];
        }
        let (base, s) = self.seg(a);
        s.get(a - base, n)
    }

    /// Returnerar (start, nytt segment påbörjat).
    fn append(&mut self, b: &[u8]) -> (usize, bool) {
        let n = b.len();
        let fits = self
            .segs
            .last()
            .is_some_and(|(base, s)| self.end + n <= base + s.cap);
        if !fits {
            let (base, prev_cap) = self
                .segs
                .last()
                .map_or((0, 0), |(base, s)| (base + s.cap, s.cap));
            let cap = (prev_cap * 2)
                .clamp(ACH, SEG_MAX)
                .max(n.next_multiple_of(ACH));
            while self.nl.len() - 1 < base >> ACH_SHIFT {
                self.nl.push(self.nl_total);
            }
            self.end = base;
            self.segs.push((base, Arc::new(Seg::new(cap))));
        }
        let start = self.end;
        let (base, s) = self.segs.last().unwrap();
        let fresh = start == *base;
        unsafe { s.write(start - base, b) };
        let (mut off, mut pos) = (0, start);
        while off < n {
            let next_b = ((pos >> ACH_SHIFT) + 1) << ACH_SHIFT;
            let take = (next_b - pos).min(n - off);
            self.nl_total += count_nl(&b[off..off + take]);
            off += take;
            pos += take;
            if pos == next_b {
                self.nl.push(self.nl_total);
            }
        }
        self.end += n;
        (start, fresh)
    }

    /// Antal '\n' under x (x ≤ end).
    #[inline]
    fn nl_before(&self, x: usize) -> u64 {
        let k = x >> ACH_SHIFT;
        let a = k << ACH_SHIFT;
        self.nl[k]
            + if x > a {
                count_nl(self.get(a, x - a))
            } else {
                0
            }
    }

    /// Position för det t:e (1-baserade) '\n'.
    fn find_nl(&self, t: u64) -> Option<usize> {
        if t == 0 || t > self.nl_total {
            return None;
        }
        let k = self.nl.partition_point(|&c| c < t) - 1;
        let a = k << ACH_SHIFT;
        let b = (a + ACH).min(self.end);
        let skip = (t - self.nl[k] - 1) as usize;
        memchr::memchr_iter(b'\n', self.get(a, b - a))
            .nth(skip)
            .map(|i| a + i)
    }
}

// ---------------------------------------------------------------- dokument

pub struct Doc {
    orig: Orig,
    add: AddBuf,
    pieces: Vec<Piece>,
    len: usize,
    /// Det kan finnas pieces med okänt radantal.
    unresolved: bool,
    version: u64,
}

/// Läsbar del av en ögonblicksbild; håller källan vid liv på andra trådar.
pub enum PartSrc {
    Orig(Orig),
    Add(Arc<Seg>),
}

pub struct Part {
    src: PartSrc,
    a: usize,
    n: usize,
}

impl Part {
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        match &self.src {
            PartSrc::Orig(o) => &o.bytes()[self.a..self.a + self.n],
            PartSrc::Add(s) => s.get(self.a, self.n),
        }
    }
}

/// Oföränderlig vy av dokumentet för bakgrundstrådar. O(pieces) att ta.
pub struct Snap {
    pub parts: Vec<Part>,
    pub len: usize,
}

/// Radslut efter en position när indexet ännu inte kan svara.
pub enum Next {
    At(usize),
    End,
    Unknown,
}

/// Längd på UTF-8-sekvens givet första byte; 0 = ogiltig startbyte.
#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

#[inline]
fn class(b: u8) -> u8 {
    match b {
        b' ' | b'\t' => 0,
        b'\r' | b'\n' => 1,
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | 0x80..=0xff => 2,
        _ => 3,
    }
}

impl Doc {
    pub fn new(orig: Orig) -> Doc {
        let n = orig.bytes().len();
        let mut d = Doc {
            orig,
            add: AddBuf::new(),
            pieces: Vec::new(),
            len: n,
            unresolved: false,
            version: 0,
        };
        if n > 0 {
            let nl = d.nl_range(false, 0, n);
            d.unresolved = nl == UNK;
            d.pieces.push(Piece {
                add: false,
                start: 0,
                len: n,
                nl,
            });
        }
        d
    }

    pub fn empty() -> Doc {
        Doc::new(orig::empty())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn orig(&self) -> &Orig {
        &self.orig
    }

    #[inline]
    fn src(&self, p: Piece) -> &[u8] {
        self.raw(p.add, p.start, p.start + p.len)
    }

    #[inline]
    fn raw(&self, add: bool, a: usize, b: usize) -> &[u8] {
        if add {
            self.add.get(a, b - a)
        } else {
            &self.orig.bytes()[a..b]
        }
    }

    // ------------------------------------------------------------ radräkning

    /// Antal '\n' före källposition x, om känt.
    #[inline]
    fn before(&self, add: bool, x: usize) -> Option<u64> {
        if add {
            Some(self.add.nl_before(x))
        } else {
            self.orig.idx.nl_before(self.orig.bytes(), x)
        }
    }

    #[inline]
    fn find_nl(&self, add: bool, t: u64) -> Option<usize> {
        if add {
            self.add.find_nl(t)
        } else {
            self.orig.idx.find_nl(self.orig.bytes(), t)
        }
    }

    /// Antal '\n' i källintervall [a, b), eller UNK.
    fn nl_range(&self, add: bool, a: usize, b: usize) -> u64 {
        if b - a <= DIRECT {
            return count_nl(self.raw(add, a, b));
        }
        match (self.before(add, a), self.before(add, b)) {
            (Some(x), Some(y)) => y - x,
            _ => UNK,
        }
    }

    /// Del av en piece med radantal (återanvänder cachat värde när möjligt).
    fn sub(&self, p: Piece, off: usize, len: usize) -> Piece {
        let nl = if off == 0 && len == p.len {
            p.nl
        } else {
            self.nl_range(p.add, p.start + off, p.start + off + len)
        };
        Piece {
            start: p.start + off,
            len,
            nl,
            ..p
        }
    }

    fn mark(&mut self, ps: &[Piece]) {
        if ps.iter().any(|p| p.nl == UNK) {
            self.unresolved = true;
        }
    }

    /// Fyller i radantal som bakgrundsindexet hunnit räkna. Returnerar true om
    /// något ändrades. Billig när allt redan är känt.
    pub fn resolve(&mut self) -> bool {
        if !self.unresolved {
            return false;
        }
        let mut changed = false;
        let mut still = false;
        for i in 0..self.pieces.len() {
            let p = self.pieces[i];
            if p.nl == UNK {
                let nl = self.nl_range(p.add, p.start, p.start + p.len);
                if nl == UNK {
                    still = true;
                } else {
                    self.pieces[i].nl = nl;
                    changed = true;
                }
            }
        }
        self.unresolved = still;
        changed
    }

    /// 0-baserat radnummer för pos, om känt.
    pub fn line_of(&self, pos: usize) -> Option<u64> {
        let mut acc = 0u64;
        let mut at = 0usize;
        for p in &self.pieces {
            if pos < at + p.len {
                let part = self.nl_range(p.add, p.start, p.start + (pos - at));
                return (part != UNK).then_some(acc + part);
            }
            if p.nl == UNK {
                return None;
            }
            acc += p.nl;
            at += p.len;
        }
        Some(acc)
    }

    /// Totalt antal rader, om känt.
    pub fn lines(&self) -> Option<u64> {
        self.line_of(self.len).map(|n| n + 1)
    }

    /// Uppskattat radantal (för gutterbredd innan indexet är klart).
    pub fn est_lines(&self) -> u64 {
        if let Some(n) = self.lines() {
            return n;
        }
        let (bytes, nl) = self.orig.idx.sample();
        let avg = if nl > 0 {
            bytes as f64 / nl as f64
        } else {
            32.0
        };
        (self.len as f64 / avg.max(1.0) * 1.1) as u64 + 1
    }

    /// Indexeringsgrad 0–1 för källfilen, None när klar.
    pub fn index_progress(&self) -> Option<f32> {
        (!self.orig.idx.complete()).then(|| self.orig.idx.progress())
    }

    /// Start på 0-baserad rad n, om känd (n bortom sista raden → None).
    pub fn pos_of_line(&self, n: u64) -> Option<usize> {
        if n == 0 {
            return Some(0);
        }
        let mut acc = 0u64;
        let mut at = 0usize;
        for p in &self.pieces {
            if p.nl == UNK {
                // Piecen är inte färdigräknad, men målraden kan ligga i den
                // redan indexerade början (Ctrl+G under pågående indexering).
                let i = self
                    .before(p.add, p.start)
                    .and_then(|b0| self.find_nl(p.add, b0 + (n - acc)))?;
                return (i < p.start + p.len).then(|| at + (i - p.start) + 1);
            }
            if acc + p.nl >= n {
                let t = n - acc;
                let i = if p.len <= DIRECT {
                    memchr::memchr_iter(b'\n', self.src(*p)).nth(t as usize - 1)?
                } else {
                    let b0 = self.before(p.add, p.start)?;
                    self.find_nl(p.add, b0 + t)? - p.start
                };
                return Some(at + i + 1);
            }
            acc += p.nl;
            at += p.len;
        }
        None
    }

    // ------------------------------------------------------------ redigering

    /// (piece-index, offset). pos == len → (pieces.len(), 0).
    fn locate(&self, pos: usize) -> (usize, usize) {
        let mut acc = 0;
        for (i, p) in self.pieces.iter().enumerate() {
            if pos < acc + p.len {
                return (i, pos - acc);
            }
            acc += p.len;
        }
        (self.pieces.len(), 0)
    }

    fn splice_at(&mut self, pos: usize, new: &[Piece]) {
        let (pi, off) = self.locate(pos);
        if off == 0 {
            self.pieces.splice(pi..pi, new.iter().copied());
        } else {
            let p = self.pieces[pi];
            let left = self.sub(p, 0, off);
            let right = self.sub(p, off, p.len - off);
            self.mark(&[left, right]);
            let mid = std::iter::once(left)
                .chain(new.iter().copied())
                .chain(std::iter::once(right));
            self.pieces.splice(pi..pi + 1, mid);
        }
        self.mark(new);
    }

    pub fn insert(&mut self, pos: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        debug_assert!(pos <= self.len);
        self.version += 1;
        let (start, fresh_seg) = self.add.append(bytes);
        let n = bytes.len();
        let nl = count_nl(bytes);
        let (pi, off) = self.locate(pos);
        // Löpande skrivning: förläng föregående add-piece istället för att splitta.
        // Aldrig över en segmentgräns (piecen måste ligga i ett segment).
        if off == 0 && pi > 0 && !fresh_seg {
            let prev = &mut self.pieces[pi - 1];
            if prev.add && prev.start + prev.len == start {
                prev.len += n;
                prev.nl += nl;
                self.len += n;
                return;
            }
        }
        self.splice_at(
            pos,
            &[Piece {
                add: true,
                start,
                len: n,
                nl,
            }],
        );
        self.len += n;
    }

    pub fn delete(&mut self, pos: usize, n: usize) -> Removed {
        let n = n.min(self.len.saturating_sub(pos));
        let mut removed = Removed {
            pieces: Vec::new(),
            len: n,
        };
        if n == 0 {
            return removed;
        }
        self.version += 1;
        let (first, mut off) = self.locate(pos);
        let mut i = first;
        let mut rem = n;
        let mut keep: Vec<Piece> = Vec::with_capacity(2);
        while rem > 0 {
            let p = self.pieces[i];
            let take = (p.len - off).min(rem);
            if off > 0 {
                keep.push(self.sub(p, 0, off));
            }
            removed.pieces.push(self.sub(p, off, take));
            if off + take < p.len {
                keep.push(self.sub(p, off + take, p.len - off - take));
            }
            rem -= take;
            off = 0;
            i += 1;
        }
        self.mark(&keep);
        self.pieces.splice(first..i, keep);
        self.len -= n;
        removed
    }

    pub fn insert_removed(&mut self, pos: usize, r: &Removed) {
        if r.len == 0 {
            return;
        }
        self.version += 1;
        self.splice_at(pos, &r.pieces);
        self.len += r.len;
    }

    pub fn removed_bytes(&self, r: &Removed) -> Vec<u8> {
        let mut v = Vec::with_capacity(r.len);
        for &p in &r.pieces {
            v.extend_from_slice(self.src(p));
        }
        v
    }

    pub fn slice(&self, a: usize, b: usize) -> Vec<u8> {
        let b = b.min(self.len);
        let mut v = Vec::with_capacity(b.saturating_sub(a));
        let (mut pi, mut off) = self.locate(a);
        let mut rem = b.saturating_sub(a);
        while rem > 0 && pi < self.pieces.len() {
            let s = self.src(self.pieces[pi]);
            let take = (s.len() - off).min(rem);
            v.extend_from_slice(&s[off..off + take]);
            rem -= take;
            pi += 1;
            off = 0;
        }
        v
    }

    pub fn materialize(&self) -> Vec<u8> {
        self.slice(0, self.len)
    }

    pub fn snapshot(&self) -> Snap {
        let parts = self
            .pieces
            .iter()
            .map(|p| {
                if p.add {
                    let (base, s) = self.add.seg(p.start);
                    Part {
                        src: PartSrc::Add(s.clone()),
                        a: p.start - base,
                        n: p.len,
                    }
                } else {
                    Part {
                        src: PartSrc::Orig(self.orig.clone()),
                        a: p.start,
                        n: p.len,
                    }
                }
            })
            .collect();
        Snap {
            parts,
            len: self.len,
        }
    }

    // ------------------------------------------------------------ iteration

    pub fn fwd(&self, pos: usize) -> Fwd<'_> {
        let (pi, off) = self.locate(pos);
        let cur: &[u8] = if pi < self.pieces.len() {
            self.src(self.pieces[pi])
        } else {
            &[]
        };
        Fwd {
            d: self,
            pi,
            cur,
            i: off,
        }
    }

    /// Bytes före `pos`, i omvänd ordning.
    pub fn back(&self, pos: usize) -> Back<'_> {
        let (pi, off) = self.locate(pos);
        let cur: &[u8] = if pi < self.pieces.len() {
            self.src(self.pieces[pi])
        } else {
            &[]
        };
        Back {
            d: self,
            pi,
            cur,
            i: off,
        }
    }

    /// (tecken eller None för ogiltig byte, antal bytes)
    pub fn chars(&self, pos: usize) -> Chars<'_> {
        Chars { it: self.fwd(pos) }
    }

    #[inline]
    pub fn byte(&self, pos: usize) -> Option<u8> {
        self.fwd(pos).next()
    }

    // ------------------------------------------------------------ rader

    /// Första '\n' i källintervall [a, b). Index när det finns, annars memchr
    /// (högst `budget` bytes; Err om budgeten tar slut).
    fn first_nl(
        &self,
        add: bool,
        a: usize,
        b: usize,
        budget: &mut usize,
    ) -> Result<Option<usize>, ()> {
        if b - a > DIRECT
            && let Some(ca) = self.before(add, a)
        {
            if let Some(cb) = self.before(add, b) {
                if ca == cb {
                    return Ok(None);
                }
                if let Some(i) = self.find_nl(add, ca + 1) {
                    return Ok(Some(i));
                }
            } else if let Some(i) = self.find_nl(add, ca + 1) {
                // Nästa kända '\n' efter a; ligger det bortom piecen finns inget i den.
                return Ok((i < b).then_some(i));
            }
        }
        if b - a > *budget {
            return match memchr::memchr(b'\n', self.raw(add, a, a + *budget)) {
                Some(i) => Ok(Some(a + i)),
                None => Err(()),
            };
        }
        *budget -= b - a;
        Ok(memchr::memchr(b'\n', self.raw(add, a, b)).map(|i| a + i))
    }

    /// Sista '\n' i källintervall [a, b).
    fn last_nl(&self, add: bool, a: usize, b: usize) -> Option<usize> {
        if b - a > DIRECT
            && let (Some(ca), Some(cb)) = (self.before(add, a), self.before(add, b))
        {
            return if ca == cb {
                None
            } else {
                self.find_nl(add, cb)
            };
        }
        memchr::memrchr(b'\n', self.raw(add, a, b)).map(|i| a + i)
    }

    /// Start på raden efter `pos` med begränsad skanning. Unknown = indexet har
    /// inte nått fram och budgeten tog slut (renderaren slutar då rita rader).
    pub fn next_line_bounded(&self, pos: usize, mut budget: usize) -> Next {
        let (mut pi, mut off) = self.locate(pos);
        let mut at = pos;
        while pi < self.pieces.len() {
            let p = self.pieces[pi];
            let from = p.start + off;
            let to = p.start + p.len;
            match self.first_nl(p.add, from, to, &mut budget) {
                Ok(Some(i)) => return Next::At(at + (i - from) + 1),
                Ok(None) => {}
                Err(()) => return Next::Unknown,
            }
            at += to - from;
            pi += 1;
            off = 0;
        }
        Next::End
    }

    pub fn next_line_start(&self, pos: usize) -> Option<usize> {
        match self.next_line_bounded(pos, usize::MAX) {
            Next::At(p) => Some(p),
            _ => None,
        }
    }

    pub fn line_start(&self, pos: usize) -> usize {
        let (pi, off) = self.locate(pos);
        let mut at = pos - off;
        if pi < self.pieces.len() && off > 0 {
            let p = self.pieces[pi];
            if let Some(i) = self.last_nl(p.add, p.start, p.start + off) {
                return at + (i - p.start) + 1;
            }
        }
        for j in (0..pi).rev() {
            let p = self.pieces[j];
            at -= p.len;
            if p.nl == 0 {
                continue;
            }
            if let Some(i) = self.last_nl(p.add, p.start, p.start + p.len) {
                return at + (i - p.start) + 1;
            }
        }
        0
    }

    /// Position för radens terminator (\r i CRLF, annars \n), eller len.
    pub fn line_end(&self, pos: usize) -> usize {
        let e = match self.next_line_start(pos) {
            Some(n) => n - 1,
            None => return self.len,
        };
        if e > pos && self.byte(e - 1) == Some(b'\r') {
            e - 1
        } else {
            e
        }
    }

    pub fn prev_line_start(&self, pos: usize) -> Option<usize> {
        let s = self.line_start(pos);
        if s == 0 {
            None
        } else {
            Some(self.line_start(s - 1))
        }
    }

    // ------------------------------------------------------------ teckenvis rörelse

    /// Ett tecken vänster; CRLF och UTF-8-sekvenser är en enhet.
    pub fn left(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut b = self.back(pos);
        if b.next() == Some(b'\n') && b.next() == Some(b'\r') {
            return pos - 2;
        }
        let mut d = 0;
        for byte in self.back(pos).take(4) {
            d += 1;
            if byte & 0xc0 != 0x80 {
                break;
            }
        }
        match self.chars(pos - d).next() {
            Some((Some(_), n)) if n == d => pos - d,
            _ => pos - 1,
        }
    }

    pub fn right(&self, pos: usize) -> usize {
        if pos >= self.len {
            return self.len;
        }
        let mut f = self.fwd(pos);
        if f.next() == Some(b'\r') && f.next() == Some(b'\n') {
            return pos + 2;
        }
        pos + self.chars(pos).next().map_or(1, |c| c.1)
    }

    pub fn word_right(&self, pos: usize) -> usize {
        let mut it = self.fwd(pos).peekable();
        let mut p = pos;
        let Some(&b0) = it.peek() else { return pos };
        let c0 = class(b0);
        while it.peek().is_some_and(|&b| class(b) == c0) {
            it.next();
            p += 1;
        }
        if c0 != 1 {
            while it.peek().is_some_and(|&b| class(b) == 0) {
                it.next();
                p += 1;
            }
        }
        p
    }

    pub fn word_left(&self, pos: usize) -> usize {
        let mut it = self.back(pos).peekable();
        let mut p = pos;
        while it.peek().is_some_and(|&b| class(b) == 0) {
            it.next();
            p -= 1;
        }
        let Some(&b0) = it.peek() else { return p };
        let c0 = class(b0);
        while it.peek().is_some_and(|&b| class(b) == c0) {
            it.next();
            p -= 1;
        }
        p
    }

    pub fn vcol(&self, pos: usize) -> usize {
        let s = self.line_start(pos);
        let (mut p, mut c) = (s, 0);
        for (ch, n) in self.chars(s) {
            if p >= pos {
                break;
            }
            c += if ch == Some('\t') { TAB - c % TAB } else { 1 };
            p += n;
        }
        c
    }

    /// Byteposition på raden som börjar i `ls` närmast (≤) visuell kolumn `want`.
    pub fn pos_at_vcol(&self, ls: usize, want: usize) -> usize {
        let (mut p, mut c) = (ls, 0);
        for (ch, n) in self.chars(ls) {
            match ch {
                Some('\n') => break,
                Some('\r') if self.byte(p + 1) == Some(b'\n') => break,
                _ => {}
            }
            let w = if ch == Some('\t') { TAB - c % TAB } else { 1 };
            if c + w > want {
                break;
            }
            c += w;
            p += n;
        }
        p
    }

    /// Ordet (eller sjoken av blanktecken/skiljetecken) under `pos` för dubbelklick.
    /// Står pos precis efter ett ord räknas det ordet. Begränsat till WORD_SCAN.
    pub fn word_at(&self, pos: usize) -> (usize, usize) {
        let here = self.byte(pos).filter(|&b| class(b) != 1);
        let before = (pos > 0).then(|| self.byte(pos - 1)).flatten();
        let c = match (here, before) {
            (Some(b), Some(p)) if class(b) != 2 && class(p) == 2 => 2,
            (Some(b), _) => class(b),
            (None, Some(p)) if class(p) != 1 => class(p),
            _ => return (pos, pos),
        };
        let a = pos
            - self
                .back(pos)
                .take(WORD_SCAN)
                .take_while(|&b| class(b) == c)
                .count();
        let z = pos
            + self
                .fwd(pos)
                .take(WORD_SCAN)
                .take_while(|&b| class(b) == c)
                .count();
        (a, z)
    }

    /// Matchande parentes för den vid markören (eller precis före).
    /// Returnerar (parentes vid markören, dess partner). Begränsad sökning.
    pub fn match_bracket(&self, cursor: usize) -> Option<(usize, usize)> {
        const PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];
        let cands = [Some(cursor), cursor.checked_sub(1)];
        for p in cands.into_iter().flatten() {
            let Some(b) = self.byte(p) else { continue };
            for (open, close) in PAIRS {
                let mut depth = 0usize;
                if b == open {
                    for (k, x) in self.fwd(p + 1).take(BRACKET_SCAN).enumerate() {
                        if x == open {
                            depth += 1;
                        } else if x == close {
                            if depth == 0 {
                                return Some((p, p + 1 + k));
                            }
                            depth -= 1;
                        }
                    }
                    return None;
                }
                if b == close {
                    for (k, x) in self.back(p).take(BRACKET_SCAN).enumerate() {
                        if x == close {
                            depth += 1;
                        } else if x == open {
                            if depth == 0 {
                                return Some((p, p - 1 - k));
                            }
                            depth -= 1;
                        }
                    }
                    return None;
                }
            }
        }
        None
    }

    /// CRLF om första radslutet är CRLF eller om filen saknar radslut.
    pub fn detect_crlf(&self) -> bool {
        let mut prev = 0u8;
        for b in self.fwd(0).take(1 << 16) {
            if b == b'\n' {
                return prev == b'\r';
            }
            prev = b;
        }
        true
    }
}

#[derive(Clone)]
pub struct Fwd<'a> {
    d: &'a Doc,
    pi: usize,
    cur: &'a [u8],
    i: usize,
}

impl Iterator for Fwd<'_> {
    type Item = u8;
    #[inline]
    fn next(&mut self) -> Option<u8> {
        loop {
            if self.i < self.cur.len() {
                let b = self.cur[self.i];
                self.i += 1;
                return Some(b);
            }
            self.pi += 1;
            if self.pi >= self.d.pieces.len() {
                self.cur = &[];
                self.i = 0;
                return None;
            }
            self.cur = self.d.src(self.d.pieces[self.pi]);
            self.i = 0;
        }
    }
}

pub struct Back<'a> {
    d: &'a Doc,
    pi: usize,
    cur: &'a [u8],
    i: usize,
}

impl Iterator for Back<'_> {
    type Item = u8;
    #[inline]
    fn next(&mut self) -> Option<u8> {
        while self.i == 0 {
            if self.pi == 0 {
                return None;
            }
            self.pi -= 1;
            self.cur = self.d.src(self.d.pieces[self.pi]);
            self.i = self.cur.len();
        }
        self.i -= 1;
        Some(self.cur[self.i])
    }
}

pub struct Chars<'a> {
    it: Fwd<'a>,
}

impl Iterator for Chars<'_> {
    type Item = (Option<char>, usize);
    #[inline]
    fn next(&mut self) -> Option<(Option<char>, usize)> {
        let b0 = self.it.next()?;
        let n = utf8_len(b0);
        if n == 1 {
            return Some((Some(b0 as char), 1));
        }
        if n == 0 {
            return Some((None, 1));
        }
        let mut buf = [b0, 0, 0, 0];
        let mut probe = self.it.clone();
        for slot in buf.iter_mut().take(n).skip(1) {
            match probe.next() {
                Some(b) if b & 0xc0 == 0x80 => *slot = b,
                _ => return Some((None, 1)),
            }
        }
        match std::str::from_utf8(&buf[..n]) {
            Ok(s) => {
                self.it = probe;
                Some((s.chars().next(), n))
            }
            Err(_) => Some((None, 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(d: &Doc) -> String {
        String::from_utf8(d.materialize()).unwrap()
    }

    fn doc(b: &[u8]) -> Doc {
        Doc::new(orig::owned(b.to_vec()))
    }

    fn wait_index(d: &mut Doc) {
        let t0 = std::time::Instant::now();
        while d.index_progress().is_some() && t0.elapsed().as_secs() < 10 {
            std::thread::yield_now();
        }
        d.resolve();
    }

    #[test]
    fn edits() {
        let mut d = doc(b"hello\r\nworld");
        d.insert(5, b"!");
        d.insert(6, b"?");
        assert_eq!(s(&d), "hello!?\r\nworld");
        let r = d.delete(3, 6);
        assert_eq!(s(&d), "helworld");
        assert_eq!(d.removed_bytes(&r), b"lo!?\r\n");
        d.insert_removed(3, &r);
        assert_eq!(s(&d), "hello!?\r\nworld");
        assert_eq!(d.slice(2, 9), b"llo!?\r\n");
    }

    #[test]
    fn lines() {
        let d = doc(b"ab\r\n\tc\nxyz");
        assert_eq!(d.line_start(5), 4);
        assert_eq!(d.line_end(0), 2);
        assert_eq!(d.next_line_start(0), Some(4));
        assert_eq!(d.prev_line_start(8), Some(4));
        assert_eq!(d.left(4), 2);
        assert_eq!(d.right(2), 4);
        assert_eq!(d.vcol(6), 5);
        assert_eq!(d.pos_at_vcol(4, 2), 4);
        assert_eq!(d.pos_at_vcol(4, 4), 5);
        assert!(d.detect_crlf());
        assert_eq!(d.next_line_start(7), None);
        assert_eq!(d.line_of(0), Some(0));
        assert_eq!(d.line_of(5), Some(1));
        assert_eq!(d.line_of(10), Some(2));
        assert_eq!(d.lines(), Some(3));
        assert_eq!(d.pos_of_line(1), Some(4));
        assert_eq!(d.pos_of_line(2), Some(7));
        assert_eq!(d.pos_of_line(3), None);
    }

    #[test]
    fn utf8() {
        // "åä€😀x" + ogiltig byte 0xff + "y"
        let mut v = "åä€😀x".as_bytes().to_vec();
        v.push(0xff);
        v.push(b'y');
        let d = doc(&v);
        let mut p = 0;
        let mut stops = vec![0];
        while p < d.len() {
            p = d.right(p);
            stops.push(p);
        }
        assert_eq!(stops, vec![0, 2, 4, 7, 11, 12, 13, 14]);
        let mut back = vec![d.len()];
        while p > 0 {
            p = d.left(p);
            back.push(p);
        }
        back.reverse();
        assert_eq!(back, stops);
        assert_eq!(d.vcol(11), 4);
        assert_eq!(d.pos_at_vcol(0, 3), 7);
        assert_eq!(d.word_right(0), 14);
    }

    /// Slumpade redigeringar på stor fil: radfunktionerna ska stämma mot en
    /// naiv modell oavsett piece-struktur och indexläge.
    #[test]
    fn line_model_fuzz() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % n.max(1) as u64) as usize
        };
        let mut base = Vec::new();
        while base.len() < 3 * CH + 777 {
            let l = rnd(120);
            base.extend(std::iter::repeat_n(b'a', l));
            base.extend_from_slice(if rnd(3) == 0 { b"\r\n" } else { b"\n" });
        }
        // Ett jättelångt radlöst block för att tvinga indexvägen.
        base.extend(std::iter::repeat_n(b'z', 5 * CH));
        base.extend_from_slice(b"\ntail");
        let mut d = doc(&base);
        let mut m = base.clone();
        wait_index(&mut d);
        for round in 0..400 {
            let pos = rnd(m.len() + 1);
            if rnd(3) == 0 && m.len() > 10 {
                let n = rnd(300).min(m.len() - pos);
                d.delete(pos, n);
                m.drain(pos..pos + n);
            } else {
                let mut ins = Vec::new();
                for _ in 0..rnd(20) {
                    ins.push(if rnd(4) == 0 { b'\n' } else { b'q' });
                }
                if rnd(50) == 0 {
                    ins.extend(std::iter::repeat_n(b'w', 3 * ACH + 5));
                    ins.push(b'\n');
                }
                d.insert(pos, &ins);
                m.splice(pos..pos, ins);
            }
            d.resolve();
            if round % 20 != 0 {
                continue;
            }
            assert_eq!(d.materialize(), m);
            let nl: Vec<usize> = m
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i)
                .collect();
            assert_eq!(d.lines(), Some(nl.len() as u64 + 1));
            for _ in 0..30 {
                let p = rnd(m.len() + 1);
                let want_line = nl.partition_point(|&i| i < p) as u64;
                assert_eq!(d.line_of(p), Some(want_line), "line_of {p}");
                let ls = nl
                    .get((want_line as usize).wrapping_sub(1))
                    .map_or(0, |i| i + 1);
                assert_eq!(d.line_start(p), ls, "line_start {p}");
                let nx = nl.get(want_line as usize).map(|i| i + 1);
                assert_eq!(d.next_line_start(p), nx, "next_line_start {p}");
                assert_eq!(d.pos_of_line(want_line), Some(ls), "pos_of_line");
            }
        }
    }

    #[test]
    fn words_and_brackets() {
        let d = doc("hej på dig, (a[b]{c}) )".as_bytes());
        assert_eq!(d.word_at(1), (0, 3));
        assert_eq!(d.word_at(3), (0, 3)); // precis efter ordet
        assert_eq!(d.word_at(4), (4, 7)); // "på" med å
        let s = "hej på dig, (a[b]{c}) )".as_bytes();
        let open = s.iter().position(|&b| b == b'(').unwrap();
        let close = s.iter().rposition(|&b| b == b')').unwrap() - 2;
        assert_eq!(d.match_bracket(open), Some((open, close)));
        assert_eq!(d.match_bracket(close + 1), Some((close, open)));
        let lb = open + 2;
        assert_eq!(d.match_bracket(lb), Some((lb, lb + 2)));
        assert_eq!(d.match_bracket(s.len()), None); // ensam ')'
        assert_eq!(d.match_bracket(0), None);
        let e = doc(b"ab\ncd");
        assert_eq!(e.word_at(2), (0, 2));
        assert_eq!(e.word_at(3), (3, 5));
    }

    #[test]
    fn typing_across_segment_boundary() {
        let mut d = doc(b"");
        let mut m = Vec::new();
        for i in 0..(3 * SEG_MAX) {
            let c = if i % 50 == 49 {
                b'\n'
            } else {
                b'a' + (i % 26) as u8
            };
            d.insert(d.len(), &[c]);
            m.push(c);
        }
        assert_eq!(d.materialize(), m);
        assert_eq!(
            d.lines(),
            Some(m.iter().filter(|&&b| b == b'\n').count() as u64 + 1)
        );
    }

    #[test]
    fn snapshot_reads_through_segments() {
        let mut d = doc(b"0123456789");
        for i in 0..2000 {
            let pos = (i * 7) % (d.len() + 1);
            d.insert(pos, format!("<{i}>").as_bytes());
        }
        let big = vec![b'B'; SEG_MAX + 10];
        d.insert(3, &big);
        let snap = d.snapshot();
        let mut v = Vec::new();
        for p in &snap.parts {
            v.extend_from_slice(p.bytes());
        }
        assert_eq!(v, d.materialize());
        assert_eq!(snap.len, d.len());
    }
}
