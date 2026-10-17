#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("MemPick работает только под Windows: память процессов читается через WinAPI.");

use eframe::egui::{self, Color32, CornerRadius, RichText};
use egui_extras::{Column, TableBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod cheat_table;
mod hotkeys;
mod memview;
mod pointer_scan;
mod process;
mod scan;
mod table;
mod tools;
mod value;

use process::{Module, Process, ProcessEntry, RegionFilter};
use scan::{Job, Mode, Results};
use table::{Entry, Freezer};
use value::{ValueKind, format_address, is_pointer_expr, module_relative, unpack};

/// Акцент для заливок и выделения.
const ACCENT: Color32 = Color32::from_rgb(0x16, 0xa3, 0x6f);
/// Тот же акцент, но читаемый как текст на тёмном и на светлом фоне.
const ACCENT_ON_DARK: Color32 = Color32::from_rgb(0x4a, 0xde, 0xa0);
const ACCENT_ON_LIGHT: Color32 = Color32::from_rgb(0x0f, 0x7a, 0x52);
const DANGER: Color32 = Color32::from_rgb(0xe5, 0x73, 0x73);
const WARNING: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x29);
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
/// Короткое имя для интерфейса.
const APP_TITLE: &str = "MemPick";
/// Длинное имя для заголовка окна и панели задач.
const SYSTEM_TITLE: &str = "MemPick - сканер памяти процессов";
/// Подставляется в build.rs из git-тега.
const APP_VERSION: &str = env!("APP_VERSION");
const VENDOR: &str = "SchrödingerBox Softworks";
const REPO_URL: &str = "https://github.com/SchrodingerBox-Softworks/mem-pick";
const AUTHOR: &str = "Schrodinger71";
const AUTHOR_URL: &str = "https://github.com/Schrodinger71";

/// Как часто перечитывается список процессов.
const PROCESS_REFRESH: Duration = Duration::from_secs(4);
/// Как часто перечитываются модули: dll подгружаются и после запуска игры.
const MODULE_REFRESH: Duration = Duration::from_secs(3);
/// Пауза перед сохранением таблицы, чтобы не писать файл на каждую букву.
const TABLE_SAVE_DELAY: Duration = Duration::from_millis(800);
/// Сколько висит сообщение в строке состояния.
const STATUS_TTL: Duration = Duration::from_secs(8);

// ---------- конфиг на диске ----------

/// Папка, где лежит exe. Настройки и таблицы адресов хранятся рядом —
/// так программа остаётся переносимой.
fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn settings_path() -> PathBuf {
    base_dir().join("settings.json")
}

/// Читает JSON, а если файла нет — сначала записывает туда `default`.
/// Битый файл не роняет программу: берутся настройки по умолчанию.
fn load_json<T: Serialize + DeserializeOwned>(path: &Path, default: T) -> T {
    if !path.exists() {
        save_json(path, &default);
        return default;
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(default)
}

fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Ok(text) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(path, text);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppSettings {
    theme: ThemeChoice,
    /// `None` — автопоиск по нескольким типам сразу.
    scan_kind: Option<ValueKind>,
    /// Искать только по выровненным адресам: в разы быстрее и почти без
    /// потерь, поля структур всё равно выровнены.
    aligned: bool,
    writable_only: bool,
    include_mapped: bool,
    /// Добавлять в автопоиск 1- и 2-байтовые целые. Находок от них море,
    /// поэтому по умолчанию выключено.
    auto_small_ints: bool,
    max_results: usize,
    /// Как часто обновляются значения в таблицах.
    refresh_ms: u64,
    /// Как часто переписываются замороженные значения.
    freeze_ms: u64,
    hex_values: bool,
    only_windowed: bool,
    /// Сохранять таблицу адресов для каждого процесса в `tables\`.
    remember_tables: bool,
    /// Ставить процесс на паузу на время поиска, как «Pause the game while
    /// scanning» в Cheat Engine.
    pause_scan: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            scan_kind: None,
            aligned: true,
            writable_only: true,
            include_mapped: false,
            auto_small_ints: false,
            max_results: 10_000_000,
            refresh_ms: 200,
            freeze_ms: 50,
            hex_values: false,
            only_windowed: true,
            remember_tables: true,
            pause_scan: false,
        }
    }
}

/// Отдельно от `egui::ThemePreference`, чтобы формат `settings.json` не
/// зависел от serde-фич egui.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemeChoice {
    fn label(self) -> &'static str {
        match self {
            Self::Dark => "Тёмная",
            Self::Light => "Светлая",
            Self::System => "Как в системе",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
            Self::System => egui::ThemePreference::System,
        }
    }
}

// ---------- иконка ----------

fn decode_icon() -> (Vec<u8>, u32, u32) {
    let image = image::load_from_memory(ICON_PNG)
        .expect("decode embedded icon")
        .into_rgba8();
    let (width, height) = image.dimensions();
    (image.into_raw(), width, height)
}

// ---------- тема ----------

/// Оттенок акцента, который читается на фоне текущей темы.
fn accent_text(visuals: &egui::Visuals) -> Color32 {
    if visuals.dark_mode {
        ACCENT_ON_DARK
    } else {
        ACCENT_ON_LIGHT
    }
}

/// Скруглённые углы и один акцентный цвет. У тёмной темы своя палитра,
/// для светлой хватает умолчаний egui.
fn build_visuals(theme: egui::Theme, accent: Color32) -> egui::Visuals {
    let mut visuals = theme.default_visuals();

    if theme == egui::Theme::Dark {
        visuals.panel_fill = Color32::from_rgb(0x1e, 0x21, 0x28);
        visuals.window_fill = Color32::from_rgb(0x24, 0x28, 0x30);
        visuals.extreme_bg_color = Color32::from_rgb(0x17, 0x19, 0x1f);
        visuals.faint_bg_color = Color32::from_rgb(0x2a, 0x2e, 0x37);
    }
    visuals.hyperlink_color = accent_text(&visuals);
    visuals.selection.bg_fill = accent.linear_multiply(0.55);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(8);

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(8);
    }
    visuals.widgets.hovered.bg_fill = accent.linear_multiply(0.35);
    visuals.widgets.active.bg_fill = accent.linear_multiply(0.55);

    visuals
}

/// Регистрирует оформление обеих тем и переключает на выбранную.
///
/// Одного `set_visuals` мало: он пишет в стиль *текущей* темы, а eframe потом
/// подставляет системную тему с нетронутым стилем.
fn apply_theme(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_visuals_of(egui::Theme::Dark, build_visuals(egui::Theme::Dark, ACCENT));
    ctx.set_visuals_of(
        egui::Theme::Light,
        build_visuals(egui::Theme::Light, ACCENT),
    );
    ctx.set_theme(choice.preference());
    ctx.request_repaint();
}

/// Скруглённая «таблетка» с цветной точкой и текстом.
fn badge(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.linear_multiply(0.16))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Точку рисуем сами: глиф «●» в мелком шрифте egui выходит квадратиком.
                let dot = |ui: &mut egui::Ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.0, color);
                };
                let label = |ui: &mut egui::Ui| {
                    ui.add(
                        egui::Label::new(RichText::new(text).small().color(color))
                            .selectable(false),
                    );
                };
                // У правого края панели `horizontal` раскладывает справа налево —
                // тогда добавляем в обратном порядке, чтобы точка осталась слева.
                if ui.layout().prefer_right_to_left() {
                    label(ui);
                    dot(ui);
                } else {
                    dot(ui);
                    label(ui);
                }
            });
        })
        .response
}

/// Главная кнопка экрана: залита акцентом, текст белый.
fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(ACCENT)
            .min_size(egui::vec2(120.0, 28.0)),
    )
}

fn card_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
}

fn weak(ui: &egui::Ui, text: impl Into<String>) -> RichText {
    RichText::new(text).color(ui.visuals().weak_text_color())
}

// ---------- мелкие форматтеры ----------

/// `1234567` → `1 234 567`.
fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n >= KB * KB * KB {
        format!("{:.1} ГБ", n / (KB * KB * KB))
    } else if n >= KB * KB {
        format!("{:.0} МБ", n / (KB * KB))
    } else if n >= KB {
        format!("{:.0} КБ", n / KB)
    } else {
        format!("{n} Б")
    }
}

/// Русское склонение: 1 адрес, 2 адреса, 5 адресов.
fn plural<'a>(n: usize, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let (n10, n100) = (n % 10, n % 100);
    if n10 == 1 && n100 != 11 {
        one
    } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
        few
    } else {
        many
    }
}

fn addresses(n: usize) -> String {
    format!(
        "{} {}",
        group_digits(n),
        plural(n, "адрес", "адреса", "адресов")
    )
}

fn read_bytes(process: &Process, addr: usize, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len];
    process.read_exact(addr, &mut buf).then_some(buf)
}

// ---------- приложение ----------

fn main() -> eframe::Result<()> {
    let (rgba, width, height) = decode_icon();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(SYSTEM_TITLE)
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([860.0, 560.0])
            .with_icon(egui::IconData {
                rgba,
                width,
                height,
            }),
        // Размер и положение окна — рядом с exe, а не в %AppData%.
        persistence_path: Some(base_dir()),
        ..Default::default()
    };
    eframe::run_native(
        "schrodingerbox-mempick",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Success,
    Error,
}

struct Status {
    text: String,
    kind: StatusKind,
    at: Instant,
}

struct Attached {
    process: Arc<Process>,
    modules: Vec<Module>,
    modules_at: Instant,
    alive: bool,
    alive_at: Instant,
    window_title: Option<String>,
}

/// Окно «Добавить адрес вручную».
struct AddDialog {
    address: String,
    kind: ValueKind,
    len: String,
    description: String,
    error: Option<String>,
}

/// Окно «Изменить значение» для строки из результатов поиска.
struct WriteDialog {
    addr: usize,
    kind: ValueKind,
    text: String,
    error: Option<String>,
    focus: bool,
}

/// Значение в таблице адресов, которое сейчас редактируется прямо в ячейке.
struct CellEdit {
    row: usize,
    text: String,
    focus: bool,
}

struct App {
    settings: AppSettings,
    icon_texture: egui::TextureHandle,
    elevated: bool,

    processes: Vec<ProcessEntry>,
    process_job: Option<JoinHandle<Vec<ProcessEntry>>>,
    processes_at: Option<Instant>,
    process_query: String,
    selected_pid: Option<u32>,
    attached: Option<Attached>,

    value: String,
    value2: String,
    first_mode: Mode,
    next_mode: Mode,
    job: Option<Job>,
    results: Option<Arc<Results>>,
    /// Результаты до последнего отсева — для кнопки «Шаг назад».
    undo: Option<Arc<Results>>,
    scan_number: u32,
    scan_info: Option<String>,
    selected_result: Option<usize>,

    entries: Vec<Entry>,
    selected_entry: Option<usize>,
    cell_edit: Option<CellEdit>,
    table_dirty_at: Option<Instant>,
    freezer: Freezer,

    settings_open: bool,
    max_results_input: String,
    add_dialog: Option<AddDialog>,
    write_dialog: Option<WriteDialog>,
    status: Option<Status>,

    hotkeys: Option<hotkeys::Hotkeys>,
    /// Запись, для которой открыто окно горячих клавиш.
    hotkey_dialog: Option<u64>,
    memview: memview::MemoryView,
    pointers: tools::PointerWindow,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (rgba, width, height) = decode_icon();
        let color_image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        let icon_texture =
            cc.egui_ctx
                .load_texture("app_icon", color_image, egui::TextureOptions::LINEAR);

        let settings: AppSettings = load_json(&settings_path(), AppSettings::default());
        apply_theme(&cc.egui_ctx, settings.theme);

        let elevated = process::is_elevated();
        if elevated {
            process::enable_debug_privilege();
        }

        let mut app = Self {
            freezer: Freezer::spawn(settings.freeze_ms),
            max_results_input: settings.max_results.to_string(),
            settings,
            icon_texture,
            elevated,
            processes: Vec::new(),
            process_job: None,
            processes_at: None,
            process_query: String::new(),
            selected_pid: None,
            attached: None,
            value: String::new(),
            value2: String::new(),
            first_mode: Mode::Exact,
            next_mode: Mode::Exact,
            job: None,
            results: None,
            undo: None,
            scan_number: 0,
            scan_info: None,
            selected_result: None,
            entries: Vec::new(),
            selected_entry: None,
            cell_edit: None,
            table_dirty_at: None,
            settings_open: false,
            add_dialog: None,
            write_dialog: None,
            status: None,
            hotkeys: hotkeys::Hotkeys::new(cc.egui_ctx.clone()),
            hotkey_dialog: None,
            memview: memview::MemoryView::default(),
            pointers: tools::PointerWindow::default(),
        };
        app.refresh_processes(&cc.egui_ctx);
        app
    }

    fn save_settings(&self) {
        save_json(&settings_path(), &self.settings);
    }

    fn set_status(&mut self, text: impl Into<String>, kind: StatusKind) {
        self.status = Some(Status {
            text: text.into(),
            kind,
            at: Instant::now(),
        });
    }

    fn scanning(&self) -> bool {
        self.job.is_some()
    }

    fn filter(&self) -> RegionFilter {
        RegionFilter {
            writable_only: self.settings.writable_only,
            include_mapped: self.settings.include_mapped,
        }
    }

    // ---------- процессы ----------

    fn refresh_processes(&mut self, ctx: &egui::Context) {
        if self.process_job.is_some() {
            return;
        }
        let ctx = ctx.clone();
        self.process_job = Some(std::thread::spawn(move || {
            let mut list = process::list_processes();
            // Сверху — процессы с окнами: игру ищут именно среди них.
            list.sort_by_cached_key(|p| (p.window_title.is_none(), p.name.to_lowercase(), p.pid));
            ctx.request_repaint();
            list
        }));
    }

    fn attach(&mut self, pid: u32, name: &str, window_title: Option<String>) {
        self.detach();
        match Process::open(pid, name) {
            Ok(process) => {
                let process = Arc::new(process);
                let modules = process.modules();
                let mut entries = if self.settings.remember_tables {
                    table::load(&base_dir(), name)
                } else {
                    Vec::new()
                };
                for entry in &mut entries {
                    entry.resolve(&modules, Some(&process));
                }
                let restored = entries.len();
                self.entries = entries;
                self.attached = Some(Attached {
                    process,
                    modules,
                    modules_at: Instant::now(),
                    alive: true,
                    alive_at: Instant::now(),
                    window_title,
                });
                self.selected_pid = Some(pid);
                let mut text = format!("Подключено к {name} (PID {pid})");
                if restored > 0 {
                    text.push_str(&format!(
                        ". Таблица адресов восстановлена: {}",
                        addresses(restored)
                    ));
                }
                self.set_status(text, StatusKind::Success);
            }
            Err(e) => self.set_status(e, StatusKind::Error),
        }
    }

    fn detach(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel();
        }
        self.save_table_now();
        self.attached = None;
        self.entries.clear();
        self.selected_entry = None;
        self.cell_edit = None;
        self.reset_scan();
        self.freezer.set(None, Vec::new());
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.clear();
        }
        self.hotkey_dialog = None;
        self.memview.open = false;
        self.pointers.reset();
    }

    // ---------- поиск ----------

    fn reset_scan(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel();
        }
        self.results = None;
        self.undo = None;
        self.scan_number = 0;
        self.scan_info = None;
        self.selected_result = None;
    }

    fn scan_kinds(&self) -> Vec<ValueKind> {
        match self.settings.scan_kind {
            Some(kind) => vec![kind],
            None => {
                let mut kinds = vec![ValueKind::I32, ValueKind::I64];
                if self.settings.auto_small_ints {
                    kinds.extend([ValueKind::I16, ValueKind::I8]);
                }
                kinds.extend([ValueKind::F32, ValueKind::F64]);
                kinds
            }
        }
    }

    fn start_first_scan(&mut self, ctx: &egui::Context) {
        let Some(attached) = &self.attached else {
            return;
        };
        let params = scan::FirstScan {
            kinds: self.scan_kinds(),
            mode: self.first_mode,
            value: self.value.clone(),
            value2: self.value2.clone(),
            aligned: self.settings.aligned,
            filter: self.filter(),
            max_results: self.settings.max_results,
            pause: self.settings.pause_scan,
        };
        let waker = ctx.clone();
        match scan::start_first(attached.process.clone(), params, move || {
            waker.request_repaint()
        }) {
            Ok(job) => {
                self.results = None;
                self.undo = None;
                self.selected_result = None;
                self.scan_info = None;
                self.job = Some(job);
            }
            Err(e) => self.set_status(e, StatusKind::Error),
        }
    }

    fn start_next_scan(&mut self, ctx: &egui::Context) {
        let (Some(attached), Some(results)) = (&self.attached, &self.results) else {
            return;
        };
        let params = scan::NextScan {
            mode: self.next_mode,
            value: self.value.clone(),
            value2: self.value2.clone(),
            pause: self.settings.pause_scan,
        };
        let waker = ctx.clone();
        match scan::start_next(
            attached.process.clone(),
            results.clone(),
            params,
            move || waker.request_repaint(),
        ) {
            Ok(job) => self.job = Some(job),
            Err(e) => self.set_status(e, StatusKind::Error),
        }
    }

    fn finish_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let first = job.first;
        match job.join() {
            Ok(outcome) => {
                let before = self.results.as_ref().map(|r| r.count()).unwrap_or(0);
                let results = Arc::new(outcome.results);
                let secs = outcome.elapsed.as_secs_f32();
                self.scan_info = Some(match (&*results, first) {
                    (Results::Snapshot(s), _) => format!(
                        "Снимок памяти: {} за {secs:.2} с. Измените значение в программе и отсейте.",
                        format_bytes(s.bytes as u64)
                    ),
                    (_, true) => format!(
                        "Найдено {} за {secs:.2} с · просмотрено {} в {} регионах",
                        addresses(results.count()),
                        format_bytes(outcome.scanned_bytes),
                        group_digits(outcome.regions)
                    ),
                    (_, false) => format!(
                        "Осталось {} из {} · {secs:.2} с",
                        addresses(results.count()),
                        group_digits(before)
                    ),
                });
                if first {
                    self.undo = None;
                    self.scan_number = 1;
                    // После первого поиска чаще всего отсеивают по точному
                    // значению, а после снимка — по изменению.
                    self.next_mode = if matches!(&*results, Results::Snapshot(_)) {
                        Mode::Changed
                    } else {
                        Mode::Exact
                    };
                } else {
                    self.undo = self.results.take();
                    self.scan_number += 1;
                }
                self.selected_result = None;
                self.results = Some(results);
            }
            Err(e) => self.set_status(e, StatusKind::Error),
        }
    }

    fn undo_scan(&mut self) {
        if let Some(prev) = self.undo.take() {
            self.scan_info = Some(format!("Вернулись к {}", addresses(prev.count())));
            self.results = Some(prev);
            self.scan_number = self.scan_number.saturating_sub(1);
            self.selected_result = None;
        }
    }

    // ---------- таблица адресов ----------

    fn mark_table_dirty(&mut self) {
        self.table_dirty_at = Some(Instant::now());
    }

    fn save_table_now(&mut self) {
        self.table_dirty_at = None;
        if !self.settings.remember_tables {
            return;
        }
        if let Some(attached) = &self.attached {
            table::save(&base_dir(), &attached.process.name, &self.entries);
        }
    }

    fn sync_freezer(&self) {
        let process = self
            .attached
            .as_ref()
            .filter(|a| a.alive)
            .map(|a| a.process.clone());
        let items = if process.is_some() {
            self.entries
                .iter()
                .filter_map(Entry::freeze_bytes)
                .collect()
        } else {
            Vec::new()
        };
        self.freezer.set(process, items);
    }

    fn add_result_to_table(&mut self, index: usize) {
        let (Some(attached), Some(results)) = (&self.attached, &self.results) else {
            return;
        };
        let Some((kind, len, hit)) = results.get(index) else {
            return;
        };
        if self
            .entries
            .iter()
            .any(|e| e.resolved == Some(hit.addr) && e.kind == kind)
        {
            self.set_status("Этот адрес уже в таблице", StatusKind::Info);
            return;
        }
        let address = module_relative(hit.addr, &attached.modules)
            .unwrap_or_else(|| format_address(hit.addr));
        self.add_entry(address, kind, len, String::new());
    }

    /// Добавляет запись в таблицу адресов. Пустое описание заменяется на
    /// «Значение N».
    fn add_entry(&mut self, address: String, kind: ValueKind, len: usize, description: String) {
        let description = if description.trim().is_empty() {
            format!("Значение {}", self.entries.len() + 1)
        } else {
            description
        };
        let mut entry = Entry::new(description, address, kind, len);
        if let Some(attached) = &self.attached {
            entry.resolve(&attached.modules, Some(&attached.process));
        }
        self.entries.push(entry);
        self.selected_entry = Some(self.entries.len() - 1);
        self.mark_table_dirty();
        self.set_status("Адрес добавлен в таблицу", StatusKind::Success);
    }

    fn delete_entry(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
            self.cell_edit = None;
            self.selected_entry = match self.selected_entry {
                Some(s) if s == index => None,
                Some(s) if s > index => Some(s - 1),
                other => other,
            };
            self.mark_table_dirty();
        }
    }
}

/// Записывает значение по адресу записи и, если она заморожена, запоминает
/// его как новое замороженное.
fn write_entry(process: &Process, entry: &mut Entry, text: &str) -> Result<(), String> {
    let addr = entry
        .resolved
        .ok_or_else(|| format!("Адрес «{}» не найден в процессе", entry.address))?;
    let bytes = entry.kind.encode(text)?;
    process.write(addr, &bytes)?;
    if entry.kind.is_variable() {
        entry.len = bytes.len();
    }
    if entry.frozen {
        entry.frozen_value = text.trim().to_string();
    }
    Ok(())
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.process_job.as_ref().is_some_and(|j| j.is_finished()) {
            if let Some(job) = self.process_job.take() {
                self.processes = job.join().unwrap_or_default();
                self.processes_at = Some(Instant::now());
            }
        }
        if self
            .processes_at
            .is_some_and(|at| at.elapsed() > PROCESS_REFRESH)
        {
            self.refresh_processes(ctx);
        }

        if self.job.as_ref().is_some_and(|j| j.is_finished()) {
            self.finish_job();
        }

        if let Some(attached) = &mut self.attached {
            if attached.alive && attached.alive_at.elapsed() > Duration::from_millis(500) {
                attached.alive_at = Instant::now();
                attached.alive = attached.process.is_alive();
                if !attached.alive {
                    let name = attached.process.name.clone();
                    self.set_status(format!("Процесс {name} завершился"), StatusKind::Error);
                }
            }
        }
        if let Some(attached) = &mut self.attached {
            if attached.alive && attached.modules_at.elapsed() > MODULE_REFRESH {
                attached.modules_at = Instant::now();
                let modules = attached.process.modules();
                if modules.len() != attached.modules.len() {
                    attached.modules = modules;
                }
            }
        }
        // Цепочки указателей пересчитываем постоянно: игра перекладывает
        // объекты, и вчерашний адрес уже может быть мусором.
        if let Some(attached) = &self.attached
            && attached.alive
        {
            for entry in &mut self.entries {
                entry.resolve(&attached.modules, Some(&attached.process));
            }
        }
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.sync(&self.entries);
        }
        self.handle_hotkeys();
        self.poll_pointer_job();

        if self
            .table_dirty_at
            .is_some_and(|at| at.elapsed() > TABLE_SAVE_DELAY)
        {
            self.save_table_now();
        }
        self.sync_freezer();

        if ctx.input(|i| i.viewport().close_requested()) {
            self.save_table_now();
        }

        if self
            .status
            .as_ref()
            .is_some_and(|s| s.at.elapsed() > STATUS_TTL)
        {
            self.status = None;
        }

        // Живые значения в таблицах и прогресс поиска.
        if self.scanning() {
            ctx.request_repaint_after(Duration::from_millis(80));
        } else if self.attached.is_some() {
            ctx.request_repaint_after(Duration::from_millis(self.settings.refresh_ms));
        } else {
            ctx.request_repaint_after(PROCESS_REFRESH);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        self.top_bar(ui, &ctx);
        self.status_bar(ui);
        self.process_panel(ui, &ctx);

        if self.attached.is_some() {
            self.entries_panel(ui);
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ui, |ui| {
                if self.attached.is_none() {
                    self.welcome(ui);
                    return;
                }
                self.attached_header(ui);
                ui.add_space(10.0);
                self.scan_card(ui, &ctx);
                ui.add_space(10.0);
                self.results_view(ui);
            });

        self.settings_window(&ctx);
        self.add_window(&ctx);
        self.write_window(&ctx);
        self.hotkey_window(&ctx);
        self.pointer_window(&ctx);
        self.memory_window(&ctx);

        // Delete убирает выбранную строку таблицы адресов, если фокус не в поле ввода.
        if ctx.input(|i| i.key_pressed(egui::Key::Delete))
            && !ctx.egui_wants_keyboard_input()
            && let Some(index) = self.selected_entry
        {
            self.delete_entry(index);
        }
    }
}

// ---------- панели ----------

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::top("top_bar")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new((
                        self.icon_texture.id(),
                        egui::vec2(24.0, 24.0),
                    )));
                    ui.add_space(6.0);
                    ui.heading(RichText::new(APP_TITLE).strong());
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(APP_VERSION)
                            .small()
                            .monospace()
                            .color(accent_text(ui.visuals())),
                    )
                    .on_hover_text(format!("{SYSTEM_TITLE}\n{VENDOR}"));
                    ui.hyperlink_to(RichText::new(format!("by {VENDOR}")).size(13.0), REPO_URL);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Настройки").clicked() {
                            self.settings_open = !self.settings_open;
                        }
                        ui.add_space(4.0);
                        if self.elevated {
                            badge(ui, "Администратор", accent_text(ui.visuals())).on_hover_text(
                                "MemPick запущен с правами администратора и видит память почти \
                                 всех процессов.",
                            );
                        } else {
                            let response = badge(ui, "Обычные права", WARNING)
                                .interact(egui::Sense::click())
                                .on_hover_text(
                                    "Без прав администратора не откроются игры и программы, \
                                     запущенные от администратора.\nНажмите, чтобы перезапустить \
                                     MemPick от имени администратора.",
                                );
                            if response.clicked() {
                                self.save_table_now();
                                if process::relaunch_as_admin() {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                } else {
                                    self.set_status(
                                        "Перезапуск от имени администратора отменён",
                                        StatusKind::Info,
                                    );
                                }
                            }
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::symmetric(14, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    match &self.status {
                        Some(status) => {
                            let color = match status.kind {
                                StatusKind::Info => ui.visuals().text_color(),
                                StatusKind::Success => accent_text(ui.visuals()),
                                StatusKind::Error => DANGER,
                            };
                            ui.add(
                                egui::Label::new(RichText::new(&status.text).small().color(color))
                                    .truncate(),
                            );
                        }
                        None => {
                            ui.label(weak(ui, "Готово").small());
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let frozen = self.freezer.active();
                        if frozen > 0 {
                            ui.label(
                                RichText::new(format!("Заморожено: {frozen}"))
                                    .small()
                                    .color(accent_text(ui.visuals())),
                            );
                        }
                    });
                });
            });
    }

    fn process_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut attach: Option<(u32, String, Option<String>)> = None;

        egui::Panel::left("processes")
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=460.0)
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Процессы");
                    ui.label(weak(ui, self.processes.len().to_string()).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.process_job.is_some() {
                            ui.spinner();
                        } else if ui
                            .small_button("Обновить")
                            .on_hover_text("Список и так обновляется сам каждые несколько секунд")
                            .clicked()
                        {
                            self.refresh_processes(ctx);
                        }
                    });
                });
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.process_query)
                        .desired_width(f32::INFINITY)
                        .hint_text("Имя, PID или заголовок окна…"),
                );
                if ui
                    .checkbox(&mut self.settings.only_windowed, "Только с окнами")
                    .on_hover_text("Игры и программы почти всегда с окном. При поиске по имени показываются все процессы.")
                    .changed()
                {
                    self.save_settings();
                }
                ui.add_space(4.0);

                let query = self.process_query.trim().to_lowercase();
                let visible: Vec<&ProcessEntry> = self
                    .processes
                    .iter()
                    .filter(|p| {
                        if query.is_empty() {
                            !self.settings.only_windowed || p.window_title.is_some()
                        } else {
                            p.name.to_lowercase().contains(&query)
                                || p.pid.to_string().starts_with(&query)
                                || p
                                    .window_title
                                    .as_deref()
                                    .is_some_and(|t| t.to_lowercase().contains(&query))
                        }
                    })
                    .collect();

                if visible.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        let text = if self.processes.is_empty() {
                            "Загружаю список…"
                        } else {
                            "Ничего не найдено"
                        };
                        ui.label(weak(ui, text));
                    });
                    return;
                }

                let attached_pid = self.attached.as_ref().map(|a| a.process.pid);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.style_mut().interaction.selectable_labels = false;
                        for p in visible {
                            let selected = self.selected_pid == Some(p.pid);
                            let is_attached = attached_pid == Some(p.pid);
                            let response = process_card(ui, p, selected, is_attached);
                            if response.clicked() {
                                self.selected_pid = Some(p.pid);
                            }
                            if response.double_clicked() && !is_attached {
                                attach = Some((p.pid, p.name.clone(), p.window_title.clone()));
                            }
                            response.context_menu(|ui| {
                                if ui.button("Подключиться").clicked() {
                                    attach =
                                        Some((p.pid, p.name.clone(), p.window_title.clone()));
                                    ui.close();
                                }
                                if ui.button("Копировать PID").clicked() {
                                    ui.ctx().copy_text(p.pid.to_string());
                                    ui.close();
                                }
                            });
                            if selected && !is_attached {
                                ui.horizontal(|ui| {
                                    ui.add_space(6.0);
                                    if primary_button(ui, true, "Подключиться").clicked() {
                                        attach = Some((
                                            p.pid,
                                            p.name.clone(),
                                            p.window_title.clone(),
                                        ));
                                    }
                                });
                                ui.add_space(6.0);
                            }
                        }
                    });
            });

        if let Some((pid, name, title)) = attach {
            self.attach(pid, &name, title);
        }
    }

    fn entries_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("entries")
            .resizable(true)
            .default_size(240.0)
            .size_range(130.0..=600.0)
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().panel_fill)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Таблица адресов");
                    ui.label(weak(ui, self.entries.len().to_string()).small());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.entries.is_empty(), egui::Button::new("Очистить"))
                            .clicked()
                        {
                            self.entries.clear();
                            self.selected_entry = None;
                            self.cell_edit = None;
                            self.mark_table_dirty();
                        }
                        if ui
                            .add_enabled(
                                !self.entries.is_empty(),
                                egui::Button::new("Экспорт .CT…"),
                            )
                            .on_hover_text("Сохранить таблицу в формате Cheat Engine")
                            .clicked()
                        {
                            self.export_ct();
                        }
                        if ui
                            .button("Импорт .CT…")
                            .on_hover_text(
                                "Загрузить таблицу Cheat Engine: адреса, указатели, типы. \
                                 Скрипты пропускаются.",
                            )
                            .clicked()
                        {
                            self.import_ct();
                        }
                        if ui.button("Добавить адрес…").clicked() {
                            self.add_dialog = Some(AddDialog {
                                address: String::new(),
                                kind: self.settings.scan_kind.unwrap_or(ValueKind::I32),
                                len: "16".into(),
                                description: String::new(),
                                error: None,
                            });
                        }
                    });
                });
                ui.add_space(4.0);

                if self.entries.is_empty() {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(weak(
                            ui,
                            "Двойной клик по найденному адресу добавит его сюда. \
                             Здесь значение можно изменить или заморозить.",
                        ));
                    });
                    return;
                }
                self.entries_table(ui);
            });
    }

    fn entries_table(&mut self, ui: &mut egui::Ui) {
        let Some(attached) = &self.attached else {
            return;
        };
        let process = attached.process.clone();
        let alive = attached.alive;
        let hex = self.settings.hex_values;
        let row_height = ui.spacing().interact_size.y + 4.0;
        let accent = accent_text(ui.visuals());
        let selected_entry = self.selected_entry;

        let mut select = None;
        let mut delete = None;
        let mut status: Option<(String, StatusKind)> = None;
        let mut dirty = false;
        let mut open_hotkeys = None;
        let mut show_memory = None;
        let mut find_pointers = None;
        let hotkey_errors: std::collections::HashSet<u64> = self
            .hotkeys
            .as_ref()
            .map(|h| h.errors.keys().copied().collect())
            .unwrap_or_default();

        let entries = &mut self.entries;
        let cell_edit = &mut self.cell_edit;

        // Подписи в ячейках не должны перехватывать клики: строка выбирается
        // и добавляется кликом по любому месту.
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("entries_table")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(34.0))
            .column(Column::initial(210.0).at_least(80.0).clip(true))
            .column(Column::initial(190.0).at_least(90.0).clip(true))
            .column(Column::initial(80.0).at_least(50.0))
            .column(Column::initial(90.0).at_least(60.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .column(Column::exact(30.0))
            .auto_shrink([false, false])
            .header(22.0, |mut header| {
                header.col(|ui| {
                    ui.label(weak(ui, "❄"))
                        .on_hover_text("Заморозка: значение держится на месте");
                });
                for title in ["Описание", "Адрес", "Тип", "Клавиши", "Значение"]
                {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
                header.col(|_| {});
            })
            .body(|body| {
                body.rows(row_height, entries.len(), |mut row| {
                    let i = row.index();
                    let entry = &mut entries[i];
                    row.set_selected(selected_entry == Some(i));

                    let current = entry
                        .resolved
                        .filter(|_| alive)
                        .and_then(|addr| read_bytes(&process, addr, entry.byte_len()));

                    row.col(|ui| {
                        let mut frozen = entry.frozen;
                        if ui
                            .checkbox(&mut frozen, "")
                            .on_hover_text("Заморозить значение")
                            .changed()
                        {
                            if frozen {
                                match &current {
                                    Some(bytes) => {
                                        entry.frozen_value = entry.kind.format(bytes, false);
                                        entry.frozen = true;
                                    }
                                    None => {
                                        status = Some((
                                            "Нельзя заморозить: значение не читается".into(),
                                            StatusKind::Error,
                                        ))
                                    }
                                }
                            } else {
                                entry.frozen = false;
                            }
                            dirty = true;
                        }
                    });
                    row.col(|ui| {
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut entry.description)
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("Описание"),
                            )
                            .changed()
                        {
                            dirty = true;
                        }
                    });
                    row.col(|ui| {
                        let label = ui.label(RichText::new(&entry.address).monospace());
                        match entry.resolved {
                            Some(addr) => {
                                label.on_hover_text(format!("{addr:#X}"));
                                if is_pointer_expr(&entry.address) {
                                    ui.label(
                                        weak(ui, format!("→ {}", format_address(addr)))
                                            .small()
                                            .monospace(),
                                    );
                                }
                            }
                            None => {
                                ui.colored_label(DANGER, "не найден").on_hover_text(
                                    entry
                                        .error
                                        .clone()
                                        .unwrap_or_else(|| "Адрес не вычисляется".into()),
                                );
                            }
                        }
                    });
                    row.col(|ui| {
                        let kind = if entry.kind.is_variable() {
                            format!("{}[{}]", entry.kind.short(), entry.len)
                        } else {
                            entry.kind.short().to_string()
                        };
                        ui.label(weak(ui, kind));
                    });
                    let (_, keys_cell) = row.col(|ui| {
                        if entry.hotkeys.is_empty() {
                            ui.label(weak(ui, "—"));
                        } else {
                            let keys: Vec<&str> =
                                entry.hotkeys.iter().map(|h| h.keys.as_str()).collect();
                            let color = if hotkey_errors.contains(&entry.id) {
                                DANGER
                            } else {
                                accent
                            };
                            ui.label(RichText::new(keys.join(", ")).small().color(color));
                        }
                    });
                    if keys_cell
                        .on_hover_text("Горячие клавиши: нажмите, чтобы настроить")
                        .clicked()
                    {
                        open_hotkeys = Some(entry.id);
                    }
                    let (_, value_cell) = row.col(|ui| {
                        let editing = cell_edit.as_mut().filter(|e| e.row == i);
                        if let Some(edit) = editing {
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut edit.text)
                                    .desired_width(f32::INFINITY),
                            );
                            if edit.focus {
                                response.request_focus();
                                edit.focus = false;
                            }
                            if response.lost_focus() {
                                if ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                                    let text = edit.text.clone();
                                    match write_entry(&process, entry, &text) {
                                        Ok(()) => {
                                            dirty = true;
                                            status = Some((
                                                format!("Записано: {text}"),
                                                StatusKind::Success,
                                            ));
                                        }
                                        Err(e) => status = Some((e, StatusKind::Error)),
                                    }
                                }
                                *cell_edit = None;
                            }
                            return;
                        }
                        let text = match &current {
                            Some(bytes) => entry.kind.format(bytes, hex),
                            None => "??".to_string(),
                        };
                        let rich = if entry.frozen {
                            RichText::new(text).monospace().color(accent)
                        } else {
                            RichText::new(text).monospace()
                        };
                        ui.label(rich);
                    });
                    // Клик ловим на самой ячейке: она чувствует клики целиком,
                    // и вложенной подписи они не достаются.
                    let editing_this = cell_edit.as_ref().is_some_and(|e| e.row == i);
                    if !editing_this
                        && let Some(bytes) = &current
                        && value_cell
                            .on_hover_text("Нажмите, чтобы изменить")
                            .clicked()
                    {
                        *cell_edit = Some(CellEdit {
                            row: i,
                            text: entry.kind.format(bytes, false),
                            focus: true,
                        });
                    }
                    row.col(|ui| {
                        if ui
                            .add(egui::Button::new(weak(ui, "🗑")).frame(false))
                            .on_hover_text("Удалить из таблицы")
                            .clicked()
                        {
                            delete = Some(i);
                        }
                    });

                    let response = row.response();
                    if response.clicked() {
                        select = Some(i);
                    }
                    response.context_menu(|ui| {
                        if ui.button("Горячие клавиши…").clicked() {
                            open_hotkeys = Some(entry.id);
                            ui.close();
                        }
                        if let Some(addr) = entry.resolved {
                            if ui.button("Показать в памяти").clicked() {
                                show_memory = Some(addr);
                                ui.close();
                            }
                            if ui.button("Найти указатели на этот адрес…").clicked()
                            {
                                find_pointers = Some(addr);
                                ui.close();
                            }
                            if ui.button("Копировать адрес").clicked() {
                                ui.ctx().copy_text(format!("{addr:X}"));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Удалить").clicked() {
                            delete = Some(i);
                            ui.close();
                        }
                    });
                });
            });

        if let Some(i) = select {
            self.selected_entry = Some(i);
        }
        if let Some(id) = open_hotkeys {
            self.hotkey_dialog = Some(id);
        }
        if let Some(addr) = show_memory {
            self.memview.goto(addr);
        }
        if let Some(addr) = find_pointers {
            self.pointers.open_for(addr);
        }
        if dirty {
            self.mark_table_dirty();
        }
        if let Some((text, kind)) = status {
            self.set_status(text, kind);
        }
        if let Some(i) = delete {
            self.delete_entry(i);
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space((ui.available_height() * 0.12).max(10.0));
        ui.vertical_centered(|ui| {
            ui.add(egui::Image::new((
                self.icon_texture.id(),
                egui::vec2(88.0, 88.0),
            )));
            ui.add_space(10.0);
            ui.heading(RichText::new("Выберите процесс").strong());
            ui.label(weak(
                ui,
                "Двойной клик по процессу слева или кнопка «Подключиться».",
            ));
            ui.add_space(18.0);

            card_frame(ui).show(ui, |ui| {
                ui.set_max_width(520.0);
                let accent = accent_text(ui.visuals());
                for (n, title, text) in [
                    (
                        "1",
                        "Подключитесь к игре",
                        "У игр есть окно, поэтому они наверху списка.",
                    ),
                    (
                        "2",
                        "Найдите число",
                        "Введите значение, которое видите на экране, — золото, патроны, \
                         здоровье — и нажмите «Первый поиск».",
                    ),
                    (
                        "3",
                        "Отсейте лишнее",
                        "Измените значение в игре, введите новое и нажмите «Отсеять». \
                         Повторяйте, пока не останется пара адресов.",
                    ),
                    (
                        "4",
                        "Поменяйте или заморозьте",
                        "Двойной клик по адресу добавит его в таблицу внизу. Там значение \
                         можно переписать или заморозить.",
                    ),
                ] {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(n).strong().size(18.0).color(accent));
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.strong(title);
                            ui.add(egui::Label::new(weak(ui, text)).wrap());
                        });
                    });
                    ui.add_space(6.0);
                }
            });
        });
    }

    fn attached_header(&mut self, ui: &mut egui::Ui) {
        let Some(attached) = &self.attached else {
            return;
        };
        let name = attached.process.name.clone();
        let pid = attached.process.pid;
        let is_32bit = attached.process.is_32bit;
        let alive = attached.alive;
        let title = attached.window_title.clone();
        let module_count = attached.modules.len();
        let mut detach = false;

        card_frame(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.heading(RichText::new(&name).strong());
                        ui.label(weak(ui, format!("PID {pid}")).monospace());
                        badge(
                            ui,
                            if is_32bit { "x86" } else { "x64" },
                            ui.visuals().text_color(),
                        );
                        if alive {
                            badge(ui, "работает", accent_text(ui.visuals()));
                        } else {
                            badge(ui, "процесс завершён", DANGER);
                        }
                    });
                    let mut line = format!("Модулей: {module_count}");
                    if let Some(title) = &title {
                        line = format!("{title} · {line}");
                    }
                    ui.add(egui::Label::new(weak(ui, line).small()).truncate());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Отключиться").clicked() {
                        detach = true;
                    }
                });
            });
        });

        if detach {
            self.detach();
            self.set_status(format!("Отключено от {name}"), StatusKind::Info);
        }
    }

    fn scan_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let has_results = self.results.is_some();
        let running = self.scanning();
        let alive = self.attached.as_ref().is_some_and(|a| a.alive);
        let string_results = self
            .results
            .as_ref()
            .is_some_and(|r| r.kinds().iter().any(|k| k.is_variable()));

        let mut run_first = false;
        let mut run_next = false;

        card_frame(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let mode = if has_results {
                        self.next_mode
                    } else {
                        self.first_mode
                    };

                    if mode.needs_value() {
                        let hint = if mode == Mode::IncreasedBy || mode == Mode::DecreasedBy {
                            "На сколько"
                        } else if mode.needs_second_value() {
                            "От"
                        } else if self.settings.scan_kind == Some(ValueKind::Bytes) {
                            "48 8B ?? 05"
                        } else {
                            "Значение"
                        };
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.value)
                                .desired_width(170.0)
                                .hint_text(hint)
                                .font(egui::TextStyle::Monospace),
                        );
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            if has_results {
                                run_next = true;
                            } else {
                                run_first = true;
                            }
                            response.request_focus();
                        }
                        if mode.needs_second_value() {
                            ui.label("и");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.value2)
                                    .desired_width(120.0)
                                    .hint_text("До")
                                    .font(egui::TextStyle::Monospace),
                            );
                        }
                    }

                    // Тип выбирается только перед первым поиском: дальше отсев
                    // идёт по тем типам, что уже нашлись.
                    ui.add_enabled_ui(!has_results, |ui| {
                        let current = match self.settings.scan_kind {
                            Some(k) => k.label(),
                            None => "Авто: целые и дробные",
                        };
                        let before = self.settings.scan_kind;
                        egui::ComboBox::from_id_salt("scan_kind")
                            .selected_text(current)
                            .width(190.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.settings.scan_kind,
                                    None,
                                    "Авто: целые и дробные",
                                )
                                .on_hover_text(
                                    "Ищет число сразу как 4- и 8-байтовое целое, float и double",
                                );
                                ui.separator();
                                for kind in ValueKind::ALL {
                                    ui.selectable_value(
                                        &mut self.settings.scan_kind,
                                        Some(kind),
                                        kind.label(),
                                    );
                                }
                            });
                        if self.settings.scan_kind != before {
                            self.save_settings();
                        }
                    });

                    let (modes, target): (&[Mode], &mut Mode) = if has_results {
                        (&Mode::NEXT, &mut self.next_mode)
                    } else {
                        (&Mode::FIRST, &mut self.first_mode)
                    };
                    egui::ComboBox::from_id_salt(("scan_mode", has_results))
                        .selected_text(target.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for &m in modes {
                                if string_results && !m.supports_variable() {
                                    continue;
                                }
                                ui.selectable_value(target, m, m.label());
                            }
                        });
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if has_results {
                        if primary_button(ui, alive, "Отсеять")
                            .on_hover_text("Оставить только адреса, подходящие под условие (Enter)")
                            .clicked()
                        {
                            run_next = true;
                        }
                        if ui
                            .add_enabled(self.undo.is_some(), egui::Button::new("Шаг назад"))
                            .on_hover_text("Вернуть результаты до последнего отсева")
                            .clicked()
                        {
                            self.undo_scan();
                        }
                        if ui
                            .button("Новый поиск")
                            .on_hover_text("Забыть найденное и начать заново")
                            .clicked()
                        {
                            self.reset_scan();
                        }
                    } else if primary_button(ui, alive, "Первый поиск")
                        .on_hover_text("Просмотреть всю память процесса (Enter)")
                        .clicked()
                    {
                        run_first = true;
                    }

                    ui.add_space(12.0);
                    ui.add_enabled_ui(!has_results, |ui| {
                        let mut changed = false;
                        changed |= ui
                            .checkbox(&mut self.settings.aligned, "Быстрый поиск")
                            .on_hover_text(
                                "Только выровненные адреса. В разы быстрее и почти ничего не \
                                 теряет: поля структур всё равно выровнены.",
                            )
                            .changed();
                        changed |= ui
                            .checkbox(&mut self.settings.writable_only, "Только изменяемая память")
                            .on_hover_text(
                                "Значения игр почти всегда лежат в памяти, доступной на запись. \
                                 Выключите, чтобы искать ещё и в коде и константах.",
                            )
                            .changed();
                        if changed {
                            self.save_settings();
                        }
                    });
                    if ui
                        .checkbox(&mut self.settings.pause_scan, "Пауза процесса")
                        .on_hover_text(
                            "Остановить игру на время поиска, чтобы значения не менялись \
                             посреди прохода. Звук и сеть игры могут ненадолго замереть.",
                        )
                        .changed()
                    {
                        self.save_settings();
                    }
                });
            });

            ui.add_space(6.0);
            if let Some(job) = &self.job {
                let progress = &job.progress;
                let found = progress.found.load(std::sync::atomic::Ordering::Relaxed) as usize;
                let fraction = progress.fraction();
                let mut cancel = false;
                ui.horizontal(|ui| {
                    let bar_width = (ui.available_width() - 90.0).max(120.0);
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(bar_width)
                            .fill(ACCENT)
                            .text(format!(
                                "{:.0}% · найдено {}",
                                fraction * 100.0,
                                group_digits(found)
                            ))
                            .animate(true),
                    );
                    if ui.button("Стоп").clicked() {
                        cancel = true;
                    }
                });
                if cancel {
                    job.cancel();
                }
            } else if let Some(info) = &self.scan_info {
                ui.horizontal_wrapped(|ui| {
                    if self.scan_number > 0 {
                        ui.label(
                            RichText::new(format!("Поиск №{}", self.scan_number))
                                .small()
                                .strong()
                                .color(accent_text(ui.visuals())),
                        );
                    }
                    ui.label(weak(ui, info).small());
                });
                if let Some(Results::Hits {
                    truncated: true, ..
                }) = self.results.as_deref()
                {
                    ui.colored_label(
                        WARNING,
                        RichText::new(format!(
                            "Совпадений больше {} — сохранены первые. Отсейте или уточните значение.",
                            group_digits(self.settings.max_results)
                        ))
                        .small(),
                    );
                }
            }
        });

        if run_first {
            self.start_first_scan(ctx);
        } else if run_next {
            self.start_next_scan(ctx);
        }
    }

    fn results_view(&mut self, ui: &mut egui::Ui) {
        let Some(attached) = &self.attached else {
            return;
        };
        let Some(results) = self.results.clone() else {
            if !self.scanning() {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.label(weak(
                        ui,
                        "Введите значение и нажмите «Первый поиск». Не знаете точного числа \
                         (полоска здоровья, таймер)? Выберите режим «Неизвестное значение» и \
                         конкретный тип.",
                    ));
                });
            }
            return;
        };

        if let Results::Snapshot(snap) = &*results {
            card_frame(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.strong("Снимок памяти сделан");
                ui.label(weak(
                    ui,
                    format!(
                        "Запомнено {} памяти, тип {}. Теперь измените значение в программе \
                         и отсейте: «Увеличилось», «Уменьшилось» или «Изменилось». \
                         Конкретные адреса появятся после первого отсева.",
                        format_bytes(snap.bytes as u64),
                        snap.kind.label().to_lowercase()
                    ),
                ));
            });
            return;
        }

        let count = results.count();
        if count == 0 {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Ничего не найдено").strong());
                ui.label(weak(
                    ui,
                    "Проверьте тип значения, попробуйте «Авто» или нажмите «Шаг назад».",
                ));
            });
            return;
        }

        let process = attached.process.clone();
        let alive = attached.alive;
        let modules = &attached.modules;
        let hex = self.settings.hex_values;
        let selected = self.selected_result;
        let changed_color = WARNING;
        let accent = accent_text(ui.visuals());
        let row_height = ui.spacing().interact_size.y;

        let mut select = None;
        let mut add = None;
        let mut write = None;
        let mut show_memory = None;
        let mut find_pointers = None;

        ui.horizontal(|ui| {
            ui.strong("Найдено");
            ui.label(weak(ui, addresses(count)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(weak(ui, "Двойной клик — в таблицу, правый клик — действия").small());
            });
        });
        ui.add_space(2.0);

        // Подписи в ячейках не должны перехватывать клики: строка выбирается
        // и добавляется кликом по любому месту.
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("results_table")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(130.0).at_least(100.0))
            .column(Column::initial(150.0).at_least(70.0).clip(true))
            .column(Column::initial(130.0).at_least(70.0).clip(true))
            .column(Column::initial(70.0).at_least(50.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .auto_shrink([false, false])
            .header(22.0, |mut header| {
                for title in ["Адрес", "Значение", "Было", "Тип", "Где"] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, count, |mut row| {
                    let i = row.index();
                    let Some((kind, len, hit)) = results.get(i) else {
                        return;
                    };
                    row.set_selected(selected == Some(i));
                    let current = if alive {
                        read_bytes(&process, hit.addr, len)
                    } else {
                        None
                    };
                    let prev = unpack(hit.prev);
                    let prev = (len <= 8).then(|| &prev[..len]);

                    row.col(|ui| {
                        ui.label(RichText::new(format_address(hit.addr)).monospace());
                    });
                    row.col(|ui| match &current {
                        Some(bytes) => {
                            let changed = prev.is_some_and(|p| p != bytes.as_slice());
                            let text = RichText::new(kind.format(bytes, hex)).monospace();
                            ui.label(if changed {
                                text.color(changed_color)
                            } else {
                                text
                            });
                        }
                        None => {
                            ui.label(weak(ui, "??").monospace());
                        }
                    });
                    row.col(|ui| {
                        let text = prev.map(|p| kind.format(p, hex)).unwrap_or_default();
                        ui.label(weak(ui, text).monospace());
                    });
                    row.col(|ui| {
                        ui.label(weak(ui, kind.short()));
                    });
                    row.col(|ui| match module_relative(hit.addr, modules) {
                        Some(rel) => {
                            ui.label(RichText::new(rel).monospace().color(accent))
                                .on_hover_text(
                                    "Статический адрес внутри модуля — переживёт перезапуск игры",
                                );
                        }
                        None => {
                            ui.label(weak(ui, "куча")).on_hover_text(
                                "Динамическая память: после перезапуска игры адрес будет другим",
                            );
                        }
                    });

                    let response = row.response();
                    if response.clicked() {
                        select = Some(i);
                    }
                    if response.double_clicked() {
                        add = Some(i);
                    }
                    response.context_menu(|ui| {
                        if ui.button("Добавить в таблицу").clicked() {
                            add = Some(i);
                            ui.close();
                        }
                        if ui.button("Изменить значение…").clicked() {
                            write = Some(i);
                            ui.close();
                        }
                        if ui.button("Показать в памяти").clicked() {
                            show_memory = Some(hit.addr);
                            ui.close();
                        }
                        if ui.button("Найти указатели на этот адрес…").clicked()
                        {
                            find_pointers = Some(hit.addr);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Копировать адрес").clicked() {
                            ui.ctx().copy_text(format!("{:X}", hit.addr));
                            ui.close();
                        }
                        if let Some(rel) = module_relative(hit.addr, modules)
                            && ui.button(format!("Копировать {rel}")).clicked()
                        {
                            ui.ctx().copy_text(rel);
                            ui.close();
                        }
                    });
                });
            });

        if let Some(i) = select {
            self.selected_result = Some(i);
        }
        if let Some(i) = add {
            self.selected_result = Some(i);
            self.add_result_to_table(i);
        }
        if let Some(addr) = show_memory {
            self.memview.goto(addr);
        }
        if let Some(addr) = find_pointers {
            self.pointers.open_for(addr);
        }
        if let Some(i) = write
            && let Some((kind, len, hit)) = results.get(i)
        {
            let text = read_bytes(&process, hit.addr, len)
                .map(|b| kind.format(&b, false))
                .unwrap_or_default();
            self.write_dialog = Some(WriteDialog {
                addr: hit.addr,
                kind,
                text,
                error: None,
                focus: true,
            });
        }
    }
}

/// Карточка процесса в списке слева.
fn process_card(
    ui: &mut egui::Ui,
    p: &ProcessEntry,
    selected: bool,
    attached: bool,
) -> egui::Response {
    let fill = if attached {
        ACCENT.linear_multiply(0.30)
    } else if selected {
        ACCENT.linear_multiply(0.16)
    } else {
        ui.visuals().faint_bg_color
    };
    let accent = accent_text(ui.visuals());
    let width = (ui.available_width() - 4.0).max(80.0);

    let card = egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(width - 20.0);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(&p.name).strong()).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(weak(ui, p.pid.to_string()).small().monospace());
                    if attached {
                        ui.label(RichText::new("подключено").small().color(accent));
                    }
                });
            });
            if let Some(title) = &p.window_title {
                ui.add(egui::Label::new(weak(ui, title).small()).truncate());
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match p.is_32bit {
                    Some(true) => {
                        ui.label(RichText::new("x86").small().monospace().color(WARNING));
                    }
                    Some(false) => {
                        ui.label(weak(ui, "x64").small().monospace());
                    }
                    None => {
                        ui.label(RichText::new("нет доступа").small().color(DANGER))
                            .on_hover_text("Скорее всего, нужен запуск от имени администратора");
                    }
                }
                if let Some(ws) = p.working_set {
                    ui.label(weak(ui, format_bytes(ws as u64)).small());
                }
            });
        });
    ui.add_space(4.0);
    card.response.interact(egui::Sense::click())
}

// ---------- окна ----------

impl App {
    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new("Настройки")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                let mut changed = false;

                ui.label(RichText::new("Оформление").strong());
                ui.horizontal(|ui| {
                    ui.label("Тема:");
                    for choice in [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System] {
                        if ui
                            .selectable_label(self.settings.theme == choice, choice.label())
                            .clicked()
                            && self.settings.theme != choice
                        {
                            self.settings.theme = choice;
                            apply_theme(ui.ctx(), choice);
                            changed = true;
                        }
                    }
                });
                changed |= ui
                    .checkbox(&mut self.settings.hex_values, "Показывать целые в hex")
                    .changed();

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Поиск").strong());
                changed |= ui
                    .checkbox(
                        &mut self.settings.aligned,
                        "Быстрый поиск (только выровненные адреса)",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut self.settings.writable_only, "Только изменяемая память")
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.include_mapped,
                        "Искать в отображённых файлах",
                    )
                    .on_hover_text(
                        "Ресурсы, шрифты и прочие файлы, загруженные в память. Обычно это шум.",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.auto_small_ints,
                        "Автопоиск: ещё и 1- и 2-байтовые целые",
                    )
                    .on_hover_text(
                        "Находок станет в десятки раз больше, зато найдутся и маленькие счётчики.",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut self.settings.pause_scan,
                        "Ставить процесс на паузу на время поиска",
                    )
                    .changed();
                ui.horizontal(|ui| {
                    ui.label("Хранить находок не больше:");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.max_results_input)
                            .desired_width(110.0)
                            .horizontal_align(egui::Align::Center),
                    );
                    if response.lost_focus() {
                        match self.max_results_input.replace(' ', "").parse::<usize>() {
                            Ok(n) if n >= 1000 => {
                                self.settings.max_results = n.min(200_000_000);
                                changed = true;
                            }
                            _ => {}
                        }
                        self.max_results_input = self.settings.max_results.to_string();
                    }
                });
                ui.label(weak(ui, "Каждая находка — 16 байт памяти: 10 млн ≈ 160 МБ").small());

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Обновление").strong());
                ui.horizontal(|ui| {
                    ui.label("Значения в таблицах, мс:");
                    let mut ms = self.settings.refresh_ms as f32;
                    if ui
                        .add(egui::Slider::new(&mut ms, 50.0..=2000.0).step_by(50.0))
                        .changed()
                    {
                        self.settings.refresh_ms = ms as u64;
                        changed = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Заморозка, мс:");
                    let mut ms = self.settings.freeze_ms as f32;
                    if ui
                        .add(egui::Slider::new(&mut ms, 10.0..=1000.0).step_by(10.0))
                        .changed()
                    {
                        self.settings.freeze_ms = ms as u64;
                        self.freezer.set_interval(self.settings.freeze_ms);
                        changed = true;
                    }
                });

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Таблица адресов").strong());
                changed |= ui
                    .checkbox(
                        &mut self.settings.remember_tables,
                        "Запоминать для каждой программы",
                    )
                    .on_hover_text("Хранится в папке tables\\ рядом с MemPick.exe")
                    .changed();

                ui.add_space(10.0);
                ui.separator();
                ui.horizontal(|ui| {
                    let weak_color = ui.visuals().weak_text_color();
                    ui.label(
                        RichText::new(format!("{APP_TITLE} {APP_VERSION}"))
                            .size(13.0)
                            .monospace()
                            .color(weak_color),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.hyperlink_to(RichText::new(AUTHOR).size(13.0), AUTHOR_URL);
                        ui.label(RichText::new("·").size(13.0).color(weak_color));
                        ui.hyperlink_to(RichText::new(VENDOR).size(13.0), REPO_URL);
                    });
                });

                if changed {
                    self.save_settings();
                }
            });
        self.settings_open = open;
    }

    fn add_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.add_dialog else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new("Добавить адрес")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                egui::Grid::new("add_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Адрес:");
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut dialog.address)
                                .desired_width(220.0)
                                .hint_text("game.exe+1A2B или 7FF6A1B2C3D0")
                                .font(egui::TextStyle::Monospace),
                        );
                        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit = true;
                        }
                        ui.end_row();

                        ui.label("Тип:");
                        egui::ComboBox::from_id_salt("add_kind")
                            .selected_text(dialog.kind.label())
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                for kind in ValueKind::ALL {
                                    ui.selectable_value(&mut dialog.kind, kind, kind.label());
                                }
                            });
                        ui.end_row();

                        if dialog.kind.is_variable() {
                            ui.label("Длина, байт:");
                            ui.add(egui::TextEdit::singleline(&mut dialog.len).desired_width(80.0));
                            ui.end_row();
                        }

                        ui.label("Описание:");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.description)
                                .desired_width(220.0)
                                .hint_text("Золото, патроны…"),
                        );
                        ui.end_row();
                    });
                ui.label(
                    weak(
                        ui,
                        "Числа — шестнадцатеричные. Модуль + смещение, указатели в скобках: [[game.exe+10]+8]+4.",
                    )
                    .small(),
                );
                if let Some(err) = &dialog.error {
                    ui.colored_label(DANGER, err);
                }
                ui.add_space(6.0);
                if primary_button(ui, true, "Добавить").clicked() {
                    submit = true;
                }
            });

        if submit {
            let len = dialog.len.trim().parse::<usize>().unwrap_or(0);
            // Проверяем выражение сразу, чтобы опечатка не уехала в таблицу.
            let result = match &self.attached {
                Some(a) => value::eval_address(&dialog.address, &a.modules, &|addr| {
                    a.process.read_pointer(addr)
                }),
                None => Err("Нет подключения к процессу".into()),
            }
            .and_then(|addr| {
                if dialog.kind.is_variable() && !(1..=4096).contains(&len) {
                    return Err("Длина — от 1 до 4096 байт".to_string());
                }
                Ok(addr)
            });
            match result {
                Ok(_) => {
                    let address = dialog.address.trim().to_string();
                    let kind = dialog.kind;
                    let description = dialog.description.trim().to_string();
                    self.add_dialog = None;
                    self.add_entry(address, kind, len, description);
                    return;
                }
                Err(e) => dialog.error = Some(e),
            }
        }
        if !open {
            self.add_dialog = None;
        }
    }

    fn write_window(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.write_dialog else {
            return;
        };
        let mut open = true;
        let mut submit = false;
        egui::Window::new("Изменить значение")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format_address(dialog.addr)).monospace());
                    ui.label(weak(ui, dialog.kind.label()));
                });
                ui.add_space(4.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut dialog.text)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                if dialog.focus {
                    response.request_focus();
                    dialog.focus = false;
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if let Some(err) = &dialog.error {
                    ui.colored_label(DANGER, err);
                }
                ui.add_space(6.0);
                if primary_button(ui, true, "Записать").clicked() {
                    submit = true;
                }
            });

        if submit {
            let result = match &self.attached {
                Some(a) => dialog
                    .kind
                    .encode(&dialog.text)
                    .and_then(|bytes| a.process.write(dialog.addr, &bytes)),
                None => Err("Нет подключения к процессу".into()),
            };
            match result {
                Ok(()) => {
                    let text = format!("Записано: {}", dialog.text.trim());
                    self.write_dialog = None;
                    self.set_status(text, StatusKind::Success);
                    return;
                }
                Err(e) => dialog.error = Some(e),
            }
        }
        if !open {
            self.write_dialog = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals() {
        assert_eq!(addresses(1), "1 адрес");
        assert_eq!(addresses(3), "3 адреса");
        assert_eq!(addresses(11), "11 адресов");
        assert_eq!(addresses(22), "22 адреса");
        assert_eq!(addresses(1_234_567), "1 234 567 адресов");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(format_bytes(512), "512 Б");
        assert_eq!(format_bytes(3 << 20), "3 МБ");
        assert_eq!(format_bytes(3 << 30), "3.0 ГБ");
    }

    #[test]
    fn settings_tolerate_missing_fields() {
        let s: AppSettings = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(s.theme, ThemeChoice::Light);
        assert!(s.aligned);
        assert_eq!(s.max_results, 10_000_000);
    }
}
