//! Källdata (mmap eller ägd buffert) + radindex som byggs utanför UI-tråden.
//!
//! Indexet är en prefixsumma av '\n' per 64 KiB-chunk. Första MiB:en räknas
//! synkront på loader-tråden (små filer har exakta radnummer redan vid första
//! frame); resten räknas av en egen tråd och publiceras chunk för chunk via
//! atomics. Läsare ser alltid ett konsistent prefix, aldrig ett lås.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const CH_SHIFT: u32 = 16;
pub const CH: usize = 1 << CH_SHIFT;
/// Räknas synkront innan fliken visas (≈ 0,1 ms varm, < 1 ms kall).
const SYNC_BYTES: usize = 1 << 20;

/// Antal '\n'. 64-bytes block med u8-räknare per lane (LLVM vektoriserar till
/// pcmpeqb/psubb), tömda var 255:e block. ~12 GB/s mot ~3 GB/s för naiv filter.
pub fn count_nl(b: &[u8]) -> u64 {
    let mut n = 0u64;
    let mut it = b.chunks_exact(64);
    let mut acc = [0u8; 64];
    let mut k = 0;
    for blk in &mut it {
        for i in 0..64 {
            acc[i] = acc[i].wrapping_add((blk[i] == b'\n') as u8);
        }
        k += 1;
        if k == 255 {
            n += acc.iter().map(|&x| x as u64).sum::<u64>();
            acc = [0; 64];
            k = 0;
        }
    }
    n += acc.iter().map(|&x| x as u64).sum::<u64>();
    n + it.remainder().iter().filter(|&&x| x == b'\n').count() as u64
}

pub struct NlIndex {
    len: usize,
    /// prefix[k] = antal '\n' i [0, k*CH). Giltigt för k ≤ done.
    prefix: Box<[AtomicU64]>,
    done: AtomicUsize,
}

impl NlIndex {
    fn new(len: usize) -> NlIndex {
        let chunks = len.div_ceil(CH);
        NlIndex {
            len,
            prefix: (0..=chunks).map(|_| AtomicU64::new(0)).collect(),
            done: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn chunks(&self) -> usize {
        self.prefix.len() - 1
    }

    #[inline]
    fn done(&self) -> usize {
        self.done.load(Ordering::Acquire)
    }

    pub fn complete(&self) -> bool {
        self.done() == self.chunks()
    }

    /// 0.0–1.0
    pub fn progress(&self) -> f32 {
        let c = self.chunks();
        if c == 0 {
            1.0
        } else {
            self.done() as f32 / c as f32
        }
    }

    /// (indexerade bytes, antal '\n' i dem) – underlag för uppskattning av radantal.
    pub fn sample(&self) -> (usize, u64) {
        let d = self.done();
        (
            (d * CH).min(self.len),
            self.prefix[d].load(Ordering::Relaxed),
        )
    }

    /// Antal '\n' i data[0..x), om indexet når dit. Skannar högst en chunk.
    #[inline]
    pub fn nl_before(&self, data: &[u8], x: usize) -> Option<u64> {
        let k = x >> CH_SHIFT;
        if k > self.done() {
            return None;
        }
        let base = self.prefix[k].load(Ordering::Relaxed);
        Some(base + count_nl(&data[k << CH_SHIFT..x]))
    }

    /// Position för det t:e (1-baserade) '\n' i data, om det ligger i indexerat område.
    pub fn find_nl(&self, data: &[u8], t: u64) -> Option<usize> {
        let d = self.done();
        if t == 0 || self.prefix[d].load(Ordering::Relaxed) < t {
            return None;
        }
        // Största k < d med prefix[k] < t ≤ prefix[k+1].
        let (mut lo, mut hi) = (0usize, d);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.prefix[mid].load(Ordering::Relaxed) < t {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let k = lo;
        let skip = (t - self.prefix[k].load(Ordering::Relaxed) - 1) as usize;
        let a = k << CH_SHIFT;
        let b = ((k + 1) << CH_SHIFT).min(data.len());
        memchr::memchr_iter(b'\n', &data[a..b])
            .nth(skip)
            .map(|i| a + i)
    }

    /// Räknar vidare från `done`. Endast en skrivare åt gången (loader → indextråd
    /// överlämnas via spawn). `keep_going` kontrolleras per chunk.
    fn build(&self, data: &[u8], limit: Option<usize>, keep_going: &dyn Fn() -> bool) {
        let mut k = self.done.load(Ordering::Relaxed);
        let mut budget = limit.unwrap_or(usize::MAX);
        while k < self.chunks() {
            if !keep_going() {
                return;
            }
            let a = k << CH_SHIFT;
            let b = (a + CH).min(data.len());
            if b - a > budget {
                return;
            }
            budget -= b - a;
            let c = count_nl(&data[a..b]);
            let base = self.prefix[k].load(Ordering::Relaxed);
            self.prefix[k + 1].store(base + c, Ordering::Relaxed);
            k += 1;
            self.done.store(k, Ordering::Release);
        }
    }
}

pub enum Store {
    Empty,
    Owned(Vec<u8>),
    #[cfg(windows)]
    Mapped(crate::winfile::Mapping),
}

pub struct OrigData {
    store: Store,
    pub idx: NlIndex,
}

impl OrigData {
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        match &self.store {
            Store::Empty => &[],
            Store::Owned(v) => v,
            #[cfg(windows)]
            Store::Mapped(m) => m.bytes(),
        }
    }
}

/// Delad, oföränderlig källa. Indextråd och sökning håller egna Arc-kloner.
pub type Orig = Arc<OrigData>;

/// Bygger källa + index. Första SYNC_BYTES räknas här; resten på en egen tråd
/// som avbryter sig själv så fort ingen annan håller källan (flik stängd/sparad).
pub fn make(store: Store) -> Orig {
    let n = match &store {
        Store::Empty => 0,
        Store::Owned(v) => v.len(),
        #[cfg(windows)]
        Store::Mapped(m) => m.bytes().len(),
    };
    let o = Arc::new(OrigData {
        store,
        idx: NlIndex::new(n),
    });
    o.idx.build(o.bytes(), Some(SYNC_BYTES), &|| true);
    if !o.idx.complete() {
        let w = o.clone();
        let _ = std::thread::Builder::new()
            .name("nlidx".into())
            .spawn(move || {
                let alive = || Arc::strong_count(&w) > 1;
                w.idx.build(w.bytes(), None, &alive);
            });
    }
    o
}

pub fn empty() -> Orig {
    make(Store::Empty)
}

pub fn owned(v: Vec<u8>) -> Orig {
    make(Store::Owned(v))
}

/// Väntar (högst `ms`) tills alla bakgrundstrådar släppt källan, så att en
/// mappad fil kan ersättas. Indextråden släpper inom en chunk (~µs).
pub fn wait_released(w: &std::sync::Weak<OrigData>, ms: u64) {
    let t0 = std::time::Instant::now();
    while w.strong_count() > 0 && t0.elapsed().as_millis() < ms as u128 {
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count() {
        let mut v = vec![b'x'; 100_000];
        for i in (0..v.len()).step_by(7) {
            v[i] = b'\n';
        }
        assert_eq!(
            count_nl(&v),
            v.iter().filter(|&&b| b == b'\n').count() as u64
        );
    }

    #[test]
    fn index_queries() {
        let mut v = vec![b'a'; 5 * CH + 123];
        let nls: Vec<usize> = (0..v.len()).step_by(997).collect();
        for &i in &nls {
            v[i] = b'\n';
        }
        let o = owned(v.clone());
        let t0 = std::time::Instant::now();
        while !o.idx.complete() && t0.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        assert!(o.idx.complete());
        for x in [0, 1, 997, 998, CH, CH + 5, 3 * CH, v.len()] {
            let want = v[..x].iter().filter(|&&b| b == b'\n').count() as u64;
            assert_eq!(o.idx.nl_before(&v, x), Some(want), "x={x}");
        }
        for (t, &p) in nls.iter().enumerate() {
            assert_eq!(o.idx.find_nl(&v, t as u64 + 1), Some(p));
        }
        assert_eq!(o.idx.find_nl(&v, nls.len() as u64 + 1), None);
    }
}
