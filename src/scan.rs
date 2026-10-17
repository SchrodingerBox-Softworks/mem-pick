//! Движок поиска: первый поиск по всей памяти, отсев найденного, снимок
//! памяти для поиска неизвестного значения. Всё работает в фоновом потоке
//! и раскладывается по ядрам; интерфейс видит только прогресс.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::process::{Process, Region, RegionFilter};
use crate::value::{Number, Scalar, ValueKind, pack, unpack};

/// Сколько читаем за раз при проходе по региону.
const CHUNK: usize = 1 << 20;
const PAGE: usize = 4096;
/// Окно, в которое собираются соседние находки при отсеве: одно чтение
/// вместо тысяч мелких.
const BLOCK: usize = 64 * 1024;
/// Сколько находок отдаётся одному потоку за раз при отсеве.
const HITS_PER_TASK: usize = 16 * 1024;
/// Предел для снимка памяти при поиске неизвестного значения.
const SNAPSHOT_LIMIT: u64 = 3 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Exact,
    Bigger,
    Smaller,
    Between,
    Unknown,
    Increased,
    Decreased,
    IncreasedBy,
    DecreasedBy,
    Changed,
    Unchanged,
}

impl Mode {
    pub const FIRST: [Mode; 5] = [
        Self::Exact,
        Self::Bigger,
        Self::Smaller,
        Self::Between,
        Self::Unknown,
    ];
    pub const NEXT: [Mode; 10] = [
        Self::Exact,
        Self::Increased,
        Self::Decreased,
        Self::Changed,
        Self::Unchanged,
        Self::IncreasedBy,
        Self::DecreasedBy,
        Self::Bigger,
        Self::Smaller,
        Self::Between,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "Точное значение",
            Self::Bigger => "Больше чем",
            Self::Smaller => "Меньше чем",
            Self::Between => "Между",
            Self::Unknown => "Неизвестное значение",
            Self::Increased => "Увеличилось",
            Self::Decreased => "Уменьшилось",
            Self::IncreasedBy => "Увеличилось на",
            Self::DecreasedBy => "Уменьшилось на",
            Self::Changed => "Изменилось",
            Self::Unchanged => "Не изменилось",
        }
    }

    pub fn needs_value(self) -> bool {
        matches!(
            self,
            Self::Exact
                | Self::Bigger
                | Self::Smaller
                | Self::Between
                | Self::IncreasedBy
                | Self::DecreasedBy
        )
    }

    pub fn needs_second_value(self) -> bool {
        self == Self::Between
    }

    /// Работает ли режим с текстом и массивами байт. Их предыдущее значение
    /// не хранится, поэтому доступен только точный поиск.
    pub fn supports_variable(self) -> bool {
        self == Self::Exact
    }
}

/// Условие отбора, уже разобранное под конкретный тип.
#[derive(Debug, Clone)]
enum Pred {
    Any,
    Bytes(Vec<u8>),
    /// Массив байт с масками: `false` в маске — любой байт.
    Pattern(Vec<u8>, Vec<bool>),
    Near(f64, f64),
    Gt(Scalar),
    Lt(Scalar),
    Between(Scalar, Scalar),
    Increased,
    Decreased,
    IncreasedBy(Number),
    DecreasedBy(Number),
    Changed,
    Unchanged,
}

fn compile(kind: ValueKind, mode: Mode, v1: &str, v2: &str) -> Result<Pred, String> {
    if kind.is_variable() {
        return match mode {
            Mode::Exact if kind == ValueKind::Bytes => {
                let (bytes, mask) = crate::value::parse_pattern(v1)?;
                Ok(if mask.iter().all(|&known| known) {
                    Pred::Bytes(bytes)
                } else {
                    Pred::Pattern(bytes, mask)
                })
            }
            Mode::Exact => Ok(Pred::Bytes(kind.encode(v1)?)),
            _ => Err(format!(
                "Для текста и массива байт доступен только режим «{}»",
                Mode::Exact.label()
            )),
        };
    }
    let num = |text: &str| kind.parse_number(text);
    Ok(match mode {
        Mode::Unknown => Pred::Any,
        Mode::Exact => {
            let n = num(v1)?;
            match n.value {
                Scalar::Float(f) => Pred::Near(f, n.tolerance),
                Scalar::Int(_) => Pred::Bytes(kind.encode_scalar(n.value)),
            }
        }
        Mode::Bigger => Pred::Gt(num(v1)?.value),
        Mode::Smaller => Pred::Lt(num(v1)?.value),
        Mode::Between => {
            let (a, b) = (num(v1)?.value, num(v2)?.value);
            if a.as_f64() <= b.as_f64() {
                Pred::Between(a, b)
            } else {
                Pred::Between(b, a)
            }
        }
        Mode::Increased => Pred::Increased,
        Mode::Decreased => Pred::Decreased,
        Mode::IncreasedBy => Pred::IncreasedBy(num(v1)?),
        Mode::DecreasedBy => Pred::DecreasedBy(num(v1)?),
        Mode::Changed => Pred::Changed,
        Mode::Unchanged => Pred::Unchanged,
    })
}

fn cmp(a: Scalar, b: Scalar) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Scalar::Int(x), Scalar::Int(y)) => Some(x.cmp(&y)),
        _ => a.as_f64().partial_cmp(&b.as_f64()),
    }
}

fn delta_matches(cur: Scalar, prev: Scalar, by: Number, sign: i128) -> bool {
    match (cur, prev, by.value) {
        (Scalar::Int(c), Scalar::Int(p), Scalar::Int(b)) => c - p == sign * b,
        _ => {
            let diff = (cur.as_f64() - prev.as_f64()) * sign as f64;
            (diff - by.value.as_f64()).abs() <= by.tolerance.max(1e-6)
        }
    }
}

impl Pred {
    fn test(&self, kind: ValueKind, cur: &[u8], prev: &[u8]) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Self::Any => true,
            Self::Bytes(needle) => cur == needle.as_slice(),
            Self::Pattern(bytes, mask) => {
                cur.len() == bytes.len()
                    && cur
                        .iter()
                        .zip(bytes)
                        .zip(mask)
                        .all(|((c, b), known)| !known || c == b)
            }
            Self::Changed => cur != prev,
            Self::Unchanged => cur == prev,
            _ => {
                let Some(c) = kind.decode(cur) else {
                    return false;
                };
                match self {
                    Self::Near(v, tol) => (c.as_f64() - v).abs() <= *tol,
                    Self::Gt(v) => cmp(c, *v) == Some(Greater),
                    Self::Lt(v) => cmp(c, *v) == Some(Less),
                    Self::Between(a, b) => {
                        cmp(c, *a).is_some_and(|o| o != Less)
                            && cmp(c, *b).is_some_and(|o| o != Greater)
                    }
                    _ => {
                        let Some(p) = kind.decode(prev) else {
                            return false;
                        };
                        match self {
                            Self::Increased => cmp(c, p) == Some(Greater),
                            Self::Decreased => cmp(c, p) == Some(Less),
                            Self::IncreasedBy(by) => delta_matches(c, p, *by, 1),
                            Self::DecreasedBy(by) => delta_matches(c, p, *by, -1),
                            _ => false,
                        }
                    }
                }
            }
        }
    }

    fn needs_previous(&self) -> bool {
        matches!(
            self,
            Self::Increased
                | Self::Decreased
                | Self::IncreasedBy(_)
                | Self::DecreasedBy(_)
                | Self::Changed
                | Self::Unchanged
        )
    }
}

// ---------- результаты ----------

/// Одна находка: адрес и значение на момент последнего поиска.
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub addr: usize,
    pub prev: u64,
}

/// Находки одного типа. При автопоиске их несколько — по одной на тип.
#[derive(Debug, Clone)]
pub struct Bucket {
    pub kind: ValueKind,
    /// Сколько байт занимает значение: размер типа или длина строки.
    pub len: usize,
    pub hits: Vec<Hit>,
}

/// Копия памяти для поиска неизвестного значения.
#[derive(Debug)]
pub struct Snapshot {
    pub kind: ValueKind,
    pub aligned: bool,
    pub blocks: Vec<(usize, Vec<u8>)>,
    pub bytes: usize,
}

#[derive(Debug)]
pub enum Results {
    Hits {
        buckets: Vec<Bucket>,
        /// Находок было больше лимита, лишние отброшены.
        truncated: bool,
    },
    Snapshot(Snapshot),
}

impl Results {
    pub fn count(&self) -> usize {
        match self {
            Self::Hits { buckets, .. } => buckets.iter().map(|b| b.hits.len()).sum(),
            Self::Snapshot(_) => 0,
        }
    }

    /// Находка по сквозному номеру строки в таблице.
    pub fn get(&self, mut index: usize) -> Option<(ValueKind, usize, Hit)> {
        let Self::Hits { buckets, .. } = self else {
            return None;
        };
        for bucket in buckets {
            if index < bucket.hits.len() {
                return Some((bucket.kind, bucket.len, bucket.hits[index]));
            }
            index -= bucket.hits.len();
        }
        None
    }

    pub fn kinds(&self) -> Vec<ValueKind> {
        match self {
            Self::Hits { buckets, .. } => buckets.iter().map(|b| b.kind).collect(),
            Self::Snapshot(s) => vec![s.kind],
        }
    }
}

// ---------- фоновая задача ----------

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub found: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let total = self.total.load(Ordering::Relaxed).max(1);
        (self.done.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) as f32
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

pub struct ScanOutcome {
    pub results: Results,
    pub elapsed: std::time::Duration,
    pub regions: usize,
    pub scanned_bytes: u64,
}

pub struct Job {
    pub progress: Arc<Progress>,
    pub first: bool,
    handle: Option<JoinHandle<Result<ScanOutcome, String>>>,
}

impl Job {
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn cancel(&self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }

    pub fn join(mut self) -> Result<ScanOutcome, String> {
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(result)) => result,
            _ => Err("Поток поиска упал".into()),
        }
    }
}

pub struct FirstScan {
    /// Типы для поиска. Больше одного — это автопоиск.
    pub kinds: Vec<ValueKind>,
    pub mode: Mode,
    pub value: String,
    pub value2: String,
    pub aligned: bool,
    pub filter: RegionFilter,
    pub max_results: usize,
    /// Заморозить процесс на время поиска, чтобы значения не менялись
    /// посреди прохода.
    pub pause: bool,
}

pub struct NextScan {
    pub mode: Mode,
    pub value: String,
    pub value2: String,
    pub pause: bool,
}

/// Держит процесс на паузе, пока жив, и отпускает при выходе из области
/// видимости — в том числе если поиск отменили или он упал с ошибкой.
struct PauseGuard<'a>(Option<&'a Process>);

impl<'a> PauseGuard<'a> {
    fn new(process: &'a Process, pause: bool) -> Self {
        Self((pause && process.suspend()).then_some(process))
    }
}

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        if let Some(process) = self.0 {
            process.resume();
        }
    }
}

/// Запускает первый поиск в фоне. `wake` зовётся по завершении, чтобы
/// интерфейс перерисовался, даже если мышь не двигается.
pub fn start_first(
    process: Arc<Process>,
    params: FirstScan,
    wake: impl Fn() + Send + 'static,
) -> Result<Job, String> {
    let preds = compile_all(&params.kinds, params.mode, &params.value, &params.value2)?;
    if params.mode == Mode::Unknown && preds.len() != 1 {
        return Err("Для неизвестного значения выберите конкретный тип".into());
    }
    let progress = Arc::new(Progress::default());
    let p = progress.clone();
    let handle = std::thread::spawn(move || {
        let started = Instant::now();
        let paused = PauseGuard::new(&process, params.pause);
        let regions = process.regions(params.filter);
        let total: u64 = regions.iter().map(|r| r.size as u64).sum();
        p.total.store(total, Ordering::Relaxed);

        let results = if params.mode == Mode::Unknown {
            snapshot(&process, &regions, preds[0].0, params.aligned, &p)
        } else {
            first_scan(
                &process,
                &regions,
                &preds,
                params.aligned,
                params.max_results,
                &p,
            )
        };
        drop(paused);
        wake();
        results.map(|results| ScanOutcome {
            results,
            elapsed: started.elapsed(),
            regions: regions.len(),
            scanned_bytes: total,
        })
    });
    Ok(Job {
        progress,
        first: true,
        handle: Some(handle),
    })
}

/// Запускает отсев: из прошлых находок остаются те, что подходят под условие.
pub fn start_next(
    process: Arc<Process>,
    previous: Arc<Results>,
    params: NextScan,
    wake: impl Fn() + Send + 'static,
) -> Result<Job, String> {
    let kinds = previous.kinds();
    let mut preds = Vec::new();
    let mut last_error = None;
    for kind in kinds {
        match compile(kind, params.mode, &params.value, &params.value2) {
            Ok(pred) => preds.push(Some(pred)),
            // Тип, в который новое значение не помещается, отсеивается целиком:
            // при автопоиске «300» после «100» байтовые находки уже не нужны.
            Err(e) => {
                preds.push(None);
                last_error = Some(e);
            }
        }
    }
    if preds.iter().all(Option::is_none) {
        return Err(last_error.unwrap_or_else(|| "Нечего отсеивать".into()));
    }

    let progress = Arc::new(Progress::default());
    let p = progress.clone();
    let handle = std::thread::spawn(move || {
        let started = Instant::now();
        let paused = PauseGuard::new(&process, params.pause);
        let results = match &*previous {
            Results::Hits { buckets, truncated } => {
                let total: u64 = buckets.iter().map(|b| b.hits.len() as u64).sum();
                p.total.store(total, Ordering::Relaxed);
                next_from_hits(&process, buckets, &preds, &p).map(|buckets| Results::Hits {
                    buckets,
                    truncated: *truncated,
                })
            }
            Results::Snapshot(snap) => {
                p.total.store(snap.bytes as u64, Ordering::Relaxed);
                let pred = preds[0].clone().expect("snapshot has one kind");
                next_from_snapshot(&process, snap, &pred, &p)
            }
        };
        drop(paused);
        wake();
        results.map(|results| ScanOutcome {
            results,
            elapsed: started.elapsed(),
            regions: 0,
            scanned_bytes: 0,
        })
    });
    Ok(Job {
        progress,
        first: false,
        handle: Some(handle),
    })
}

/// Разбирает условие под каждый тип. Типы, в которые значение не помещается,
/// при автопоиске молча выпадают; ошибка — только если не подошёл ни один.
fn compile_all(
    kinds: &[ValueKind],
    mode: Mode,
    v1: &str,
    v2: &str,
) -> Result<Vec<(ValueKind, usize, Pred)>, String> {
    let mut out = Vec::new();
    let mut last_error = None;
    for &kind in kinds {
        let candidates = std::iter::once(kind).chain(
            (kinds.len() > 1)
                .then(|| kind.unsigned_twin())
                .flatten()
                .filter(|twin| !kinds.contains(twin)),
        );
        for candidate in candidates {
            match compile(candidate, mode, v1, v2) {
                Ok(pred) => {
                    let len = match &pred {
                        Pred::Bytes(b) | Pred::Pattern(b, _) => b.len(),
                        _ => candidate.size(),
                    };
                    out.push((candidate, len, pred));
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
    }
    if out.is_empty() {
        return Err(last_error.unwrap_or_else(|| "Не выбран тип".into()));
    }
    Ok(out)
}

/// Делит работу на `count` задач и раздаёт их потокам по мере готовности.
/// Результаты возвращаются в порядке задач.
pub(crate) fn parallel<T: Send>(count: usize, work: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(count.max(1));
    let next = AtomicUsize::new(0);
    let mut collected: Vec<(usize, T)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        local.push((i, work(i)));
                    }
                    local
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_default())
            .collect()
    });
    collected.sort_by_key(|(i, _)| *i);
    collected.into_iter().map(|(_, t)| t).collect()
}

/// Регион, порезанный на куски по `CHUNK`: так потоки делят работу ровнее,
/// чем по целым регионам, где один может быть на гигабайт.
pub(crate) fn chunks(regions: &[Region]) -> Vec<Region> {
    let mut out = Vec::new();
    for r in regions {
        let mut off = 0;
        while off < r.size {
            let size = CHUNK.min(r.size - off);
            out.push(Region {
                base: r.base + off,
                size,
            });
            off += size;
        }
    }
    out
}

fn first_scan(
    process: &Process,
    regions: &[Region],
    preds: &[(ValueKind, usize, Pred)],
    aligned: bool,
    max_results: usize,
    progress: &Progress,
) -> Result<Results, String> {
    let overlap = preds.iter().map(|(_, len, _)| *len).max().unwrap_or(1) - 1;
    // Кусок читается с «нахлёстом» в начало следующего, чтобы не потерять
    // значение, которое лежит ровно на стыке. Нахлёст не выходит за регион.
    let pieces: Vec<(Region, usize)> = regions
        .iter()
        .flat_map(|r| {
            let end = r.base + r.size;
            chunks(std::slice::from_ref(r))
                .into_iter()
                .map(move |c| (c, (end - (c.base + c.size)).min(overlap)))
        })
        .collect();
    let truncated = AtomicBool::new(false);

    let per_piece = parallel(pieces.len(), |i| {
        let (piece, extra) = pieces[i];
        let mut found: Vec<Vec<Hit>> = vec![Vec::new(); preds.len()];
        if progress.cancelled() || truncated.load(Ordering::Relaxed) {
            return found;
        }
        let mut buf = vec![0u8; piece.size + extra];
        let n = process.read(piece.base, &mut buf);
        if n == buf.len() {
            scan_buffer(&buf, piece.base, piece.size, preds, aligned, &mut found);
        } else {
            // Часть страниц не читается — идём по одной.
            let mut page = vec![0u8; PAGE];
            let mut off = 0;
            while off < piece.size {
                let len = PAGE.min(piece.size - off);
                if process.read_exact(piece.base + off, &mut page[..len]) {
                    scan_buffer(
                        &page[..len],
                        piece.base + off,
                        len,
                        preds,
                        aligned,
                        &mut found,
                    );
                }
                off += len;
            }
        }
        let count: usize = found.iter().map(Vec::len).sum();
        let total = progress.found.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
        if total as usize >= max_results {
            truncated.store(true, Ordering::Relaxed);
        }
        progress
            .done
            .fetch_add(piece.size as u64, Ordering::Relaxed);
        found
    });

    if progress.cancelled() {
        return Err("Поиск остановлен".into());
    }

    let mut buckets: Vec<Bucket> = preds
        .iter()
        .map(|(kind, len, _)| Bucket {
            kind: *kind,
            len: *len,
            hits: Vec::new(),
        })
        .collect();
    let mut left = max_results;
    for found in per_piece {
        for (bucket, hits) in buckets.iter_mut().zip(found) {
            let take = hits.len().min(left);
            bucket.hits.extend_from_slice(&hits[..take]);
            left -= take;
        }
    }
    buckets.retain(|b| !b.hits.is_empty());
    Ok(Results::Hits {
        buckets,
        truncated: truncated.load(Ordering::Relaxed),
    })
}

/// Проверяет смещения `0..scan_len` в буфере; байты после `scan_len` — только
/// хвост для значений, которые начинаются в конце куска.
fn scan_buffer(
    buf: &[u8],
    base: usize,
    scan_len: usize,
    preds: &[(ValueKind, usize, Pred)],
    aligned: bool,
    found: &mut [Vec<Hit>],
) {
    for ((kind, len, pred), out) in preds.iter().zip(found.iter_mut()) {
        let len = *len;
        if buf.len() < len {
            continue;
        }
        let step = if aligned { kind.alignment() } else { 1 };
        let last = scan_len.min(buf.len() - len + 1);
        let start = (step - base % step) % step;

        if let Pred::Bytes(needle) = pred {
            // Самый частый случай — точное целое. Сравниваем байты без
            // декодирования, первым байтом отсекаем почти всё.
            let first = needle[0];
            let mut i = start;
            while i < last {
                if buf[i] == first && &buf[i..i + len] == needle.as_slice() {
                    out.push(Hit {
                        addr: base + i,
                        prev: pack(&buf[i..i + len]),
                    });
                }
                i += step;
            }
        } else {
            let mut i = start;
            while i < last {
                let cur = &buf[i..i + len];
                if pred.test(*kind, cur, cur) {
                    out.push(Hit {
                        addr: base + i,
                        prev: pack(cur),
                    });
                }
                i += step;
            }
        }
    }
}

fn snapshot(
    process: &Process,
    regions: &[Region],
    kind: ValueKind,
    aligned: bool,
    progress: &Progress,
) -> Result<Results, String> {
    let total: u64 = regions.iter().map(|r| r.size as u64).sum();
    if total > SNAPSHOT_LIMIT {
        return Err(format!(
            "Слишком много памяти для снимка ({} МБ). Включите «Только записываемая память» \
             или начните с точного значения.",
            total >> 20
        ));
    }
    let pieces = chunks(regions);
    let blocks = parallel(pieces.len(), |i| {
        let piece = pieces[i];
        let mut out = Vec::new();
        if progress.cancelled() {
            return out;
        }
        let mut buf = vec![0u8; piece.size];
        if process.read_exact(piece.base, &mut buf) {
            out.push((piece.base, buf));
        } else {
            // Часть страниц не читается — запоминаем те, что читаются.
            for (k, page) in buf.chunks(PAGE).enumerate() {
                let base = piece.base + k * PAGE;
                let mut copy = vec![0u8; page.len()];
                if process.read_exact(base, &mut copy) {
                    out.push((base, copy));
                }
            }
        }
        progress
            .done
            .fetch_add(piece.size as u64, Ordering::Relaxed);
        out
    });
    if progress.cancelled() {
        return Err("Поиск остановлен".into());
    }
    let blocks: Vec<(usize, Vec<u8>)> = blocks.into_iter().flatten().collect();
    let bytes = blocks.iter().map(|(_, b)| b.len()).sum();
    Ok(Results::Snapshot(Snapshot {
        kind,
        aligned,
        blocks,
        bytes,
    }))
}

fn next_from_snapshot(
    process: &Process,
    snap: &Snapshot,
    pred: &Pred,
    progress: &Progress,
) -> Result<Results, String> {
    let kind = snap.kind;
    let len = kind.size();
    let step = if snap.aligned { kind.alignment() } else { 1 };

    let per_block = parallel(snap.blocks.len(), |i| {
        let (base, old) = &snap.blocks[i];
        let mut hits = Vec::new();
        if progress.cancelled() {
            return hits;
        }
        let mut cur = vec![0u8; old.len()];
        if process.read_exact(*base, &mut cur) && cur.len() >= len {
            let start = (step - base % step) % step;
            let mut i = start;
            while i + len <= cur.len() {
                let now = &cur[i..i + len];
                if pred.test(kind, now, &old[i..i + len]) {
                    hits.push(Hit {
                        addr: base + i,
                        prev: pack(now),
                    });
                }
                i += step;
            }
        }
        progress.done.fetch_add(old.len() as u64, Ordering::Relaxed);
        progress
            .found
            .fetch_add(hits.len() as u64, Ordering::Relaxed);
        hits
    });
    if progress.cancelled() {
        return Err("Поиск остановлен".into());
    }
    let hits: Vec<Hit> = per_block.into_iter().flatten().collect();
    let buckets = if hits.is_empty() {
        Vec::new()
    } else {
        vec![Bucket { kind, len, hits }]
    };
    Ok(Results::Hits {
        buckets,
        truncated: false,
    })
}

fn next_from_hits(
    process: &Process,
    buckets: &[Bucket],
    preds: &[Option<Pred>],
    progress: &Progress,
) -> Result<Vec<Bucket>, String> {
    // Задача — кусок находок одного типа. Внутри куска находки идут по
    // возрастанию адреса, поэтому соседние читаются одним блоком.
    let mut tasks = Vec::new();
    for (b, bucket) in buckets.iter().enumerate() {
        if preds[b].is_none() {
            progress
                .done
                .fetch_add(bucket.hits.len() as u64, Ordering::Relaxed);
            continue;
        }
        let mut start = 0;
        while start < bucket.hits.len() {
            let end = (start + HITS_PER_TASK).min(bucket.hits.len());
            tasks.push((b, start, end));
            start = end;
        }
    }

    let per_task = parallel(tasks.len(), |t| {
        let (b, start, end) = tasks[t];
        let bucket = &buckets[b];
        let pred = preds[b].as_ref().expect("task only for compiled kinds");
        // Новая строка может быть другой длины, чем старая.
        let len = match pred {
            Pred::Bytes(needle) | Pred::Pattern(needle, _) => needle.len(),
            _ => bucket.len,
        };
        let mut kept = Vec::new();
        if progress.cancelled() {
            return (b, len, kept);
        }
        let hits = &bucket.hits[start..end];
        let mut buf = vec![0u8; BLOCK + len];
        let mut single = vec![0u8; len];
        let mut i = 0;
        while i < hits.len() {
            let block_start = hits[i].addr;
            let mut j = i;
            let mut block_end = block_start;
            while j < hits.len() && hits[j].addr + len - block_start <= BLOCK + len {
                block_end = block_end.max(hits[j].addr + len);
                j += 1;
            }
            let span = block_end - block_start;
            let whole = process.read_exact(block_start, &mut buf[..span]);
            for hit in &hits[i..j] {
                let cur: &[u8] = if whole {
                    let off = hit.addr - block_start;
                    &buf[off..off + len]
                } else if process.read_exact(hit.addr, &mut single) {
                    &single
                } else {
                    continue;
                };
                let prev = unpack(hit.prev);
                let prev = &prev[..len.min(8)];
                let matches = if pred.needs_previous() {
                    pred.test(bucket.kind, cur, prev)
                } else {
                    pred.test(bucket.kind, cur, cur)
                };
                if matches {
                    kept.push(Hit {
                        addr: hit.addr,
                        prev: pack(cur),
                    });
                }
            }
            i = j;
        }
        progress
            .done
            .fetch_add(hits.len() as u64, Ordering::Relaxed);
        progress
            .found
            .fetch_add(kept.len() as u64, Ordering::Relaxed);
        (b, len, kept)
    });
    if progress.cancelled() {
        return Err("Поиск остановлен".into());
    }

    let mut out: Vec<Bucket> = buckets
        .iter()
        .map(|b| Bucket {
            kind: b.kind,
            len: b.len,
            hits: Vec::new(),
        })
        .collect();
    for (b, len, kept) in per_task {
        out[b].len = len;
        out[b].hits.extend(kept);
    }
    out.retain(|b| !b.hits.is_empty());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(job: Job) -> Results {
        job.join().expect("scan finished").results
    }

    fn own() -> Arc<Process> {
        Arc::new(Process::open(std::process::id(), "self").expect("open self"))
    }

    fn filter() -> RegionFilter {
        RegionFilter {
            writable_only: true,
            include_mapped: false,
        }
    }

    fn contains(results: &Results, addr: usize) -> bool {
        (0..results.count()).any(|i| results.get(i).is_some_and(|(_, _, h)| h.addr == addr))
    }

    #[test]
    fn predicates() {
        let k = ValueKind::I32;
        let b = |v: i32| v.to_le_bytes();
        let gt = compile(k, Mode::Bigger, "10", "").unwrap();
        assert!(gt.test(k, &b(11), &b(0)));
        assert!(!gt.test(k, &b(10), &b(0)));
        let between = compile(k, Mode::Between, "20", "10").unwrap();
        assert!(between.test(k, &b(10), &b(0)));
        assert!(between.test(k, &b(20), &b(0)));
        assert!(!between.test(k, &b(21), &b(0)));
        let inc_by = compile(k, Mode::IncreasedBy, "5", "").unwrap();
        assert!(inc_by.test(k, &b(15), &b(10)));
        assert!(!inc_by.test(k, &b(16), &b(10)));
        let dec_by = compile(k, Mode::DecreasedBy, "5", "").unwrap();
        assert!(dec_by.test(k, &b(5), &b(10)));

        let f = ValueKind::F32;
        let exact = compile(f, Mode::Exact, "87", "").unwrap();
        assert!(exact.test(f, &87.3f32.to_le_bytes(), &[0; 4]));
        assert!(!exact.test(f, &87.6f32.to_le_bytes(), &[0; 4]));
    }

    #[test]
    fn auto_falls_back_to_unsigned() {
        let preds = compile_all(&[ValueKind::I8, ValueKind::I32], Mode::Exact, "200", "").unwrap();
        let kinds: Vec<_> = preds.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(kinds, vec![ValueKind::U8, ValueKind::I32]);
        assert!(compile_all(&[ValueKind::I32], Mode::Exact, "abc", "").is_err());
    }

    #[test]
    fn finds_and_filters_value_in_own_memory() {
        // Число, которого больше нигде в памяти теста быть не должно.
        let mut target = Box::new(0x5A17_C0DEi32);
        let addr = &*target as *const i32 as usize;
        let process = own();

        let job = start_first(
            process.clone(),
            FirstScan {
                kinds: vec![ValueKind::I32],
                mode: Mode::Exact,
                value: (*target).to_string(),
                value2: String::new(),
                aligned: true,
                filter: filter(),
                max_results: 1_000_000,
                pause: false,
            },
            || {},
        )
        .unwrap();
        let first = Arc::new(wait(job));
        assert!(contains(&first, addr), "first scan must find the box");

        *target += 7;
        std::hint::black_box(&target);
        let job = start_next(
            process,
            first,
            NextScan {
                mode: Mode::IncreasedBy,
                value: "7".into(),
                value2: String::new(),
                pause: false,
            },
            || {},
        )
        .unwrap();
        let next = wait(job);
        assert!(contains(&next, addr), "filter must keep the box");
    }

    #[test]
    fn finds_byte_pattern_with_wildcards() {
        let target = Box::new([0x4Du8, 0xE1, 0x7A, 0x9C, 0x33, 0x5B, 0xC4, 0x02]);
        let addr = target.as_ptr() as usize;
        let job = start_first(
            own(),
            FirstScan {
                kinds: vec![ValueKind::Bytes],
                mode: Mode::Exact,
                value: "4D E1 ?? 9C 33 ?? C4".into(),
                value2: String::new(),
                aligned: false,
                filter: filter(),
                max_results: 1_000_000,
                pause: false,
            },
            || {},
        )
        .unwrap();
        let results = wait(job);
        assert!(contains(&results, addr));
        std::hint::black_box(&target);
    }

    #[test]
    fn unknown_value_snapshot_then_changed() {
        let mut target = Box::new(1234.5f64);
        let addr = &*target as *const f64 as usize;
        let process = own();

        let job = start_first(
            process.clone(),
            FirstScan {
                kinds: vec![ValueKind::F64],
                mode: Mode::Unknown,
                value: String::new(),
                value2: String::new(),
                aligned: true,
                filter: filter(),
                max_results: 1_000_000,
                pause: false,
            },
            || {},
        )
        .unwrap();
        let snap = Arc::new(wait(job));
        assert!(matches!(&*snap, Results::Snapshot(_)));

        *target -= 100.0;
        std::hint::black_box(&target);
        let job = start_next(
            process,
            snap,
            NextScan {
                mode: Mode::Decreased,
                value: String::new(),
                value2: String::new(),
                pause: false,
            },
            || {},
        )
        .unwrap();
        let next = wait(job);
        assert!(contains(&next, addr));
    }
}
