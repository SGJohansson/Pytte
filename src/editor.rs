//! Flikstate, redigering med ångra/gör om, asynkron laddning och
//! återställning av session från WAL.

use std::fs;

use crate::doc::{Doc, Removed};
use crate::wal::{self, Msg, Op, Parsed, Wal};
use crate::winfile::open_orig;

// ---------------------------------------------------------------- ångra

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Typing,
    Back,
    Fwd,
    Other,
}

enum Step {
    Ins { pos: usize, len: usize },
    Del { pos: usize, removed: Removed },
}

pub struct Group {
    steps: Vec<Step>,
    before: usize,
    after: usize,
    kind: Kind,
}

const UNDO_MAX: usize = 10_000;
/// Radoperationer kopierar blocket; större block vägras hellre än att frysa UI.
pub const LINE_OP_MAX: usize = 64 << 20;

/// Delar upp i (innehåll, terminator) där terminatorn är "\r\n", "\n" eller tom.
fn split_term(b: &[u8]) -> (&[u8], &[u8]) {
    if b.ends_with(b"\r\n") {
        b.split_at(b.len() - 2)
    } else if b.ends_with(b"\n") {
        b.split_at(b.len() - 1)
    } else {
        (b, &[])
    }
}

// ---------------------------------------------------------------- flik

pub struct Tab {
    pub id: u64,
    /// Visningsnummer för namnlösa flikar ("namnlös-N"). Skilt från id, som aldrig
    /// återanvänds; numret är det lägsta lediga bland öppna namnlösa flikar.
    pub no: u64,
    pub path: Option<String>,
    pub doc: Doc,
    pub cursor: usize,
    pub anchor: Option<usize>,
    /// Byteoffset för första synliga rad (alltid en radstart).
    pub top: usize,
    pub hscroll: usize,
    pub want: Option<usize>,
    pub dirty: bool,
    pub crlf: bool,
    pub loading: bool,
    /// Gutterns sifferbredd. Växer bara (aldrig layouthopp nedåt).
    pub gutter: usize,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Nästa redigering startar en ny ångra-grupp.
    sealed: bool,
}

impl Tab {
    pub fn new(id: u64, path: Option<String>, doc: Doc) -> Tab {
        let crlf = doc.detect_crlf();
        Tab {
            id,
            no: id,
            path,
            doc,
            cursor: 0,
            anchor: None,
            top: 0,
            hscroll: 0,
            want: None,
            dirty: false,
            crlf,
            loading: false,
            gutter: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            sealed: true,
        }
    }

    pub fn loading(id: u64, path: String) -> Tab {
        let mut t = Tab::new(id, Some(path), Doc::empty());
        t.loading = true;
        t
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => p.rsplit(['\\', '/']).next().unwrap_or(p).to_string(),
            None => format!("namnlös-{}", self.no),
        }
    }

    pub fn sel(&self) -> Option<(usize, usize)> {
        self.anchor
            .filter(|&a| a != self.cursor)
            .map(|a| (a.min(self.cursor), a.max(self.cursor)))
    }

    /// Anropas före varje markörförflyttning. `extend` = shift hålls ned.
    pub fn begin_move(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.sealed = true;
    }

    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn clear_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.sealed = true;
    }

    pub fn clamp(&mut self) {
        let len = self.doc.len();
        self.cursor = self.cursor.min(len);
        if self.cursor > 0
            && self.doc.byte(self.cursor) == Some(b'\n')
            && self.doc.byte(self.cursor - 1) == Some(b'\r')
        {
            self.cursor -= 1;
        }
        self.anchor = self.anchor.map(|a| a.min(len));
        self.top = self.doc.line_start(self.top.min(len));
    }

    pub fn vmove(&mut self, down: bool) {
        let want = self.want.unwrap_or_else(|| self.doc.vcol(self.cursor));
        let target = if down {
            self.doc.next_line_start(self.cursor)
        } else {
            self.doc.prev_line_start(self.cursor)
        };
        if let Some(ls) = target {
            self.cursor = self.doc.pos_at_vcol(ls, want);
        }
        self.want = Some(want);
    }

    pub fn scroll(&mut self, n: isize) {
        for _ in 0..n.unsigned_abs() {
            let next = if n > 0 {
                self.doc.next_line_start(self.top)
            } else {
                self.doc.prev_line_start(self.top)
            };
            match next {
                Some(t) => self.top = t,
                None => break,
            }
        }
    }

    pub fn ensure_visible(&mut self, rows: usize, cols: usize) {
        let cl = self.doc.line_start(self.cursor);
        if cl < self.top {
            self.top = cl;
        } else {
            let mut s = cl;
            let mut k = 0;
            while k + 1 < rows && s > self.top {
                s = self.doc.prev_line_start(s).unwrap_or(0);
                k += 1;
            }
            if s > self.top {
                self.top = s;
            }
        }
        let vc = self.doc.vcol(self.cursor);
        if vc < self.hscroll {
            self.hscroll = vc.saturating_sub(8);
        } else if vc >= self.hscroll + cols {
            self.hscroll = (vc + 8).saturating_sub(cols);
        }
    }

    // ------------------------------------------------------------ primitiv (loggar till WAL)

    fn shift_top(&mut self, a: usize, b: usize, ins: usize) {
        if b <= self.top {
            self.top = self.top - (b - a) + ins;
        } else if a < self.top {
            self.top = a;
        }
    }

    fn raw_delete(&mut self, wal: &Wal, a: usize, b: usize) -> Removed {
        let r = self.doc.delete(a, b - a);
        wal.send(Msg::Append(self.id, wal::rec_del(a, b - a)));
        self.shift_top(a, b, 0);
        r
    }

    fn raw_insert(&mut self, wal: &Wal, pos: usize, bytes: &[u8]) {
        self.doc.insert(pos, bytes);
        wal.send(Msg::Append(self.id, wal::rec_ins(pos, bytes)));
        self.shift_top(pos, pos, bytes.len());
    }

    fn raw_insert_removed(&mut self, wal: &Wal, pos: usize, r: &Removed) {
        self.doc.insert_removed(pos, r);
        wal.send(Msg::Append(
            self.id,
            wal::rec_ins(pos, &self.doc.removed_bytes(r)),
        ));
        self.shift_top(pos, pos, r.len);
    }

    fn settle(&mut self) {
        self.top = self.doc.line_start(self.top.min(self.doc.len()));
        self.anchor = None;
        self.want = None;
        self.dirty = true;
    }

    // ------------------------------------------------------------ redigering

    /// Ersätter [a, b) med `ins`. Sammanslår löpande skrivning/radering till en ångra-grupp.
    pub fn replace(&mut self, wal: &Wal, a: usize, b: usize, ins: &[u8], kind: Kind) {
        if self.loading || (a == b && ins.is_empty()) {
            return;
        }
        let before = self.cursor;
        let mut steps = Vec::with_capacity(2);
        if b > a {
            let removed = self.raw_delete(wal, a, b);
            steps.push(Step::Del { pos: a, removed });
        }
        if !ins.is_empty() {
            self.raw_insert(wal, a, ins);
            steps.push(Step::Ins {
                pos: a,
                len: ins.len(),
            });
        }
        self.cursor = a + ins.len();
        self.settle();
        self.redo.clear();

        let merge = !self.sealed
            && self.undo.last().is_some_and(|g| {
                g.kind == kind
                    && match (kind, g.steps.last(), steps.as_slice()) {
                        (
                            Kind::Typing,
                            Some(Step::Ins { pos, len }),
                            [Step::Ins { pos: p2, .. }],
                        ) => {
                            pos + len == *p2
                                && !ins.first().is_some_and(|c| c.is_ascii_whitespace())
                        }
                        (
                            Kind::Back,
                            Some(Step::Del { pos, .. }),
                            [Step::Del { pos: p2, removed }],
                        ) => p2 + removed.len == *pos,
                        (Kind::Fwd, Some(Step::Del { pos, .. }), [Step::Del { pos: p2, .. }]) => {
                            p2 == pos
                        }
                        _ => false,
                    }
            });
        if merge {
            let g = self.undo.last_mut().unwrap();
            match (kind, g.steps.last_mut()) {
                (Kind::Typing, Some(Step::Ins { len, .. })) => *len += ins.len(),
                _ => g.steps.extend(steps),
            }
            g.after = self.cursor;
        } else {
            self.undo.push(Group {
                steps,
                before,
                after: self.cursor,
                kind,
            });
            if self.undo.len() > UNDO_MAX {
                self.undo.remove(0);
            }
        }
        self.sealed = kind == Kind::Other;
    }

    // ------------------------------------------------------------ radoperationer

    /// [s, e): hela rader som täcks av markering/markör. En markering som slutar
    /// precis i en radstart tar inte med den raden.
    fn line_block(&self) -> (usize, usize) {
        let d = &self.doc;
        let (a, z) = self.sel().unwrap_or((self.cursor, self.cursor));
        let s = d.line_start(a);
        let last = if z > a && z > s && d.line_start(z) == z {
            z - 1
        } else {
            z
        };
        (s, d.next_line_start(last).unwrap_or(d.len()))
    }

    /// Ersätter [a, b) och placerar markör/ankare; ett ångra-steg.
    fn line_edit(
        &mut self,
        wal: &Wal,
        a: usize,
        b: usize,
        ins: &[u8],
        cursor: usize,
        anchor: Option<usize>,
    ) {
        self.replace(wal, a, b, ins, Kind::Other);
        self.cursor = cursor.min(self.doc.len());
        self.anchor = anchor.map(|x| x.min(self.doc.len()));
        if let Some(g) = self.undo.last_mut() {
            g.after = self.cursor;
        }
    }

    fn too_big(n: usize) -> Result<(), String> {
        if n > LINE_OP_MAX {
            Err(format!(
                "RADBLOCKET ÄR FÖR STORT (> {} MB)",
                LINE_OP_MAX >> 20
            ))
        } else {
            Ok(())
        }
    }

    /// Ctrl+D: duplicerar raden/raderna under markören. Markören står kvar.
    pub fn duplicate_lines(&mut self, wal: &Wal) -> Result<(), String> {
        if self.loading {
            return Ok(());
        }
        let (s, e) = self.line_block();
        Self::too_big(e - s)?;
        let blk = self.doc.slice(s, e);
        let (core, term) = split_term(&blk);
        let ins = if term.is_empty() {
            let nl: &[u8] = if self.crlf { b"\r\n" } else { b"\n" };
            [nl, core].concat()
        } else {
            blk.clone()
        };
        let (c, a) = (self.cursor, self.anchor);
        self.line_edit(wal, e, e, &ins, c, a);
        Ok(())
    }

    /// Alt+Upp/Ned: flyttar raden/raderna förbi grannraden. Markering följer med.
    pub fn move_lines(&mut self, wal: &Wal, down: bool) -> Result<(), String> {
        if self.loading {
            return Ok(());
        }
        let d = &self.doc;
        let (s, e) = self.line_block();
        let (r0, r1, first, second) = if down {
            if e >= d.len() {
                return Ok(());
            }
            let e2 = d.next_line_start(e).unwrap_or(d.len());
            Self::too_big(e2 - s)?;
            (s, e2, d.slice(s, e), d.slice(e, e2))
        } else {
            if s == 0 {
                return Ok(());
            }
            let p0 = d.line_start(s - 1);
            Self::too_big(e - p0)?;
            (p0, e, d.slice(p0, s), d.slice(s, e))
        };
        // second hamnar före first; varje del behåller sin terminatorposition.
        let (fc, ft) = split_term(&first);
        let (sc, st) = split_term(&second);
        let new = [sc, ft, fc, st].concat();
        // Blockets positioner: [s, s+bc] → nytt läge, terminator/slut → efter blocket.
        let map = |p: usize| -> usize {
            if down {
                if p - s <= fc.len() {
                    r0 + sc.len() + ft.len() + (p - s)
                } else {
                    r0 + new.len()
                }
            } else if p - s <= sc.len() {
                r0 + (p - s)
            } else {
                r0 + new.len()
            }
        };
        let (c, a) = (map(self.cursor), self.anchor.map(map));
        self.line_edit(wal, r0, r1, &new, c, a);
        self.want = None;
        Ok(())
    }

    /// Ctrl+Shift+K: raderar raden/raderna; markören behåller kolumnen.
    pub fn delete_lines(&mut self, wal: &Wal) {
        if self.loading {
            return;
        }
        let d = &self.doc;
        let vc = d.vcol(self.cursor);
        let (s, e) = self.line_block();
        // Sista raden utan terminator: ta föregående radslut istället.
        let a = if e == d.len() && s > 0 && d.byte(e.saturating_sub(1)) != Some(b'\n') {
            if s >= 2 && d.byte(s - 2) == Some(b'\r') {
                s - 2
            } else {
                s - 1
            }
        } else {
            s
        };
        if a == e {
            return;
        }
        self.replace(wal, a, e, b"", Kind::Other);
        let ls = self.doc.line_start(a.min(self.doc.len()));
        self.cursor = self.doc.pos_at_vcol(ls, vc);
        if let Some(g) = self.undo.last_mut() {
            g.after = self.cursor;
        }
    }

    fn apply_inverse(&mut self, wal: &Wal, g: Group) -> Group {
        let mut inv = Vec::with_capacity(g.steps.len());
        for s in g.steps.into_iter().rev() {
            match s {
                Step::Ins { pos, len } => {
                    let removed = self.raw_delete(wal, pos, pos + len);
                    inv.push(Step::Del { pos, removed });
                }
                Step::Del { pos, removed } => {
                    self.raw_insert_removed(wal, pos, &removed);
                    inv.push(Step::Ins {
                        pos,
                        len: removed.len,
                    });
                }
            }
        }
        self.cursor = g.before.min(self.doc.len());
        self.settle();
        self.sealed = true;
        Group {
            steps: inv,
            before: g.after,
            after: g.before,
            kind: Kind::Other,
        }
    }

    pub fn undo(&mut self, wal: &Wal) -> bool {
        if self.loading {
            return false;
        }
        let Some(g) = self.undo.pop() else {
            return false;
        };
        let inv = self.apply_inverse(wal, g);
        self.redo.push(inv);
        true
    }

    pub fn redo(&mut self, wal: &Wal) -> bool {
        if self.loading {
            return false;
        }
        let Some(g) = self.redo.pop() else {
            return false;
        };
        let inv = self.apply_inverse(wal, g);
        self.undo.push(inv);
        true
    }
}

// ---------------------------------------------------------------- laddning (loader-tråd)

pub enum Job {
    Open {
        path: String,
    },
    Recover {
        path: String,
        parsed: Parsed,
        wal: String,
    },
}

pub enum Outcome {
    Ready {
        doc: Doc,
        ident: (u64, u64),
        dirty: bool,
        reset: bool,
        msg: Option<String>,
    },
    /// Fliken tas bort; ev. meddelande till statusraden.
    Failed(String),
}

/// Spelar upp ops; returnerar (offset att trunkera WAL till, antal applicerade ops).
fn replay(doc: &mut Doc, p: &Parsed) -> (usize, usize) {
    for (k, (op, start)) in p.ops.iter().enumerate() {
        let ok = match op {
            Op::Ins(pos, b) if *pos <= doc.len() => {
                doc.insert(*pos, b);
                true
            }
            Op::Del(pos, n) if pos + n <= doc.len() => {
                doc.delete(*pos, *n);
                true
            }
            _ => false,
        };
        if !ok {
            return (*start, k);
        }
    }
    (p.valid_end, p.ops.len())
}

fn truncate(path: &str, len: usize) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path)
        && f.metadata().map(|m| m.len() > len as u64).unwrap_or(false)
    {
        let _ = f.set_len(len as u64);
    }
}

fn orphan(wp: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dst = format!("{wp}.orphan-{ts}");
    let _ = fs::rename(wp, &dst);
    dst
}

pub fn load(job: Job) -> Outcome {
    let t0 = std::time::Instant::now();
    match job {
        Job::Open { path } => match open_orig(&path) {
            Ok((orig, ident)) => Outcome::Ready {
                doc: Doc::new(orig),
                ident,
                dirty: false,
                reset: true,
                msg: Some(format!(
                    "ÖPPNAD {} B PÅ {:.2} MS",
                    ident.0,
                    t0.elapsed().as_secs_f64() * 1e3
                )),
            },
            Err(e) => Outcome::Failed(format!("KUNDE INTE ÖPPNA {path}: {e}")),
        },
        Job::Recover {
            path,
            parsed: p,
            wal: wp,
        } => match open_orig(&path) {
            Ok((orig, ident)) if ident == (p.base_len, p.base_mtime) => {
                let mut doc = Doc::new(orig);
                let (end, n) = replay(&mut doc, &p);
                truncate(&wp, end);
                Outcome::Ready {
                    doc,
                    ident,
                    dirty: n > 0,
                    reset: false,
                    msg: None,
                }
            }
            Ok((orig, ident)) => {
                let msg = (!p.ops.is_empty()).then(|| {
                    let o = orphan(&wp);
                    format!("{path} ÄNDRAD UTANFÖR PYTTE; OSPARADE ÄNDRINGAR FLYTTADE TILL {o}")
                });
                Outcome::Ready {
                    doc: Doc::new(orig),
                    ident,
                    dirty: false,
                    reset: true,
                    msg,
                }
            }
            Err(_) => {
                if p.ops.is_empty() {
                    let _ = fs::remove_file(&wp);
                    Outcome::Failed(String::new())
                } else {
                    let o = orphan(&wp);
                    Outcome::Failed(format!(
                        "{path} SAKNAS; OSPARADE ÄNDRINGAR FLYTTADE TILL {o}"
                    ))
                }
            }
        },
    }
}

// ---------------------------------------------------------------- återställning

#[derive(Default)]
pub struct Recovered {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub next_id: u64,
    /// Flikar med basfil laddas asynkront.
    pub jobs: Vec<(u64, Job)>,
}

/// Synkron del: läser manifest + WAL (små filer). Basfiler öppnas på loader-trådar.
pub fn recover(dir: &str) -> Recovered {
    let (active_id, man) = wal::read_manifest(dir);
    let mut ids: Vec<u64> = man.iter().map(|m| m.0).collect();
    let mut stray: Vec<u64> = wal::list_wals(dir)
        .into_iter()
        .filter(|i| !ids.contains(i))
        .collect();
    stray.sort_unstable();
    ids.extend(stray);
    ids.retain(|id| fs::metadata(wal::wal_path(dir, *id)).is_ok());

    let mut out = Recovered {
        next_id: ids.iter().max().map_or(1, |m| m + 1),
        ..Default::default()
    };

    for id in ids {
        let wp = wal::wal_path(dir, id);
        let Some(p) = fs::read(&wp).ok().and_then(|d| wal::parse(&d)) else {
            let _ = fs::remove_file(&wp);
            continue;
        };
        let mut t = match p.path.clone() {
            Some(path) => {
                out.jobs.push((
                    id,
                    Job::Recover {
                        path: path.clone(),
                        parsed: p,
                        wal: wp,
                    },
                ));
                Tab::loading(id, path)
            }
            None => {
                let mut doc = Doc::empty();
                let (end, n) = replay(&mut doc, &p);
                truncate(&wp, end);
                let mut t = Tab::new(id, None, doc);
                t.dirty = n > 0;
                t
            }
        };
        if let Some(&(_, c, top)) = man.iter().find(|m| m.0 == id) {
            t.cursor = c;
            t.top = top;
        }
        if !t.loading {
            t.clamp();
        }
        if id == active_id {
            out.active = out.tabs.len();
        }
        out.tabs.push(t);
    }
    out
}
