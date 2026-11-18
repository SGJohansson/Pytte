//! Piece table över mmap:ad källfil + append-buffert. Inga radindex:
//! allt är byteoffset, så öppning är O(1) oavsett filstorlek.
//! Texten tolkas som UTF-8; ogiltiga bytes lämnas orörda och stegas en i taget.

use crate::winfile::Orig;

pub const TAB: usize = 4;

#[derive(Clone, Copy)]
struct Piece {
    add: bool,
    start: usize,
    len: usize,
}

/// Bortklippt intervall som piece-referenser (O(pieces), inte O(bytes)).
/// Giltigt så länge Doc lever: orig är oföränderlig och add är append-only.
pub struct Removed {
    pieces: Vec<Piece>,
    pub len: usize,
}

pub struct Doc {
    orig: Orig,
    add: Vec<u8>,
    pieces: Vec<Piece>,
    len: usize,
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
        let pieces = if n > 0 {
            vec![Piece {
                add: false,
                start: 0,
                len: n,
            }]
        } else {
            Vec::new()
        };
        Doc {
            orig,
            add: Vec::new(),
            pieces,
            len: n,
        }
    }

    pub fn empty() -> Doc {
        Doc::new(Orig::Empty)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn src(&self, p: Piece) -> &[u8] {
        if p.add {
            &self.add[p.start..p.start + p.len]
        } else {
            &self.orig.bytes()[p.start..p.start + p.len]
        }
    }

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
            let left = Piece { len: off, ..p };
            let right = Piece {
                start: p.start + off,
                len: p.len - off,
                ..p
            };
            let mid = std::iter::once(left)
                .chain(new.iter().copied())
                .chain(std::iter::once(right));
            self.pieces.splice(pi..pi + 1, mid);
        }
    }

    pub fn insert(&mut self, pos: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        debug_assert!(pos <= self.len);
        let start = self.add.len();
        self.add.extend_from_slice(bytes);
        let n = bytes.len();
        let (pi, off) = self.locate(pos);
        // Löpande skrivning: förläng föregående add-piece istället för att splitta.
        if off == 0 && pi > 0 {
            let prev = &mut self.pieces[pi - 1];
            if prev.add && prev.start + prev.len == start {
                prev.len += n;
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
        let (first, mut off) = self.locate(pos);
        let mut i = first;
        let mut rem = n;
        let mut keep: Vec<Piece> = Vec::with_capacity(2);
        while rem > 0 {
            let p = self.pieces[i];
            let take = (p.len - off).min(rem);
            if off > 0 {
                keep.push(Piece { len: off, ..p });
            }
            removed.pieces.push(Piece {
                start: p.start + off,
                len: take,
                ..p
            });
            if off + take < p.len {
                keep.push(Piece {
                    start: p.start + off + take,
                    len: p.len - off - take,
                    ..p
                });
            }
            rem -= take;
            off = 0;
            i += 1;
        }
        self.pieces.splice(first..i, keep);
        self.len -= n;
        removed
    }

    pub fn insert_removed(&mut self, pos: usize, r: &Removed) {
        if r.len == 0 {
            return;
        }
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

    pub fn line_start(&self, pos: usize) -> usize {
        match self.back(pos).position(|b| b == b'\n') {
            Some(k) => pos - k,
            None => 0,
        }
    }

    /// Position för radens terminator (\r i CRLF, annars \n), eller len.
    pub fn line_end(&self, pos: usize) -> usize {
        let e = match self.fwd(pos).position(|b| b == b'\n') {
            Some(k) => pos + k,
            None => return self.len,
        };
        if e > pos && self.byte(e - 1) == Some(b'\r') {
            e - 1
        } else {
            e
        }
    }

    pub fn next_line_start(&self, pos: usize) -> Option<usize> {
        self.fwd(pos).position(|b| b == b'\n').map(|k| pos + k + 1)
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

    #[test]
    fn edits() {
        let mut d = Doc::new(Orig::Owned(b"hello\r\nworld".to_vec()));
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
        let d = Doc::new(Orig::Owned(b"ab\r\n\tc\nxyz".to_vec()));
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
    }

    #[test]
    fn utf8() {
        // "åä€😀x" + ogiltig byte 0xff + "y"
        let mut v = "åä€😀x".as_bytes().to_vec();
        v.push(0xff);
        v.push(b'y');
        let d = Doc::new(Orig::Owned(v));
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
}
