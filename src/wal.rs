//! Append-only write-ahead-logg per flik + sessionsmanifest.
//! All disk-I/O sker på en egen tråd; UI-tråden skickar bara meddelanden.
//!
//! Post:   [u32 len][payload][u32 crc32(payload)]
//! Payload 0 = header: u64 base_len, u64 base_mtime, u32 plen, path (utf-8, tom = namnlös)
//!         1 = insert: u64 pos, bytes
//!         2 = delete: u64 pos, u64 n

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use crate::winfile::WinFile;

// ---------------------------------------------------------------- crc32

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC: [u32; 256] = crc_table();

fn crc32(d: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in d {
        c = CRC[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

// ---------------------------------------------------------------- poster

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 8);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v.extend_from_slice(&crc32(payload).to_le_bytes());
    v
}

pub fn rec_header(base_len: u64, base_mtime: u64, path: Option<&str>) -> Vec<u8> {
    let p = path.unwrap_or("").as_bytes();
    let mut v = vec![0u8];
    v.extend_from_slice(&base_len.to_le_bytes());
    v.extend_from_slice(&base_mtime.to_le_bytes());
    v.extend_from_slice(&(p.len() as u32).to_le_bytes());
    v.extend_from_slice(p);
    frame(&v)
}

pub fn rec_ins(pos: usize, bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&(pos as u64).to_le_bytes());
    v.extend_from_slice(bytes);
    frame(&v)
}

pub fn rec_del(pos: usize, n: usize) -> Vec<u8> {
    let mut v = vec![2u8];
    v.extend_from_slice(&(pos as u64).to_le_bytes());
    v.extend_from_slice(&(n as u64).to_le_bytes());
    frame(&v)
}

pub enum Op {
    Ins(usize, Vec<u8>),
    Del(usize, usize),
}

pub struct Parsed {
    pub base_len: u64,
    pub base_mtime: u64,
    pub path: Option<String>,
    /// (op, filoffset där posten börjar)
    pub ops: Vec<(Op, usize)>,
    /// Slut på sista giltiga post – allt efter är en avsliten svans.
    pub valid_end: usize,
}

fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

pub fn parse(data: &[u8]) -> Option<Parsed> {
    let mut recs: Vec<(&[u8], usize)> = Vec::new();
    let mut i = 0;
    while let Some(n) = u32_at(data, i) {
        let n = n as usize;
        let Some(p) = data.get(i + 4..i + 4 + n) else {
            break;
        };
        let Some(c) = u32_at(data, i + 4 + n) else {
            break;
        };
        if crc32(p) != c {
            break;
        }
        recs.push((p, i));
        i += 8 + n;
    }
    let valid_end = i;
    let mut it = recs.into_iter();
    let (h, _) = it.next()?;
    if h.first() != Some(&0) {
        return None;
    }
    let base_len = u64_at(h, 1)?;
    let base_mtime = u64_at(h, 9)?;
    let plen = u32_at(h, 17)? as usize;
    let path = std::str::from_utf8(h.get(21..21 + plen)?).ok()?;
    let path = if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    };

    let mut ops = Vec::new();
    let mut end = valid_end;
    for (p, off) in it {
        let op = match p.first() {
            Some(1) => u64_at(p, 1).map(|pos| Op::Ins(pos as usize, p[9..].to_vec())),
            Some(2) => u64_at(p, 1)
                .zip(u64_at(p, 9))
                .map(|(a, n)| Op::Del(a as usize, n as usize)),
            _ => None,
        };
        match op {
            Some(op) => ops.push((op, off)),
            None => {
                end = off;
                break;
            }
        }
    }
    Some(Parsed {
        base_len,
        base_mtime,
        path,
        ops,
        valid_end: end,
    })
}

// ---------------------------------------------------------------- sökvägar

pub fn wal_path(dir: &str, id: u64) -> String {
    format!("{dir}\\tab_{id}.wal")
}

pub fn list_wals(dir: &str) -> Vec<u64> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| {
        let n = e.ok()?.file_name().into_string().ok()?;
        n.strip_prefix("tab_")?.strip_suffix(".wal")?.parse().ok()
    })
    .collect()
}

/// (aktivt flik-id, [(id, cursor, top)])
pub fn read_manifest(dir: &str) -> (u64, Vec<(u64, usize, usize)>) {
    let Ok(s) = fs::read_to_string(format!("{dir}\\session.txt")) else {
        return (0, Vec::new());
    };
    let mut lines = s.lines();
    if lines.next() != Some("PYTTE 1") {
        return (0, Vec::new());
    }
    let mut active = 0;
    let mut tabs = Vec::new();
    for l in lines {
        let f: Vec<&str> = l.split(' ').collect();
        match f.as_slice() {
            ["active", id] => active = id.parse().unwrap_or(0),
            ["tab", id, c, t] => {
                if let (Ok(id), Ok(c), Ok(t)) = (id.parse(), c.parse(), t.parse()) {
                    tabs.push((id, c, t));
                }
            }
            _ => {}
        }
    }
    (active, tabs)
}

pub fn manifest_string(active: u64, tabs: impl Iterator<Item = (u64, usize, usize)>) -> String {
    let mut s = format!("PYTTE 1\nactive {active}\n");
    for (id, c, t) in tabs {
        s += &format!("tab {id} {c} {t}\n");
    }
    s
}

// ---------------------------------------------------------------- skrivtråd

pub enum Msg {
    Append(u64, Vec<u8>),
    /// Trunkera och skriv ny header (ny flik / efter sparning).
    Reset(u64, Vec<u8>),
    Remove(u64),
    Manifest(String),
    Flush(Sender<()>),
}

pub struct Wal {
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
    pub failed: Arc<AtomicBool>,
}

impl Wal {
    pub fn start(dir: String) -> Wal {
        let (tx, rx) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let f2 = failed.clone();
        let thread = std::thread::Builder::new()
            .name("wal".into())
            .spawn(move || run(dir, rx, f2))
            .expect("wal-tråd");
        Wal {
            tx: Some(tx),
            thread: Some(thread),
            failed,
        }
    }

    #[inline]
    pub fn send(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }

    /// Blockerar tills allt som skickats före anropet ligger på disk.
    pub fn flush(&self) {
        let (t, r) = mpsc::channel();
        self.send(Msg::Flush(t));
        let _ = r.recv();
    }

    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(dir: String, rx: Receiver<Msg>, failed: Arc<AtomicBool>) {
    let mut files: HashMap<u64, WinFile> = HashMap::new();
    let mut pending: HashMap<u64, Vec<u8>> = HashMap::new();
    let fail = |r: std::io::Result<()>| {
        if r.is_err() {
            failed.store(true, Ordering::Relaxed);
        }
    };

    let flush_pending = |files: &mut HashMap<u64, WinFile>, pending: &mut HashMap<u64, Vec<u8>>| {
        for (id, buf) in pending.drain() {
            let r = (|| {
                if let std::collections::hash_map::Entry::Vacant(e) = files.entry(id) {
                    e.insert(WinFile::create(&wal_path(&dir, id), false)?);
                }
                files.get_mut(&id).unwrap().write_all(&buf)
            })();
            fail(r);
        }
    };

    while let Ok(first) = rx.recv() {
        // Batcha allt som köats upp: en WriteFile per flik och varv.
        for m in std::iter::once(first).chain(rx.try_iter()) {
            match m {
                Msg::Append(id, rec) => pending.entry(id).or_default().extend_from_slice(&rec),
                other => {
                    flush_pending(&mut files, &mut pending);
                    match other {
                        Msg::Reset(id, hdr) => {
                            files.remove(&id);
                            let r = WinFile::create(&wal_path(&dir, id), true).and_then(|mut f| {
                                f.write_all(&hdr)?;
                                files.insert(id, f);
                                Ok(())
                            });
                            fail(r);
                        }
                        Msg::Remove(id) => {
                            files.remove(&id);
                            let _ = fs::remove_file(wal_path(&dir, id));
                        }
                        Msg::Manifest(s) => {
                            let tmp = format!("{dir}\\session.tmp");
                            let r = WinFile::create(&tmp, true)
                                .and_then(|mut f| f.write_all(s.as_bytes()))
                                .and_then(|_| fs::rename(&tmp, format!("{dir}\\session.txt")));
                            fail(r);
                        }
                        Msg::Flush(ack) => {
                            let _ = ack.send(());
                        }
                        Msg::Append(..) => unreachable!(),
                    }
                }
            }
        }
        flush_pending(&mut files, &mut pending);
    }
    flush_pending(&mut files, &mut pending);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_tail() {
        let mut d = rec_header(10, 99, Some("C:\\x.txt"));
        d.extend(rec_ins(3, b"abc"));
        d.extend(rec_del(1, 2));
        let good = d.len();
        d.extend(&rec_ins(0, b"zz")[..5]); // avsliten
        let p = parse(&d).unwrap();
        assert_eq!(p.path.as_deref(), Some("C:\\x.txt"));
        assert_eq!((p.base_len, p.base_mtime), (10, 99));
        assert_eq!(p.ops.len(), 2);
        assert_eq!(p.valid_end, good);
    }
}
