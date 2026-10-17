//! Глобальные горячие клавиши для записей таблицы адресов: нажал F1 прямо
//! в игре — значение заморозилось или выросло на сотню. Работают, даже
//! когда окно MemPick свёрнуто.

use eframe::egui;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use crate::table::Entry;

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
    /// id комбинации → (id записи, номер клавиши у записи).
    bindings: HashMap<u32, (u64, usize)>,
    pressed: Arc<Mutex<Vec<u32>>>,
    /// Что не удалось зарегистрировать, по записям: id записи → ошибки.
    pub errors: HashMap<u64, Vec<String>>,
    /// Отпечаток того, что сейчас зарегистрировано, чтобы не дёргать WinAPI
    /// на каждом кадре.
    signature: Vec<(u64, String)>,
}

impl Hotkeys {
    pub fn new(ctx: egui::Context) -> Option<Self> {
        let manager = GlobalHotKeyManager::new().ok()?;
        let pressed = Arc::new(Mutex::new(Vec::new()));
        let queue = pressed.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            // Только нажатие: на отпускание сработало бы второй раз.
            if event.state == HotKeyState::Pressed {
                if let Ok(mut q) = queue.lock() {
                    q.push(event.id);
                }
                ctx.request_repaint();
            }
        }));
        Some(Self {
            manager,
            registered: Vec::new(),
            bindings: HashMap::new(),
            pressed,
            errors: HashMap::new(),
            signature: Vec::new(),
        })
    }

    /// Перерегистрирует клавиши, если в таблице что-то поменялось.
    pub fn sync(&mut self, entries: &[Entry]) {
        let signature: Vec<(u64, String)> = entries
            .iter()
            .flat_map(|e| e.hotkeys.iter().map(move |h| (e.id, h.keys.clone())))
            .collect();
        if signature == self.signature {
            return;
        }
        self.signature = signature;

        for hotkey in self.registered.drain(..) {
            let _ = self.manager.unregister(hotkey);
        }
        self.bindings.clear();
        self.errors.clear();

        for entry in entries {
            for (i, binding) in entry.hotkeys.iter().enumerate() {
                let keys = binding.keys.trim();
                if keys.is_empty() {
                    continue;
                }
                let error = match HotKey::from_str(keys) {
                    Err(e) => Some(format!("«{keys}» не разобрать: {e}")),
                    Ok(hotkey) if self.bindings.contains_key(&hotkey.id()) => {
                        Some(format!("«{keys}» уже занята другой записью"))
                    }
                    Ok(hotkey) => match self.manager.register(hotkey) {
                        Ok(()) => {
                            self.registered.push(hotkey);
                            self.bindings.insert(hotkey.id(), (entry.id, i));
                            None
                        }
                        Err(e) => Some(format!("«{keys}» занята другой программой: {e}")),
                    },
                };
                if let Some(error) = error {
                    self.errors.entry(entry.id).or_default().push(error);
                }
            }
        }
    }

    /// Снимает все клавиши — например, при отключении от процесса.
    pub fn clear(&mut self) {
        self.sync(&[]);
    }

    /// Нажатия с прошлого кадра: (id записи, номер клавиши у записи).
    pub fn take_pressed(&self) -> Vec<(u64, usize)> {
        let ids: Vec<u32> = self
            .pressed
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        ids.iter()
            .filter_map(|id| self.bindings.get(id).copied())
            .collect()
    }
}
