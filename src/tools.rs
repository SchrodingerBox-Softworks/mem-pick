//! Окна-инструменты по мотивам Cheat Engine: поиск указателей, горячие
//! клавиши записей, просмотр памяти, импорт и экспорт таблиц `.CT`.

use std::sync::atomic::Ordering;

use super::*;
use crate::memview::MemAction;
use crate::pointer_scan::{PHASE_COLLECT, PointerJob, PointerPath, PointerScanParams};
use crate::table::{EntryHotkey, HotkeyAction};
use crate::value::Scalar;

/// Состояние окна «Поиск указателей».
pub struct PointerWindow {
    pub open: bool,
    target: String,
    /// Тип, с которым найденная цепочка попадёт в таблицу.
    kind: ValueKind,
    max_level: usize,
    max_offset: String,
    job: Option<PointerJob>,
    paths: Vec<PointerPath>,
    info: Option<String>,
    error: Option<String>,
    rescan_target: String,
    selected: Option<usize>,
}

impl Default for PointerWindow {
    fn default() -> Self {
        Self {
            open: false,
            target: String::new(),
            kind: ValueKind::I32,
            max_level: 4,
            max_offset: "1000".into(),
            job: None,
            paths: Vec::new(),
            info: None,
            error: None,
            rescan_target: String::new(),
            selected: None,
        }
    }
}

impl PointerWindow {
    pub fn open_for(&mut self, addr: usize) {
        self.open = true;
        self.target = format_address(addr);
        self.rescan_target = self.target.clone();
        self.error = None;
    }

    pub fn reset(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel();
        }
        *self = Self {
            max_level: self.max_level,
            max_offset: std::mem::take(&mut self.max_offset),
            ..Self::default()
        };
    }
}

impl App {
    // ---------- поиск указателей ----------

    pub(super) fn poll_pointer_job(&mut self) {
        let finished = self.pointers.job.as_ref().is_some_and(|j| j.is_finished());
        if !finished {
            return;
        }
        let Some(job) = self.pointers.job.take() else {
            return;
        };
        match job.join() {
            Ok(outcome) => {
                let mut info = format!(
                    "Найдено {} {} за {:.1} с · указателей в памяти: {}",
                    group_digits(outcome.paths.len()),
                    plural(outcome.paths.len(), "цепочка", "цепочки", "цепочек"),
                    outcome.elapsed.as_secs_f32(),
                    group_digits(outcome.pointers),
                );
                if outcome.truncated {
                    info.push_str(" · остановлено по лимиту");
                }
                self.pointers.paths = outcome.paths;
                self.pointers.info = Some(info);
                self.pointers.selected = None;
            }
            Err(e) => self.pointers.error = Some(e),
        }
    }

    fn start_pointer_scan(&mut self, ctx: &egui::Context) {
        let Some(attached) = &self.attached else {
            return;
        };
        let read = |a: usize| attached.process.read_pointer(a);
        let target = match value::eval_address(&self.pointers.target, &attached.modules, &read) {
            Ok(t) => t,
            Err(e) => {
                self.pointers.error = Some(e);
                return;
            }
        };
        let max_offset = match usize::from_str_radix(
            self.pointers
                .max_offset
                .trim()
                .trim_start_matches("0x")
                .trim_start_matches("0X"),
            16,
        ) {
            Ok(v) if (4..=0x10_0000).contains(&v) => v,
            _ => {
                self.pointers.error =
                    Some("Максимальное смещение — шестнадцатеричное, от 4 до 100000".into());
                return;
            }
        };
        let waker = ctx.clone();
        self.pointers.error = None;
        self.pointers.info = None;
        self.pointers.paths.clear();
        self.pointers.job = Some(pointer_scan::start(
            attached.process.clone(),
            PointerScanParams {
                target,
                max_level: self.pointers.max_level,
                max_offset,
                max_results: 200_000,
                pause: self.settings.pause_scan,
            },
            move || waker.request_repaint(),
        ));
    }

    pub(super) fn pointer_window(&mut self, ctx: &egui::Context) {
        if !self.pointers.open {
            return;
        }
        let Some(attached) = &self.attached else {
            return;
        };
        let process = attached.process.clone();
        let modules = attached.modules.clone();
        let accent = accent_text(&ctx.global_style().visuals);

        let mut open = true;
        let mut run = false;
        let mut add: Option<usize> = None;
        let mut rescan = false;
        let running = self.pointers.job.is_some();

        egui::Window::new("Поиск указателей")
            .open(&mut open)
            .default_width(760.0)
            .default_height(560.0)
            .resizable(true)
            .show(ctx, |ui| {
                let pw = &mut self.pointers;
                ui.label(weak(
                    ui,
                    "Ищет цепочки от статического адреса модуля до значения. Такая цепочка \
                     переживает перезапуск игры, в отличие от адреса в куче.",
                ));
                ui.add_space(6.0);
                ui.add_enabled_ui(!running, |ui| {
                    egui::Grid::new("pointer_params")
                        .num_columns(2)
                        .spacing([10.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Адрес значения:");
                            ui.add(
                                egui::TextEdit::singleline(&mut pw.target)
                                    .desired_width(240.0)
                                    .font(egui::TextStyle::Monospace),
                            );
                            ui.end_row();

                            ui.label("Глубина цепочки:");
                            ui.add(egui::Slider::new(&mut pw.max_level, 1..=7))
                                .on_hover_text(
                                    "Сколько указателей подряд. 3–5 хватает почти всегда; \
                                     больше — дольше и шумнее.",
                                );
                            ui.end_row();

                            ui.label("Макс. смещение (hex):");
                            ui.add(
                                egui::TextEdit::singleline(&mut pw.max_offset)
                                    .desired_width(90.0)
                                    .font(egui::TextStyle::Monospace),
                            )
                            .on_hover_text("Насколько далеко поле может лежать от начала объекта");
                            ui.end_row();

                            ui.label("Тип значения:");
                            egui::ComboBox::from_id_salt("pointer_kind")
                                .selected_text(pw.kind.label())
                                .width(200.0)
                                .show_ui(ui, |ui| {
                                    for kind in ValueKind::ALL {
                                        if !kind.is_variable() {
                                            ui.selectable_value(&mut pw.kind, kind, kind.label());
                                        }
                                    }
                                });
                            ui.end_row();
                        });
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if let Some(job) = &pw.job {
                        let progress = &job.progress;
                        let found = progress.found.load(Ordering::Relaxed);
                        let text = if job.phase.load(Ordering::Relaxed) == PHASE_COLLECT {
                            format!(
                                "Собираю указатели… {:.0}% · {}",
                                progress.fraction() * 100.0,
                                group_digits(found as usize)
                            )
                        } else {
                            format!(
                                "Ищу цепочки: глубина {} · найдено {}",
                                job.depth.load(Ordering::Relaxed),
                                group_digits(found as usize)
                            )
                        };
                        ui.add(
                            egui::ProgressBar::new(progress.fraction())
                                .desired_width((ui.available_width() - 80.0).max(120.0))
                                .fill(ACCENT)
                                .text(text)
                                .animate(true),
                        );
                        if ui.button("Стоп").clicked() {
                            job.cancel();
                        }
                    } else if primary_button(ui, true, "Искать").clicked() {
                        run = true;
                    }
                });
                if let Some(err) = &pw.error {
                    ui.colored_label(DANGER, err);
                }
                if let Some(info) = &pw.info {
                    ui.label(weak(ui, info).small());
                }

                if pw.paths.is_empty() {
                    return;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("После перезапуска игры:");
                    ui.add(
                        egui::TextEdit::singleline(&mut pw.rescan_target)
                            .desired_width(180.0)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("новый адрес значения"),
                    );
                    if ui
                        .button("Оставить подходящие")
                        .on_hover_text(
                            "Найдите значение заново обычным поиском и введите его новый адрес: \
                             останутся только цепочки, которые к нему ведут.",
                        )
                        .clicked()
                    {
                        rescan = true;
                    }
                });
                ui.add_space(4.0);

                ui.style_mut().interaction.selectable_labels = false;
                let selected = pw.selected;
                let kind = pw.kind;
                let paths = &pw.paths;
                let mut select = None;
                let row_height = ui.spacing().interact_size.y;
                TableBuilder::new(ui)
                    .id_salt("pointer_paths")
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(Column::initial(200.0).at_least(120.0).clip(true))
                    .column(Column::initial(220.0).at_least(100.0).clip(true))
                    .column(Column::initial(130.0).at_least(90.0))
                    .column(Column::remainder().at_least(80.0).clip(true))
                    .max_scroll_height(f32::INFINITY)
                    .auto_shrink([false, false])
                    .header(22.0, |mut header| {
                        for title in ["База", "Смещения", "Ведёт на", "Значение"]
                        {
                            header.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(row_height, paths.len(), |mut row| {
                            let i = row.index();
                            let path = &paths[i];
                            row.set_selected(selected == Some(i));
                            let target = path.resolve(&process, &modules);
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{}+{:X}",
                                        path.module, path.module_offset
                                    ))
                                    .monospace()
                                    .color(accent),
                                );
                            });
                            row.col(|ui| {
                                let offsets: Vec<String> = path
                                    .offsets
                                    .iter()
                                    .map(|o| {
                                        if *o < 0 {
                                            format!("-{:X}", -o)
                                        } else {
                                            format!("{o:X}")
                                        }
                                    })
                                    .collect();
                                ui.label(RichText::new(offsets.join(" → ")).monospace());
                            });
                            row.col(|ui| match target {
                                Some(t) => {
                                    ui.label(RichText::new(format_address(t)).monospace());
                                }
                                None => {
                                    ui.label(weak(ui, "не читается"));
                                }
                            });
                            row.col(|ui| {
                                let text = target
                                    .and_then(|t| read_bytes(&process, t, kind.size()))
                                    .map(|b| kind.format(&b, false))
                                    .unwrap_or_else(|| "??".into());
                                ui.label(RichText::new(text).monospace());
                            });
                            let response = row.response();
                            if response.clicked() {
                                select = Some(i);
                            }
                            if response.double_clicked() {
                                add = Some(i);
                            }
                            response
                                .on_hover_text(path.expression())
                                .context_menu(|ui| {
                                    if ui.button("Добавить в таблицу").clicked() {
                                        add = Some(i);
                                        ui.close();
                                    }
                                    if ui.button("Копировать выражение").clicked()
                                    {
                                        ui.ctx().copy_text(path.expression());
                                        ui.close();
                                    }
                                });
                        });
                    });
                if select.is_some() {
                    pw.selected = select;
                }
            });

        if run {
            self.start_pointer_scan(ctx);
        }
        if rescan {
            let read = |a: usize| process.read_pointer(a);
            match value::eval_address(&self.pointers.rescan_target, &modules, &read) {
                Ok(target) => {
                    let before = self.pointers.paths.len();
                    self.pointers.paths =
                        pointer_scan::rescan(&self.pointers.paths, &process, &modules, target);
                    self.pointers.selected = None;
                    self.pointers.info = Some(format!(
                        "Осталось {} из {}",
                        group_digits(self.pointers.paths.len()),
                        group_digits(before)
                    ));
                    self.pointers.error = None;
                }
                Err(e) => self.pointers.error = Some(e),
            }
        }
        if let Some(i) = add
            && let Some(path) = self.pointers.paths.get(i)
        {
            let expr = path.expression();
            let kind = self.pointers.kind;
            self.add_entry(expr, kind, 0, String::new());
        }
        if !open {
            if let Some(job) = &self.pointers.job {
                job.cancel();
            }
            self.pointers.open = false;
        }
    }

    // ---------- горячие клавиши ----------

    pub(super) fn handle_hotkeys(&mut self) {
        let pressed = self
            .hotkeys
            .as_ref()
            .map(|h| h.take_pressed())
            .unwrap_or_default();
        for (id, index) in pressed {
            let Some(attached) = self.attached.as_ref().filter(|a| a.alive) else {
                continue;
            };
            let process = attached.process.clone();
            let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
                continue;
            };
            let Some(hotkey) = entry.hotkeys.get(index).cloned() else {
                continue;
            };
            let name = if entry.description.is_empty() {
                entry.address.clone()
            } else {
                entry.description.clone()
            };
            let result = apply_hotkey(&process, entry, &hotkey);
            match result {
                Ok(what) => self.set_status(
                    format!("{} · {name}: {what}", hotkey.keys),
                    StatusKind::Success,
                ),
                Err(e) => {
                    self.set_status(format!("{} · {name}: {e}", hotkey.keys), StatusKind::Error)
                }
            }
            self.mark_table_dirty();
        }
    }

    pub(super) fn hotkey_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.hotkey_dialog else {
            return;
        };
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            self.hotkey_dialog = None;
            return;
        };
        let errors = self
            .hotkeys
            .as_ref()
            .and_then(|h| h.errors.get(&id).cloned())
            .unwrap_or_default();
        let available = self.hotkeys.is_some();
        let mut open = true;
        let mut changed = false;

        let entry = &mut self.entries[index];
        let title = if entry.description.is_empty() {
            entry.address.clone()
        } else {
            entry.description.clone()
        };
        egui::Window::new(format!("Горячие клавиши · {title}"))
            .id(egui::Id::new("hotkey_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                if !available {
                    ui.colored_label(
                        DANGER,
                        "Глобальные горячие клавиши недоступны в этой системе",
                    );
                    return;
                }
                let numeric = !entry.kind.is_variable();
                let mut remove = None;
                for (i, hotkey) in entry.hotkeys.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut hotkey.keys)
                                    .desired_width(120.0)
                                    .hint_text("F1"),
                            )
                            .changed();
                        egui::ComboBox::from_id_salt(("hotkey_action", i))
                            .selected_text(hotkey.action.label())
                            .width(190.0)
                            .show_ui(ui, |ui| {
                                for action in HotkeyAction::ALL {
                                    let allowed = numeric
                                        || !matches!(
                                            action,
                                            HotkeyAction::Increase | HotkeyAction::Decrease
                                        );
                                    if allowed {
                                        changed |= ui
                                            .selectable_value(
                                                &mut hotkey.action,
                                                action,
                                                action.label(),
                                            )
                                            .changed();
                                    }
                                }
                            });
                        if hotkey.action.needs_value() {
                            changed |= ui
                                .add(
                                    egui::TextEdit::singleline(&mut hotkey.value)
                                        .desired_width(80.0)
                                        .hint_text("100"),
                                )
                                .changed();
                        }
                        if ui
                            .add(egui::Button::new(weak(ui, "🗑")).frame(false))
                            .on_hover_text("Убрать клавишу")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    entry.hotkeys.remove(i);
                    changed = true;
                }
                if ui.button("Добавить клавишу").clicked() {
                    entry.hotkeys.push(EntryHotkey {
                        keys: String::new(),
                        action: HotkeyAction::ToggleFreeze,
                        value: String::new(),
                    });
                    changed = true;
                }
                for err in &errors {
                    ui.colored_label(DANGER, RichText::new(err).small());
                }
                ui.add_space(4.0);
                ui.label(
                    weak(
                        ui,
                        "Например: F1, Ctrl+F2, Alt+Shift+3, Numpad5. Клавиши работают \
                         глобально — прямо в игре, даже когда MemPick свёрнут.",
                    )
                    .small(),
                );
            });
        if changed {
            self.mark_table_dirty();
        }
        if !open {
            self.hotkey_dialog = None;
        }
    }

    // ---------- память ----------

    pub(super) fn memory_window(&mut self, ctx: &egui::Context) {
        let Some(attached) = &self.attached else {
            return;
        };
        let accent = accent_text(&ctx.global_style().visuals);
        let action = self
            .memview
            .show(ctx, &attached.process, &attached.modules, accent, WARNING);
        let modules = attached.modules.clone();
        match action {
            Some(MemAction::AddToTable(addr, kind)) => {
                let address =
                    module_relative(addr, &modules).unwrap_or_else(|| format_address(addr));
                self.add_entry(address, kind, 0, String::new());
            }
            Some(MemAction::Write(addr, kind)) => {
                let text = read_bytes(&attached.process, addr, kind.size())
                    .map(|b| kind.format(&b, false))
                    .unwrap_or_default();
                self.write_dialog = Some(WriteDialog {
                    addr,
                    kind,
                    text,
                    error: None,
                    focus: true,
                });
            }
            None => {}
        }
    }

    // ---------- таблицы Cheat Engine ----------

    pub(super) fn import_ct(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Импорт таблицы Cheat Engine")
            .add_filter("Таблица Cheat Engine", &["CT", "ct"])
            .pick_file()
        else {
            return;
        };
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                self.set_status(
                    format!("Не прочитать {}: {e}", path.display()),
                    StatusKind::Error,
                );
                return;
            }
        };
        match cheat_table::import(&text) {
            Ok(imported) => {
                let count = imported.entries.len();
                for mut entry in imported.entries {
                    if let Some(a) = &self.attached {
                        entry.resolve(&a.modules, Some(&a.process));
                    }
                    self.entries.push(entry);
                }
                self.mark_table_dirty();
                let mut text = format!("Импортировано: {}", addresses(count));
                if imported.skipped > 0 {
                    text.push_str(&format!(
                        ", пропущено {} (скрипты и группы)",
                        imported.skipped
                    ));
                }
                self.set_status(text, StatusKind::Success);
            }
            Err(e) => self.set_status(e, StatusKind::Error),
        }
    }

    pub(super) fn export_ct(&mut self) {
        let name = self
            .attached
            .as_ref()
            .map(|a| a.process.name.trim_end_matches(".exe").to_string())
            .unwrap_or_else(|| "table".into());
        let Some(path) = rfd::FileDialog::new()
            .set_title("Экспорт таблицы для Cheat Engine")
            .add_filter("Таблица Cheat Engine", &["CT"])
            .set_file_name(format!("{name}.CT"))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, cheat_table::export(&self.entries)) {
            Ok(()) => self.set_status(
                format!("Таблица сохранена: {}", path.display()),
                StatusKind::Success,
            ),
            Err(e) => self.set_status(format!("Не записать файл: {e}"), StatusKind::Error),
        }
    }
}

/// Выполняет действие горячей клавиши. Возвращает, что получилось, для
/// строки состояния.
fn apply_hotkey(
    process: &Process,
    entry: &mut Entry,
    hotkey: &EntryHotkey,
) -> Result<String, String> {
    let current = || {
        entry
            .resolved
            .and_then(|addr| read_bytes(process, addr, entry.byte_len()))
            .ok_or_else(|| "значение не читается".to_string())
    };
    match hotkey.action {
        HotkeyAction::ToggleFreeze if entry.frozen => {
            entry.frozen = false;
            Ok("разморожено".into())
        }
        HotkeyAction::ToggleFreeze | HotkeyAction::Freeze => {
            let bytes = current()?;
            entry.frozen_value = entry.kind.format(&bytes, false);
            entry.frozen = true;
            Ok(format!("заморожено на {}", entry.frozen_value))
        }
        HotkeyAction::Unfreeze => {
            entry.frozen = false;
            Ok("разморожено".into())
        }
        HotkeyAction::SetValue => {
            let value = hotkey.value.clone();
            write_entry(process, entry, &value)?;
            Ok(format!("записано {value}"))
        }
        HotkeyAction::Increase | HotkeyAction::Decrease => {
            let bytes = current()?;
            let kind = entry.kind;
            let cur = kind
                .decode(&bytes)
                .ok_or_else(|| "это не число".to_string())?;
            let by = kind.parse_number(&hotkey.value)?.value;
            let sign = if hotkey.action == HotkeyAction::Increase {
                1
            } else {
                -1
            };
            let next = match (cur, by) {
                (Scalar::Int(c), Scalar::Int(b)) => Scalar::Int(c + sign * b),
                _ => Scalar::Float(cur.as_f64() + sign as f64 * by.as_f64()),
            };
            let text = kind.format(&kind.encode_scalar(next), false);
            write_entry(process, entry, &text)?;
            Ok(format!("теперь {text}"))
        }
    }
}
