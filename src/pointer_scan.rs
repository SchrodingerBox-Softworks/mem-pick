//! Поиск указателей — то, ради чего обычно открывают Cheat Engine.
//!
//! Адрес значения в куче меняется при каждом запуске игры, но до него почти
//! всегда можно дойти цепочкой указателей от статического адреса внутри
//! модуля: `[[game.exe+1A2B]+10]+8`. Такая цепочка переживает перезапуск.
//!
//! Как ищем (идея та же, что у Cheat Engine, реализация своя):
//! 1. Проходим изменяемую память и выписываем все выровненные значения,
//!    похожие на указатель (попадают в выделенную память процесса).
//! 2. Сортируем их по значению — получается «обратная карта»: по адресу
//!    быстро находятся все указатели, которые смотрят чуть ниже него.
//! 3. От искомого адреса идём назад: ищем указатели на `[адрес - смещение,
//!    адрес]`, и от каждого — снова назад, пока не упрёмся в статический
//!    адрес модуля или в предел глубины. Глубину наращиваем постепенно,
//!    поэтому короткие цепочки находятся первыми.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::process::{Module, Process, Region, RegionFilter, module_at};
use crate::scan::{Progress, chunks, parallel};

/// Больше записей в обратной карте не держим: это уже гигабайты памяти.
const MAX_POINTERS: usize = 80_000_000;
const PAGE: usize = 4096;
/// Предел посещённых узлов на одну глубину, чтобы поиск не ушёл в
/// комбинаторный взрыв на огромных играх.
const MAX_VISITS_PER_DEPTH: u64 = 40_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerPath {
    pub module: String,
    pub module_offset: usize,
    /// Смещения в порядке применения: сначала к значению по базе, потом дальше.
    pub offsets: Vec<i64>,
}

impl PointerPath {
    /// Выражение для таблицы адресов: `[[game.exe+1A2B]+10]+8`.
    pub fn expression(&self) -> String {
        let mut expr = format!("{}+{:X}", self.module, self.module_offset);
        for &off in &self.offsets {
            expr = if off >= 0 {
                format!("[{expr}]+{off:X}")
            } else {
                format!("[{expr}]-{:X}", -off)
            };
        }
        expr
    }

    /// Куда цепочка указывает сейчас.
    pub fn resolve(&self, process: &Process, modules: &[Module]) -> Option<usize> {
        let module = modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(&self.module))?;
        let mut addr = module.base + self.module_offset;
        for &off in &self.offsets {
            let ptr = process.read_pointer(addr)?;
            addr = (ptr as i64).checked_add(off)? as usize;
        }
        Some(addr)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PointerScanParams {
    pub target: usize,
    pub max_level: usize,
    pub max_offset: usize,
    pub max_results: usize,
    pub pause: bool,
}

pub const PHASE_COLLECT: u8 = 0;
pub const PHASE_SEARCH: u8 = 1;

pub struct PointerOutcome {
    pub paths: Vec<PointerPath>,
    pub pointers: usize,
    pub elapsed: Duration,
    /// Поиск остановлен по лимиту, а не потому, что всё перебрал.
    pub truncated: bool,
}

pub struct PointerJob {
    pub progress: Arc<Progress>,
    pub phase: Arc<AtomicU8>,
    /// Текущая глубина на втором этапе.
    pub depth: Arc<AtomicU64>,
    handle: Option<JoinHandle<Result<PointerOutcome, String>>>,
}

impl PointerJob {
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    pub fn cancel(&self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }

    pub fn join(mut self) -> Result<PointerOutcome, String> {
        match self.handle.take().map(|h| h.join()) {
            Some(Ok(result)) => result,
            _ => Err("Поток поиска указателей упал".into()),
        }
    }
}

pub fn start(
    process: Arc<Process>,
    params: PointerScanParams,
    wake: impl Fn() + Send + 'static,
) -> PointerJob {
    let progress = Arc::new(Progress::default());
    let phase = Arc::new(AtomicU8::new(PHASE_COLLECT));
    let depth = Arc::new(AtomicU64::new(0));
    let (p, ph, d) = (progress.clone(), phase.clone(), depth.clone());
    let handle = std::thread::spawn(move || {
        let result = run(&process, params, &p, &ph, &d);
        wake();
        result
    });
    PointerJob {
        progress,
        phase,
        depth,
        handle: Some(handle),
    }
}

fn run(
    process: &Process,
    params: PointerScanParams,
    progress: &Progress,
    phase: &AtomicU8,
    depth: &AtomicU64,
) -> Result<PointerOutcome, String> {
    let started = Instant::now();
    if params.pause && process.suspend() {
        let result = collect(process, progress);
        process.resume();
        let map = result?;
        return search(process, params, map, progress, phase, depth, started);
    }
    let map = collect(process, progress)?;
    search(process, params, map, progress, phase, depth, started)
}

struct PointerMap {
    /// (значение, адрес, где оно лежит), отсортировано по значению.
    entries: Vec<(usize, usize)>,
    modules: Vec<Module>,
}

/// Выделенные диапазоны памяти, куда может смотреть указатель.
fn valid_ranges(process: &Process) -> Vec<Region> {
    process.regions(RegionFilter {
        writable_only: false,
        include_mapped: true,
    })
}

fn points_into(ranges: &[Region], value: usize) -> bool {
    let idx = ranges.partition_point(|r| r.base <= value);
    idx > 0 && {
        let r = &ranges[idx - 1];
        value < r.base + r.size
    }
}

fn collect(process: &Process, progress: &Progress) -> Result<PointerMap, String> {
    let ranges = valid_ranges(process);
    let (Some(first), Some(last)) = (ranges.first(), ranges.last()) else {
        return Err("Не удалось прочитать карту памяти процесса".into());
    };
    let (lo, hi) = (first.base, last.base + last.size);

    // Указатели хранятся в изменяемой памяти: куча, стеки, секции .data.
    let storage = process.regions(RegionFilter {
        writable_only: true,
        include_mapped: false,
    });
    let pieces = chunks(&storage);
    progress.total.store(
        storage.iter().map(|r| r.size as u64).sum(),
        Ordering::Relaxed,
    );
    let size = process.pointer_size();

    let per_piece = parallel(pieces.len(), |i| {
        let piece = pieces[i];
        let mut found = Vec::new();
        if progress.cancelled() {
            return found;
        }
        let mut collect_from = |base: usize, buf: &[u8]| {
            for (j, slot) in buf.chunks_exact(size).enumerate() {
                let value = if size == 4 {
                    u32::from_le_bytes(slot.try_into().unwrap_or_default()) as usize
                } else {
                    u64::from_le_bytes(slot.try_into().unwrap_or_default()) as usize
                };
                // Быстрый отсев по общим границам, бинарный поиск — только
                // для правдоподобных значений.
                if value >= lo && value < hi && value % 4 == 0 && points_into(&ranges, value) {
                    found.push((value, base + j * size));
                }
            }
        };
        let mut buf = vec![0u8; piece.size];
        if process.read_exact(piece.base, &mut buf) {
            collect_from(piece.base, &buf);
        } else {
            // Часть страниц успели освободить или защитить — читаем по одной.
            for (k, page) in buf.chunks_mut(PAGE).enumerate() {
                let base = piece.base + k * PAGE;
                if process.read_exact(base, page) {
                    collect_from(base, page);
                }
            }
        }
        progress
            .done
            .fetch_add(piece.size as u64, Ordering::Relaxed);
        progress
            .found
            .fetch_add(found.len() as u64, Ordering::Relaxed);
        found
    });
    if progress.cancelled() {
        return Err("Поиск указателей остановлен".into());
    }

    let total: usize = per_piece.iter().map(Vec::len).sum();
    if total > MAX_POINTERS {
        return Err(format!(
            "Слишком много кандидатов в указатели ({total}). Попробуйте, когда игра \
             займёт меньше памяти, например в главном меню."
        ));
    }
    let mut entries = Vec::with_capacity(total);
    for chunk in per_piece {
        entries.extend(chunk);
    }
    entries.sort_unstable();
    Ok(PointerMap {
        entries,
        modules: process.modules(),
    })
}

struct Search<'a> {
    map: &'a PointerMap,
    max_offset: usize,
    max_results: usize,
    progress: &'a Progress,
    visits: u64,
    results: Vec<PointerPath>,
    offsets: Vec<i64>,
    chain: Vec<usize>,
    truncated: bool,
}

impl Search<'_> {
    /// Ищет цепочки длиной ровно `left` до адреса `target`.
    fn walk(&mut self, target: usize, left: usize) {
        if self.stopped() {
            return;
        }
        // Ссылка на карту живёт дольше `&mut self`, поэтому её можно
        // держать, пока рекурсия меняет остальное состояние.
        let map = self.map;
        let entries = &map.entries;
        let lo = target.saturating_sub(self.max_offset);
        let start = entries.partition_point(|&(v, _)| v < lo);
        let end = entries.partition_point(|&(v, _)| v <= target);

        for &(value, at) in &entries[start..end] {
            self.visits += 1;
            if self.stopped() {
                return;
            }
            self.offsets.push((target - value) as i64);
            let module = module_at(&map.modules, at);
            if left == 1 {
                if let Some(m) = module {
                    let mut offsets = self.offsets.clone();
                    offsets.reverse();
                    self.results.push(PointerPath {
                        module: m.name.clone(),
                        module_offset: at - m.base,
                        offsets,
                    });
                    self.progress.found.fetch_add(1, Ordering::Relaxed);
                }
            } else if module.is_none() && !self.chain.contains(&at) {
                // Статический адрес — уже база, глубже через него не идём.
                self.chain.push(at);
                self.walk(at, left - 1);
                self.chain.pop();
            }
            self.offsets.pop();
        }
    }

    fn stopped(&mut self) -> bool {
        if self.results.len() >= self.max_results || self.visits >= MAX_VISITS_PER_DEPTH {
            self.truncated = true;
            return true;
        }
        self.progress.cancelled()
    }
}

fn search(
    process: &Process,
    params: PointerScanParams,
    map: PointerMap,
    progress: &Progress,
    phase: &AtomicU8,
    depth: &AtomicU64,
    started: Instant,
) -> Result<PointerOutcome, String> {
    let _ = process;
    phase.store(PHASE_SEARCH, Ordering::Relaxed);
    progress.found.store(0, Ordering::Relaxed);
    progress
        .total
        .store(params.max_level as u64, Ordering::Relaxed);

    let mut search = Search {
        map: &map,
        max_offset: params.max_offset,
        max_results: params.max_results,
        progress,
        visits: 0,
        results: Vec::new(),
        offsets: Vec::new(),
        chain: Vec::new(),
        truncated: false,
    };
    for level in 1..=params.max_level.max(1) {
        depth.store(level as u64, Ordering::Relaxed);
        progress.done.store(level as u64 - 1, Ordering::Relaxed);
        search.visits = 0;
        search.walk(params.target, level);
        if progress.cancelled() {
            break;
        }
        if search.results.len() >= params.max_results {
            break;
        }
    }
    if progress.cancelled() && search.results.is_empty() {
        return Err("Поиск указателей остановлен".into());
    }
    progress
        .done
        .store(params.max_level as u64, Ordering::Relaxed);

    let truncated = search.truncated;
    let mut paths = search.results;
    paths.sort_by(|a, b| {
        (a.offsets.len(), &a.module, a.module_offset, &a.offsets).cmp(&(
            b.offsets.len(),
            &b.module,
            b.module_offset,
            &b.offsets,
        ))
    });
    paths.dedup();
    Ok(PointerOutcome {
        paths,
        pointers: map.entries.len(),
        elapsed: started.elapsed(),
        truncated,
    })
}

/// Пересканирование: после перезапуска игры оставляет только цепочки,
/// которые снова приводят к нужному адресу.
pub fn rescan(
    paths: &[PointerPath],
    process: &Process,
    modules: &[Module],
    target: usize,
) -> Vec<PointerPath> {
    paths
        .iter()
        .filter(|p| p.resolve(process, modules) == Some(target))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Статическая переменная живёт в секции .data тестового exe — это и есть
    /// «статический адрес модуля», от которого должна найтись цепочка.
    static ROOT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn expression_format() {
        let path = PointerPath {
            module: "game.exe".into(),
            module_offset: 0x1A2B,
            offsets: vec![0x10, -8],
        };
        assert_eq!(path.expression(), "[[game.exe+1A2B]+10]-8");
    }

    #[test]
    fn finds_two_level_chain_in_own_process() {
        // ROOT → outer (куча) → +0x18 inner (куча) → +0x20 значение.
        let mut inner = vec![0u64; 16].into_boxed_slice();
        inner[4] = 0x0BAD_F00D;
        let inner_addr = inner.as_ptr() as usize;
        let mut outer = vec![0usize; 8].into_boxed_slice();
        outer[3] = inner_addr;
        let outer_addr = outer.as_ptr() as usize;
        ROOT.store(outer_addr, Ordering::SeqCst);
        let target = inner_addr + 0x20;

        let process = Arc::new(Process::open(std::process::id(), "self").unwrap());
        let job = start(
            process.clone(),
            PointerScanParams {
                target,
                max_level: 2,
                max_offset: 0x100,
                max_results: 100_000,
                pause: false,
            },
            || {},
        );
        let outcome = job.join().expect("pointer scan");
        let modules = process.modules();
        let root_addr = &ROOT as *const AtomicUsize as usize;
        let root_module = module_at(&modules, root_addr).expect("ROOT inside exe");

        let expected = PointerPath {
            module: root_module.name.clone(),
            module_offset: root_addr - root_module.base,
            offsets: vec![0x18, 0x20],
        };
        assert!(
            outcome.paths.contains(&expected),
            "expected {} among {} paths",
            expected.expression(),
            outcome.paths.len()
        );
        assert_eq!(expected.resolve(&process, &modules), Some(target));

        // Пересканирование отбрасывает цепочки, которые больше не ведут к цели.
        let kept = rescan(&outcome.paths, &process, &modules, target);
        assert!(kept.contains(&expected));
        assert!(rescan(&[expected], &process, &modules, target + 4).is_empty());

        std::hint::black_box((&inner, &outer));
    }
}
