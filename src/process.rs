//! Всё, что касается чужого процесса через WinAPI: список процессов, открытие,
//! регионы памяти, модули, чтение и запись, права администратора.

use std::collections::HashMap;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    LUID,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READWRITE, PAGE_WRITECOPY,
    VirtualProtectEx, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, IsWow64Process, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, SW_SHOWNORMAL,
};

/// `GetExitCodeProcess` отдаёт это, пока процесс жив.
const STILL_ACTIVE: u32 = 259;

// Приостановить процесс целиком одним вызовом умеет только ntdll: в WinAPI
// есть лишь SuspendThread для каждого потока по отдельности.
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(process: HANDLE) -> i32;
    fn NtResumeProcess(process: HANDLE) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Текст последней ошибки WinAPI на языке системы.
fn last_error() -> String {
    let code = unsafe { GetLastError() };
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

/// Закрывает хэндл, когда выходит из области видимости.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ---------- список процессов ----------

#[derive(Debug, Clone)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    /// Заголовок главного окна, если оно есть. По нему проще всего найти игру.
    pub window_title: Option<String>,
    /// `Some(true)` для 32-битного процесса. `None`, если процесс не открылся
    /// даже на чтение сведений — обычно это системные процессы.
    pub is_32bit: Option<bool>,
    pub working_set: Option<usize>,
}

pub fn list_processes() -> Vec<ProcessEntry> {
    let titles = window_titles();
    let own_pid = std::process::id();
    let mut out = Vec::new();

    unsafe {
        let snapshot = OwnedHandle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0));
        if snapshot.0 == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot.0, &mut entry) != 0 {
            loop {
                let pid = entry.th32ProcessID;
                // PID 0 — «бездействие системы», читать там нечего; себя тоже не показываем.
                if pid != 0 && pid != own_pid {
                    let (is_32bit, working_set) = query_basic_info(pid);
                    out.push(ProcessEntry {
                        pid,
                        name: from_wide(&entry.szExeFile),
                        window_title: titles.get(&pid).cloned(),
                        is_32bit,
                        working_set,
                    });
                }
                if Process32NextW(snapshot.0, &mut entry) == 0 {
                    break;
                }
            }
        }
    }
    out
}

fn query_basic_info(pid: u32) -> (Option<bool>, Option<usize>) {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return (None, None);
        }
        let handle = OwnedHandle(handle);

        let mut wow64 = 0;
        let is_32bit = (IsWow64Process(handle.0, &mut wow64) != 0).then_some(wow64 != 0);

        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let working_set = (K32GetProcessMemoryInfo(handle.0, &mut counters, counters.cb) != 0)
            .then_some(counters.WorkingSetSize);

        (is_32bit, working_set)
    }
}

/// PID → заголовок видимого окна верхнего уровня. Если окон несколько,
/// берётся самый длинный заголовок: у игр и редакторов главное окно обычно
/// подписано подробнее служебных.
fn window_titles() -> HashMap<u32, String> {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> i32 {
        unsafe {
            let map = &mut *(lparam as *mut HashMap<u32, String>);
            if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
                return 1;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return 1;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let copied = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if copied <= 0 {
                return 1;
            }
            let title = from_wide(&buf);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let slot = map.entry(pid).or_default();
            if title.chars().count() > slot.chars().count() {
                *slot = title;
            }
            1
        }
    }

    let mut map: HashMap<u32, String> = HashMap::new();
    unsafe {
        EnumWindows(Some(callback), &mut map as *mut _ as LPARAM);
    }
    map
}

// ---------- открытый процесс ----------

#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub base: usize,
    pub size: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub base: usize,
    pub size: usize,
}

/// Какие регионы памяти брать в поиск.
#[derive(Debug, Clone, Copy)]
pub struct RegionFilter {
    /// Только страницы, доступные на запись. Значения игр почти всегда там,
    /// а поиск идёт в разы быстрее.
    pub writable_only: bool,
    /// Отображённые в память файлы (`MEM_MAPPED`). Обычно это шум: шрифты,
    /// локализация, ресурсы.
    pub include_mapped: bool,
}

pub struct Process {
    handle: HANDLE,
    pub pid: u32,
    pub name: String,
    pub is_32bit: bool,
}

// Хэндл процесса — просто число, ядро само синхронизирует обращения к нему.
unsafe impl Send for Process {}
unsafe impl Sync for Process {}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Process {
    pub fn open(pid: u32, name: &str) -> Result<Self, String> {
        let rights = PROCESS_QUERY_INFORMATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE
            | PROCESS_VM_OPERATION
            | PROCESS_SUSPEND_RESUME;
        let handle = unsafe { OpenProcess(rights, 0, pid) };
        if handle.is_null() {
            let code = unsafe { GetLastError() };
            let reason = std::io::Error::from_raw_os_error(code as i32);
            let mut message = format!("Не удалось открыть {name} (PID {pid}): {reason}");
            if code == ERROR_ACCESS_DENIED && !is_elevated() {
                message.push_str(". Попробуйте запустить MemPick от имени администратора.");
            }
            return Err(message);
        }

        let mut wow64 = 0;
        let is_32bit = unsafe { IsWow64Process(handle, &mut wow64) } != 0 && wow64 != 0;
        Ok(Self {
            handle,
            pid,
            name: name.to_string(),
            is_32bit,
        })
    }

    /// Размер указателя в целевом процессе: 4 байта для 32-битного.
    pub fn pointer_size(&self) -> usize {
        if self.is_32bit { 4 } else { 8 }
    }

    /// Читает указатель с учётом разрядности процесса.
    pub fn read_pointer(&self, addr: usize) -> Option<usize> {
        let mut buf = [0u8; 8];
        let size = self.pointer_size();
        self.read_exact(addr, &mut buf[..size])
            .then(|| u64::from_le_bytes(buf) as usize)
    }

    /// Ставит все потоки процесса на паузу. `true`, если получилось.
    pub fn suspend(&self) -> bool {
        unsafe { NtSuspendProcess(self.handle) >= 0 }
    }

    pub fn resume(&self) {
        unsafe { NtResumeProcess(self.handle) };
    }

    pub fn is_alive(&self) -> bool {
        let mut code = 0;
        unsafe { GetExitCodeProcess(self.handle, &mut code) != 0 && code == STILL_ACTIVE }
    }

    /// Читает сколько получится, возвращает число прочитанных байт.
    pub fn read(&self, addr: usize, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                &mut read,
            )
        };
        if ok != 0 { buf.len() } else { read }
    }

    pub fn read_exact(&self, addr: usize, buf: &mut [u8]) -> bool {
        self.read(addr, buf) == buf.len()
    }

    /// Пишет байты. Если страница защищена от записи (код, константы), на
    /// время записи снимает защиту и возвращает её обратно.
    pub fn write(&self, addr: usize, data: &[u8]) -> Result<(), String> {
        if self.write_raw(addr, data) {
            return Ok(());
        }
        let mut old = 0u32;
        let unprotected = unsafe {
            VirtualProtectEx(
                self.handle,
                addr as *const c_void,
                data.len(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
        } != 0;
        if !unprotected {
            return Err(format!(
                "Не удалось записать по адресу {addr:#X}: {}",
                last_error()
            ));
        }
        let written = self.write_raw(addr, data);
        let error = (!written).then(last_error);
        unsafe {
            let mut ignored = 0u32;
            VirtualProtectEx(
                self.handle,
                addr as *const c_void,
                data.len(),
                old,
                &mut ignored,
            );
        }
        match error {
            None => Ok(()),
            Some(e) => Err(format!("Не удалось записать по адресу {addr:#X}: {e}")),
        }
    }

    fn write_raw(&self, addr: usize, data: &[u8]) -> bool {
        let mut written = 0usize;
        let ok = unsafe {
            WriteProcessMemory(
                self.handle,
                addr as *const c_void,
                data.as_ptr() as *const c_void,
                data.len(),
                &mut written,
            )
        };
        ok != 0 && written == data.len()
    }

    /// Загруженные модули (exe и dll), отсортированные по адресу.
    pub fn modules(&self) -> Vec<Module> {
        let mut out = Vec::new();
        unsafe {
            let snapshot = OwnedHandle(CreateToolhelp32Snapshot(
                TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32,
                self.pid,
            ));
            if snapshot.0 == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut entry: MODULEENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
            if Module32FirstW(snapshot.0, &mut entry) != 0 {
                loop {
                    out.push(Module {
                        name: from_wide(&entry.szModule),
                        base: entry.modBaseAddr as usize,
                        size: entry.modBaseSize as usize,
                    });
                    if Module32NextW(snapshot.0, &mut entry) == 0 {
                        break;
                    }
                }
            }
        }
        out.sort_by_key(|m| m.base);
        out
    }

    /// Закоммиченные регионы, которые можно читать (и, если попросили, писать).
    pub fn regions(&self, filter: RegionFilter) -> Vec<Region> {
        let (min_addr, max_addr) = address_space();
        let mut out: Vec<Region> = Vec::new();
        let mut addr = min_addr;

        while addr < max_addr {
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            let got = unsafe {
                VirtualQueryEx(
                    self.handle,
                    addr as *const c_void,
                    &mut mbi,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if got == 0 {
                break;
            }
            let base = mbi.BaseAddress as usize;
            let size = mbi.RegionSize;

            if mbi.State == MEM_COMMIT && is_wanted(&mbi, filter) {
                // Соседние регионы склеиваем: меньше чтений, и значение на стыке
                // двух регионов не теряется.
                match out.last_mut() {
                    Some(last) if last.base + last.size == base => last.size += size,
                    _ => out.push(Region { base, size }),
                }
            }

            let next = base.saturating_add(size);
            if next <= addr {
                break;
            }
            addr = next;
        }
        out
    }
}

fn is_wanted(mbi: &MEMORY_BASIC_INFORMATION, filter: RegionFilter) -> bool {
    let protect = mbi.Protect;
    if protect == 0 || protect & PAGE_NOACCESS != 0 || protect & PAGE_GUARD != 0 {
        return false;
    }
    if !filter.include_mapped && mbi.Type == MEM_MAPPED {
        return false;
    }
    if filter.writable_only {
        let writable =
            PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
        return protect & writable != 0;
    }
    true
}

fn address_space() -> (usize, usize) {
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    unsafe { GetSystemInfo(&mut info) };
    (
        info.lpMinimumApplicationAddress as usize,
        info.lpMaximumApplicationAddress as usize,
    )
}

/// Модуль, в который попадает адрес. `modules` отсортированы по базе.
pub fn module_at(modules: &[Module], addr: usize) -> Option<&Module> {
    let idx = modules.partition_point(|m| m.base <= addr);
    let module = modules.get(idx.checked_sub(1)?)?;
    (addr < module.base + module.size).then_some(module)
}

// ---------- права ----------

/// Запущен ли MemPick с повышенными правами.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let token = OwnedHandle(token);
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Включает SeDebugPrivilege. Без неё администратор не откроет часть чужих
/// процессов, с ней — откроет почти всё, кроме защищённых.
pub fn enable_debug_privilege() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            return false;
        }
        let token = OwnedHandle(token);
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if LookupPrivilegeValueW(std::ptr::null(), SE_DEBUG_NAME, &mut luid) == 0 {
            return false;
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(
            token.0,
            0,
            &privileges,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
            && GetLastError() == 0
    }
}

/// Запускает этот же exe от имени администратора. `true`, если пользователь
/// согласился в окне UAC и новый экземпляр стартовал.
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // По контракту ShellExecute всё, что больше 32, — успех.
    result as isize > 32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(base: usize, size: usize) -> Module {
        Module {
            name: format!("{base:x}"),
            base,
            size,
        }
    }

    #[test]
    fn module_lookup() {
        let modules = vec![module(0x1000, 0x100), module(0x5000, 0x1000)];
        assert!(module_at(&modules, 0x0fff).is_none());
        assert_eq!(module_at(&modules, 0x1000).unwrap().base, 0x1000);
        assert_eq!(module_at(&modules, 0x10ff).unwrap().base, 0x1000);
        assert!(module_at(&modules, 0x1100).is_none());
        assert_eq!(module_at(&modules, 0x5fff).unwrap().base, 0x5000);
        assert!(module_at(&modules, 0x6000).is_none());
    }

    #[test]
    fn reads_and_writes_own_memory() {
        let own = Process::open(std::process::id(), "self").expect("open self");
        let value = Box::new(0xDEAD_BEEF_1234_5678u64);
        let addr = &*value as *const u64 as usize;

        let mut buf = [0u8; 8];
        assert!(own.read_exact(addr, &mut buf));
        assert_eq!(u64::from_ne_bytes(buf), 0xDEAD_BEEF_1234_5678);

        own.write(addr, &42u64.to_ne_bytes()).expect("write");
        assert_eq!(unsafe { std::ptr::read_volatile(addr as *const u64) }, 42);

        assert!(own.is_alive());
        assert!(!own.modules().is_empty());
        let regions = own.regions(RegionFilter {
            writable_only: true,
            include_mapped: false,
        });
        assert!(
            regions
                .iter()
                .any(|r| addr >= r.base && addr < r.base + r.size)
        );
    }
}
