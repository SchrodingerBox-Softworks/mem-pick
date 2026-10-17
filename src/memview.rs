//! Просмотр памяти в hex, как «Memory View» в Cheat Engine: шестнадцать байт
//! в строке, ASCII справа, изменившиеся байты подсвечиваются и постепенно
//! гаснут. Снизу — что лежит по выбранному адресу во всех типах сразу.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use crate::process::{Module, Process};
use crate::value::{ValueKind, eval_address, format_address, module_relative};

const COLUMNS: usize = 16;
const ROWS: usize = 24;
const FADE: Duration = Duration::from_millis(1500);

/// Что пользователь попросил сделать из окна памяти.
pub enum MemAction {
    AddToTable(usize, ValueKind),
    Write(usize, ValueKind),
}

pub struct MemoryView {
    pub open: bool,
    address_input: String,
    base: usize,
    selected: Option<usize>,
    prev: Vec<Option<u8>>,
    prev_base: usize,
    changed_at: Vec<Option<Instant>>,
    error: Option<String>,
}

impl Default for MemoryView {
    fn default() -> Self {
        Self {
            open: false,
            address_input: String::new(),
            base: 0,
            selected: None,
            prev: Vec::new(),
            prev_base: usize::MAX,
            changed_at: vec![None; ROWS * COLUMNS],
            error: None,
        }
    }
}

impl MemoryView {
    /// Открывает окно на адресе, выделяя его.
    pub fn goto(&mut self, addr: usize) {
        self.open = true;
        self.base = addr & !(COLUMNS - 1);
        // Пара строк контекста сверху.
        self.base = self.base.saturating_sub(COLUMNS * 2);
        self.selected = Some(addr);
        self.address_input = format_address(addr);
        self.error = None;
    }

    fn read(&self, process: &Process) -> Vec<Option<u8>> {
        let mut out = vec![None; ROWS * COLUMNS];
        let mut buf = vec![0u8; ROWS * COLUMNS];
        if process.read_exact(self.base, &mut buf) {
            for (o, b) in out.iter_mut().zip(buf) {
                *o = Some(b);
            }
            return out;
        }
        // На границе выделенной памяти читаем построчно.
        for row in 0..ROWS {
            let mut line = [0u8; COLUMNS];
            if process.read_exact(self.base + row * COLUMNS, &mut line) {
                for (i, b) in line.into_iter().enumerate() {
                    out[row * COLUMNS + i] = Some(b);
                }
            }
        }
        out
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        process: &Process,
        modules: &[Module],
        accent: Color32,
        changed_color: Color32,
    ) -> Option<MemAction> {
        if !self.open {
            return None;
        }
        let bytes = self.read(process);
        let now = Instant::now();
        if self.prev_base == self.base && self.prev.len() == bytes.len() {
            for (i, (old, new)) in self.prev.iter().zip(&bytes).enumerate() {
                if old != new && old.is_some() {
                    self.changed_at[i] = Some(now);
                }
            }
        } else {
            self.changed_at = vec![None; ROWS * COLUMNS];
        }

        let mut action = None;
        let mut open = self.open;
        egui::Window::new("Память")
            .open(&mut open)
            .default_width(640.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.style_mut().interaction.selectable_labels = false;
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.address_input)
                            .desired_width(260.0)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Адрес или game.exe+1A2B"),
                    );
                    let go = ui.button("Перейти").clicked()
                        || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go {
                        let read = |a: usize| process.read_pointer(a);
                        match eval_address(&self.address_input, modules, &read) {
                            Ok(addr) => self.goto(addr),
                            Err(e) => self.error = Some(e),
                        }
                    }
                    if ui
                        .button("Выше")
                        .on_hover_text("На 256 байт выше")
                        .clicked()
                    {
                        self.base = self.base.saturating_sub(0x100);
                    }
                    if ui
                        .button("Ниже")
                        .on_hover_text("На 256 байт ниже")
                        .clicked()
                    {
                        self.base = self.base.saturating_add(0x100);
                    }
                });
                if let Some(err) = &self.error {
                    ui.colored_label(Color32::from_rgb(0xe5, 0x73, 0x73), err);
                }
                if let Some(rel) = module_relative(self.base, modules) {
                    ui.label(RichText::new(rel).small().monospace().color(accent));
                }
                ui.add_space(4.0);

                let grid = ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 1.0);
                    for row in 0..ROWS {
                        ui.horizontal(|ui| {
                            let row_addr = self.base + row * COLUMNS;
                            ui.label(
                                RichText::new(format_address(row_addr))
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            ui.add_space(6.0);
                            for col in 0..COLUMNS {
                                if col == 8 {
                                    ui.add_space(4.0);
                                }
                                let i = row * COLUMNS + col;
                                let addr = row_addr + col;
                                let text = match bytes[i] {
                                    Some(b) => format!("{b:02X}"),
                                    None => "??".into(),
                                };
                                let mut rich = RichText::new(text).monospace();
                                if bytes[i].is_none() {
                                    rich = rich.color(ui.visuals().weak_text_color());
                                } else if let Some(at) = self.changed_at[i] {
                                    let left =
                                        1.0 - at.elapsed().as_secs_f32() / FADE.as_secs_f32();
                                    if left > 0.0 {
                                        rich = rich.color(changed_color).strong();
                                    }
                                }
                                if self.selected == Some(addr) {
                                    rich = rich.background_color(accent.linear_multiply(0.45));
                                }
                                let response =
                                    ui.add(egui::Label::new(rich).sense(egui::Sense::click()));
                                if response.clicked() {
                                    self.selected = Some(addr);
                                }
                            }
                            ui.add_space(8.0);
                            let ascii: String = (0..COLUMNS)
                                .map(|c| match bytes[row * COLUMNS + c] {
                                    Some(b) if (0x20..0x7f).contains(&b) => b as char,
                                    Some(_) => '.',
                                    None => ' ',
                                })
                                .collect();
                            ui.label(
                                RichText::new(ascii)
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        });
                    }
                });
                // Колесо мыши над сеткой листает по строкам.
                if grid.response.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll.abs() > 0.5 {
                        let lines = (scroll / 18.0).round() as isize;
                        let delta = lines.unsigned_abs() * COLUMNS;
                        self.base = if lines > 0 {
                            self.base.saturating_sub(delta)
                        } else {
                            self.base.saturating_add(delta)
                        };
                    }
                }

                let Some(sel) = self.selected else {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Нажмите на байт, чтобы увидеть, что по этому адресу.")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    return;
                };
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("По адресу");
                    ui.label(RichText::new(format_address(sel)).monospace());
                    if let Some(rel) = module_relative(sel, modules) {
                        ui.label(RichText::new(rel).monospace().small().color(accent));
                    }
                });
                let mut raw = [0u8; 8];
                let readable = process.read(sel, &mut raw);
                egui::Grid::new("inspector")
                    .num_columns(4)
                    .spacing([14.0, 2.0])
                    .show(ui, |ui| {
                        for kind in [
                            ValueKind::I8,
                            ValueKind::I16,
                            ValueKind::I32,
                            ValueKind::I64,
                            ValueKind::F32,
                            ValueKind::F64,
                        ] {
                            ui.label(
                                RichText::new(kind.short())
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            let text = if readable >= kind.size() {
                                kind.format(&raw[..kind.size()], false)
                            } else {
                                "??".into()
                            };
                            ui.label(RichText::new(text).monospace());
                            if ui.small_button("В таблицу").clicked() {
                                action = Some(MemAction::AddToTable(sel, kind));
                            }
                            if ui.small_button("Изменить…").clicked() {
                                action = Some(MemAction::Write(sel, kind));
                            }
                            ui.end_row();
                        }
                    });
            });
        self.open = open;
        self.prev = bytes;
        self.prev_base = self.base;
        if self.open && self.changed_at.iter().flatten().any(|t| t.elapsed() < FADE) {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        action
    }
}
