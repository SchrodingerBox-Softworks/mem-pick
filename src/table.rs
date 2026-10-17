//! Таблица адресов: то, что пользователь нашёл и хочет держать под рукой.
//! Значения можно заморозить — фоновый поток будет переписывать их, пока
//! игра пытается их изменить. Таблица сохраняется рядом с exe, отдельно
//! для каждого процесса.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::process::{Module, Process};
use crate::value::{ValueKind, eval_address};

/// Стабильный номер записи на время работы программы: по нему горячая
/// клавиша находит свою запись, даже если строки переставили или удалили.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Что делает горячая клавиша записи — тот же набор, что в Cheat Engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    ToggleFreeze,
    Freeze,
    Unfreeze,
    SetValue,
    Increase,
    Decrease,
}

impl HotkeyAction {
    pub const ALL: [HotkeyAction; 6] = [
        Self::ToggleFreeze,
        Self::Freeze,
        Self::Unfreeze,
        Self::SetValue,
        Self::Increase,
        Self::Decrease,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ToggleFreeze => "Заморозить / разморозить",
            Self::Freeze => "Заморозить",
            Self::Unfreeze => "Разморозить",
            Self::SetValue => "Записать значение",
            Self::Increase => "Увеличить на",
            Self::Decrease => "Уменьшить на",
        }
    }

    pub fn needs_value(self) -> bool {
        matches!(self, Self::SetValue | Self::Increase | Self::Decrease)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryHotkey {
    /// Комбинация в виде текста: `F1`, `Ctrl+Shift+1`, `Alt+Numpad5`.
    pub keys: String,
    pub action: HotkeyAction,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub description: String,
    /// Адрес в том виде, в каком его видит пользователь: `game.exe+1A2B`
    /// или абсолютный `7FF6A1B2C3D0`. Относительный переживает перезапуск игры.
    pub address: String,
    pub kind: ValueKind,
    /// Длина строки в байтах. Для чисел не используется.
    #[serde(default)]
    pub len: usize,
    #[serde(default)]
    pub frozen: bool,
    /// Что записывать, пока значение заморожено, — в виде текста, как его
    /// ввёл бы пользователь.
    #[serde(default)]
    pub frozen_value: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hotkeys: Vec<EntryHotkey>,
    #[serde(skip)]
    pub resolved: Option<usize>,
    /// Почему адрес не вычислился — для подсказки в таблице.
    #[serde(skip)]
    pub error: Option<String>,
    #[serde(skip, default = "next_id")]
    pub id: u64,
}

impl Entry {
    pub fn byte_len(&self) -> usize {
        if self.kind.is_variable() {
            self.len.max(1)
        } else {
            self.kind.size()
        }
    }

    pub fn new(description: String, address: String, kind: ValueKind, len: usize) -> Self {
        Self {
            description,
            address,
            kind,
            len,
            frozen: false,
            frozen_value: String::new(),
            hotkeys: Vec::new(),
            resolved: None,
            error: None,
            id: next_id(),
        }
    }

    /// Вычисляет адрес. Цепочки указателей читаются из процесса, поэтому
    /// их надо пересчитывать постоянно: игра перекладывает объекты.
    pub fn resolve(&mut self, modules: &[Module], process: Option<&Process>) {
        let read = |addr: usize| process.and_then(|p| p.read_pointer(addr));
        match eval_address(&self.address, modules, &read) {
            Ok(addr) => {
                self.resolved = Some(addr);
                self.error = None;
            }
            Err(e) => {
                self.resolved = None;
                self.error = Some(e);
            }
        }
    }

    /// Байты для заморозки, если значение заморожено и всё разбирается.
    pub fn freeze_bytes(&self) -> Option<(usize, Vec<u8>)> {
        if !self.frozen {
            return None;
        }
        let addr = self.resolved?;
        let bytes = self.kind.encode(&self.frozen_value).ok()?;
        Some((addr, bytes))
    }
}

// ---------- файлы ----------

fn tables_dir(base: &Path) -> PathBuf {
    base.join("tables")
}

/// Имя файла таблицы для процесса. Имя exe и так годится для файловой
/// системы, но на всякий случай вычищаем всё лишнее.
fn table_path(base: &Path, process_name: &str) -> PathBuf {
    let safe: String = process_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    tables_dir(base).join(format!("{safe}.json"))
}

pub fn load(base: &Path, process_name: &str) -> Vec<Entry> {
    std::fs::read_to_string(table_path(base, process_name))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(base: &Path, process_name: &str, entries: &[Entry]) {
    let path = table_path(base, process_name);
    if entries.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let _ = std::fs::create_dir_all(tables_dir(base));
    if let Ok(text) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(path, text);
    }
}

// ---------- заморозка ----------

#[derive(Default)]
struct FreezeState {
    process: Option<Arc<Process>>,
    items: Vec<(usize, Vec<u8>)>,
}

/// Фоновый поток, который переписывает замороженные значения. Отдельный
/// поток, а не кадры интерфейса: свёрнутое окно кадров не рисует, а заморозка
/// должна работать и тогда.
pub struct Freezer {
    state: Arc<Mutex<FreezeState>>,
    interval_ms: Arc<AtomicU64>,
}

impl Freezer {
    pub fn spawn(interval_ms: u64) -> Self {
        let state = Arc::new(Mutex::new(FreezeState::default()));
        let interval = Arc::new(AtomicU64::new(interval_ms));
        let (s, i) = (state.clone(), interval.clone());
        std::thread::Builder::new()
            .name("freezer".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(i.load(Ordering::Relaxed).max(10)));
                    let (process, items) = match s.lock() {
                        Ok(st) if !st.items.is_empty() => (st.process.clone(), st.items.clone()),
                        _ => continue,
                    };
                    let Some(process) = process else { continue };
                    for (addr, bytes) in &items {
                        let _ = process.write(*addr, bytes);
                    }
                }
            })
            .expect("spawn freezer thread");
        Self {
            state,
            interval_ms: interval,
        }
    }

    pub fn set(&self, process: Option<Arc<Process>>, items: Vec<(usize, Vec<u8>)>) {
        if let Ok(mut st) = self.state.lock() {
            st.process = process;
            st.items = items;
        }
    }

    pub fn set_interval(&self, ms: u64) {
        self.interval_ms.store(ms, Ordering::Relaxed);
    }

    pub fn active(&self) -> usize {
        self.state.lock().map(|s| s.items.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrip_and_resolve() {
        let modules = vec![Module {
            name: "game.exe".into(),
            base: 0x1000,
            size: 0x1000,
        }];
        let mut entry = Entry::new("Золото".into(), "game.exe+10".into(), ValueKind::I32, 0);
        entry.frozen = true;
        entry.frozen_value = "999".into();
        entry.resolve(&modules, None);
        assert_eq!(entry.resolved, Some(0x1010));
        assert_eq!(
            entry.freeze_bytes(),
            Some((0x1010, 999i32.to_le_bytes().to_vec()))
        );

        let text = serde_json::to_string(&entry).unwrap();
        let back: Entry = serde_json::from_str(&text).unwrap();
        assert_eq!(back.address, "game.exe+10");
        assert!(back.resolved.is_none());
    }

    #[test]
    fn table_file_name_is_sanitized() {
        let p = table_path(Path::new("x"), "my game:1?.exe");
        assert_eq!(p.file_name().unwrap(), "my_game_1_.exe.json");
    }

    #[test]
    fn freezer_rewrites_value() {
        let target = Box::new(5i32);
        let addr = &*target as *const i32 as usize;
        let own = Arc::new(Process::open(std::process::id(), "self").unwrap());
        let freezer = Freezer::spawn(10);
        freezer.set(Some(own), vec![(addr, 77i32.to_le_bytes().to_vec())]);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while unsafe { std::ptr::read_volatile(addr as *const i32) } != 77 {
            assert!(std::time::Instant::now() < deadline, "freezer never wrote");
            std::thread::sleep(Duration::from_millis(5));
        }
        freezer.set(None, Vec::new());
    }
}
