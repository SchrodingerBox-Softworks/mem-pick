//! Типы значений, которые умеет искать MemPick: разбор того, что ввёл
//! пользователь, кодирование в байты и обратно, форматирование адресов.

use serde::{Deserialize, Serialize};

use crate::process::{Module, module_at};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Utf8,
    Utf16,
    /// Массив байт (AOB). При поиске можно ставить `??` вместо любого байта.
    Bytes,
}

impl ValueKind {
    /// Порядок для выпадающего списка: сначала то, что ищут чаще всего.
    pub const ALL: [ValueKind; 13] = [
        Self::I32,
        Self::F32,
        Self::F64,
        Self::I64,
        Self::I16,
        Self::I8,
        Self::U32,
        Self::U64,
        Self::U16,
        Self::U8,
        Self::Utf8,
        Self::Utf16,
        Self::Bytes,
    ];

    /// Размер одного значения. Для строк — размер одного символа: полная
    /// длина строки хранится отдельно, рядом с адресом.
    pub fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 | Self::Utf8 | Self::Bytes => 1,
            Self::I16 | Self::U16 | Self::Utf16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }

    /// Значение переменной длины: текст или массив байт. Длина хранится
    /// отдельно, рядом с адресом.
    pub fn is_variable(self) -> bool {
        matches!(self, Self::Utf8 | Self::Utf16 | Self::Bytes)
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    /// Шаг «быстрого» поиска. Компиляторы выравнивают поля структур, поэтому
    /// значения почти всегда лежат по кратным адресам; 8-байтовые часто
    /// выровнены только на 4 (32-битные игры), так что дальше 4 не идём.
    pub fn alignment(self) -> usize {
        self.size().min(4)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::I8 => "Целое, 1 байт",
            Self::I16 => "Целое, 2 байта",
            Self::I32 => "Целое, 4 байта",
            Self::I64 => "Целое, 8 байт",
            Self::U8 => "Без знака, 1 байт",
            Self::U16 => "Без знака, 2 байта",
            Self::U32 => "Без знака, 4 байта",
            Self::U64 => "Без знака, 8 байт",
            Self::F32 => "Дробное (float)",
            Self::F64 => "Дробное (double)",
            Self::Utf8 => "Текст UTF-8",
            Self::Utf16 => "Текст UTF-16",
            Self::Bytes => "Массив байт (AOB)",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::F32 => "float",
            Self::F64 => "double",
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Bytes => "aob",
        }
    }

    /// Беззнаковый тип той же ширины — запасной вариант автопоиска, когда
    /// число не влезает в знаковый.
    pub fn unsigned_twin(self) -> Option<ValueKind> {
        match self {
            Self::I8 => Some(Self::U8),
            Self::I16 => Some(Self::U16),
            Self::I32 => Some(Self::U32),
            Self::I64 => Some(Self::U64),
            _ => None,
        }
    }

    fn int_range(self) -> (i128, i128) {
        match self {
            Self::I8 => (i8::MIN as i128, i8::MAX as i128),
            Self::I16 => (i16::MIN as i128, i16::MAX as i128),
            Self::I32 => (i32::MIN as i128, i32::MAX as i128),
            Self::I64 => (i64::MIN as i128, i64::MAX as i128),
            Self::U8 => (0, u8::MAX as i128),
            Self::U16 => (0, u16::MAX as i128),
            Self::U32 => (0, u32::MAX as i128),
            Self::U64 => (0, u64::MAX as i128),
            _ => (0, 0),
        }
    }

    /// Числовое значение из байт. Для строк — `None`.
    pub fn decode(self, b: &[u8]) -> Option<Scalar> {
        if b.len() < self.size() {
            return None;
        }
        Some(match self {
            Self::I8 => Scalar::Int(b[0] as i8 as i128),
            Self::U8 => Scalar::Int(b[0] as i128),
            Self::I16 => Scalar::Int(i16::from_le_bytes([b[0], b[1]]) as i128),
            Self::U16 => Scalar::Int(u16::from_le_bytes([b[0], b[1]]) as i128),
            Self::I32 => Scalar::Int(i32::from_le_bytes(b[..4].try_into().ok()?) as i128),
            Self::U32 => Scalar::Int(u32::from_le_bytes(b[..4].try_into().ok()?) as i128),
            Self::I64 => Scalar::Int(i64::from_le_bytes(b[..8].try_into().ok()?) as i128),
            Self::U64 => Scalar::Int(u64::from_le_bytes(b[..8].try_into().ok()?) as i128),
            Self::F32 => Scalar::Float(f32::from_le_bytes(b[..4].try_into().ok()?) as f64),
            Self::F64 => Scalar::Float(f64::from_le_bytes(b[..8].try_into().ok()?)),
            Self::Utf8 | Self::Utf16 | Self::Bytes => return None,
        })
    }

    /// Разбирает число, введённое пользователем, с проверкой диапазона.
    pub fn parse_number(self, text: &str) -> Result<Number, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("Введите значение".into());
        }
        if self.is_float() {
            let normalized = text.replace(',', ".");
            let value: f64 = normalized
                .parse()
                .map_err(|_| format!("«{text}» — не число"))?;
            // Округлённое сравнение, как в Cheat Engine: «87» найдёт и 87.3,
            // которое игра показывает целым числом.
            let decimals = normalized
                .split_once('.')
                .map(|(_, frac)| frac.chars().take_while(char::is_ascii_digit).count())
                .unwrap_or(0);
            let tolerance = 0.5 * 10f64.powi(-(decimals as i32));
            return Ok(Number {
                value: Scalar::Float(value),
                tolerance,
            });
        }
        if self.is_variable() {
            return Err("Это не числовой тип".into());
        }
        let value = parse_int(text).ok_or_else(|| format!("«{text}» — не целое число"))?;
        let (min, max) = self.int_range();
        if value < min || value > max {
            return Err(format!(
                "{value} не помещается в {} ({min}…{max})",
                self.short()
            ));
        }
        Ok(Number {
            value: Scalar::Int(value),
            tolerance: 0.0,
        })
    }

    /// Байты, которые надо записать в память, чтобы там оказалось `text`.
    pub fn encode(self, text: &str) -> Result<Vec<u8>, String> {
        match self {
            Self::Utf8 => {
                if text.is_empty() {
                    return Err("Введите текст".into());
                }
                Ok(text.as_bytes().to_vec())
            }
            Self::Utf16 => {
                if text.is_empty() {
                    return Err("Введите текст".into());
                }
                Ok(text.encode_utf16().flat_map(u16::to_le_bytes).collect())
            }
            Self::Bytes => {
                let (bytes, mask) = parse_pattern(text)?;
                if mask.iter().any(|known| !known) {
                    return Err("Для записи маски «??» не подходят — нужны все байты".into());
                }
                Ok(bytes)
            }
            _ => {
                let number = self.parse_number(text)?;
                Ok(self.encode_scalar(number.value))
            }
        }
    }

    pub fn encode_scalar(self, value: Scalar) -> Vec<u8> {
        let int = || match value {
            Scalar::Int(v) => v,
            Scalar::Float(f) => f.round() as i128,
        };
        let float = || match value {
            Scalar::Int(v) => v as f64,
            Scalar::Float(f) => f,
        };
        match self {
            Self::I8 | Self::U8 => vec![int() as u8],
            Self::I16 | Self::U16 => (int() as u16).to_le_bytes().to_vec(),
            Self::I32 | Self::U32 => (int() as u32).to_le_bytes().to_vec(),
            Self::I64 | Self::U64 => (int() as u64).to_le_bytes().to_vec(),
            Self::F32 => (float() as f32).to_le_bytes().to_vec(),
            Self::F64 => float().to_le_bytes().to_vec(),
            Self::Utf8 | Self::Utf16 | Self::Bytes => Vec::new(),
        }
    }

    /// Текст для таблиц. `hex` показывает целые в шестнадцатеричном виде.
    pub fn format(self, bytes: &[u8], hex: bool) -> String {
        match self {
            Self::Utf8 => printable(&String::from_utf8_lossy(bytes)),
            Self::Utf16 => {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                printable(&String::from_utf16_lossy(&units))
            }
            Self::Bytes => format_hex_bytes(bytes),
            _ => match self.decode(bytes) {
                None => "??".into(),
                Some(Scalar::Int(v)) if hex => {
                    let bits = (v as u128) & (u128::MAX >> (128 - 8 * self.size()));
                    format!("0x{bits:X}")
                }
                Some(Scalar::Int(v)) => v.to_string(),
                Some(Scalar::Float(f)) => format_float(f, self == Self::F32),
            },
        }
    }
}

/// Число вместе с допуском для сравнения (у целых допуск нулевой).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Number {
    pub value: Scalar,
    pub tolerance: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scalar {
    Int(i128),
    Float(f64),
}

impl Scalar {
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Int(v) => v as f64,
            Self::Float(f) => f,
        }
    }
}

fn parse_int(text: &str) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = digits.replace(['_', ' '], "");
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()?
    } else {
        digits.parse::<i128>().ok()?
    };
    Some(if negative { -value } else { value })
}

fn format_float(f: f64, single: bool) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e12).contains(&abs) {
        return format!("{f:e}");
    }
    // Кратчайшая запись, которая читается обратно в то же число.
    if single {
        format!("{}", f as f32)
    } else {
        format!("{f}")
    }
}

/// Управляющие символы в таблице превращаются в точки, как в hex-редакторах.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .collect()
}

/// Сжимает до 8 байт в число — так хранится предыдущее значение каждой
/// находки, без отдельного выделения памяти на каждую.
pub fn pack(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(buf)
}

pub fn unpack(raw: u64) -> [u8; 8] {
    raw.to_le_bytes()
}

// ---------- адреса ----------

/// Разбирает адрес без разыменования указателей.
#[cfg(test)]
pub fn parse_address(text: &str, modules: &[Module]) -> Result<usize, String> {
    eval_address(text, modules, &|_| None)
}

/// Вычисляет адресное выражение в синтаксисе Cheat Engine:
/// `7FF6A1B2C3D0`, `game.exe+1A2B`, `game.exe + 0x10 - 4`,
/// `[[game.exe+1234]+10]+8`. Квадратные скобки — прочитать указатель по
/// адресу внутри. Числа всегда шестнадцатеричные, как в отладчиках.
pub fn eval_address(
    text: &str,
    modules: &[Module],
    read_pointer: &dyn Fn(usize) -> Option<usize>,
) -> Result<usize, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Введите адрес".into());
    }
    let mut parser = AddressParser {
        chars: text.chars().collect(),
        pos: 0,
        modules,
        read_pointer,
    };
    let value = parser.expr()?;
    parser.skip_spaces();
    if parser.pos != parser.chars.len() {
        return Err(format!("Не понимаю адрес «{text}»"));
    }
    usize::try_from(value).map_err(|_| "Адрес вне адресного пространства".to_string())
}

struct AddressParser<'a> {
    chars: Vec<char>,
    pos: usize,
    modules: &'a [Module],
    read_pointer: &'a dyn Fn(usize) -> Option<usize>,
}

impl AddressParser<'_> {
    fn skip_spaces(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn expr(&mut self) -> Result<i128, String> {
        let mut total = self.term()?;
        loop {
            self.skip_spaces();
            match self.chars.get(self.pos) {
                Some('+') => {
                    self.pos += 1;
                    total += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    total -= self.term()?;
                }
                _ => return Ok(total),
            }
        }
    }

    fn term(&mut self) -> Result<i128, String> {
        self.skip_spaces();
        if self.chars.get(self.pos) == Some(&'[') {
            self.pos += 1;
            let inner = self.expr()?;
            self.skip_spaces();
            if self.chars.get(self.pos) != Some(&']') {
                return Err("Не хватает закрывающей «]»".into());
            }
            self.pos += 1;
            let addr = usize::try_from(inner).map_err(|_| "Отрицательный адрес".to_string())?;
            return (self.read_pointer)(addr)
                .map(|p| p as i128)
                .ok_or_else(|| format!("Указатель по адресу {addr:X} не читается"));
        }

        let start = self.pos;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| !matches!(c, '+' | '-' | '[' | ']'))
        {
            self.pos += 1;
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        // Cheat Engine пишет модули в кавычках: "game.exe"+1234.
        let token = raw.trim().trim_matches('"');
        if token.is_empty() {
            return Err("Пропущено число или имя модуля".into());
        }
        if let Some(m) = self
            .modules
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(token))
        {
            return Ok(m.base as i128);
        }
        let hex = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .unwrap_or(token);
        i128::from_str_radix(hex, 16).map_err(|_| {
            if token.contains('.') {
                format!("Модуль «{token}» не загружен в процесс")
            } else {
                format!("«{token}» — не шестнадцатеричное число")
            }
        })
    }
}

/// Есть ли в выражении разыменование указателя.
pub fn is_pointer_expr(text: &str) -> bool {
    text.contains('[')
}

/// Разбирает шаблон байт: `48 8B ?? 05`, `488B??05`. `??` или `?` — любой байт.
/// Возвращает байты и маску (true — байт известен).
pub fn parse_pattern(text: &str) -> Result<(Vec<u8>, Vec<bool>), String> {
    let compact: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if compact.is_empty() {
        return Err("Введите байты, например: 48 8B ?? 05".into());
    }
    // С пробелами — каждый кусок отдельный байт (так одиночный «?» тоже
    // маска); слитно — по два символа на байт.
    let tokens: Vec<String> = if text.split_whitespace().count() > 1 {
        text.split_whitespace().map(str::to_string).collect()
    } else {
        compact
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect()
    };
    let mut bytes = Vec::with_capacity(tokens.len());
    let mut mask = Vec::with_capacity(tokens.len());
    for token in tokens {
        if token.len() <= 2 && token.chars().all(|c| c == '?') {
            bytes.push(0);
            mask.push(false);
            continue;
        }
        if token.len() > 2 {
            return Err(format!(
                "«{token}» — по два шестнадцатеричных символа на байт"
            ));
        }
        let byte = u8::from_str_radix(&token, 16)
            .map_err(|_| format!("«{token}» — не шестнадцатеричный байт"))?;
        bytes.push(byte);
        mask.push(true);
    }
    if !mask.iter().any(|&known| known) {
        return Err("В шаблоне должен быть хотя бы один известный байт".into());
    }
    Ok((bytes, mask))
}

pub fn format_hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// `game.exe+1A2B`, если адрес внутри модуля. Такие адреса переживают
/// перезапуск игры, а адреса в куче — нет.
pub fn module_relative(addr: usize, modules: &[Module]) -> Option<String> {
    module_at(modules, addr).map(|m| format!("{}+{:X}", m.name, addr - m.base))
}

pub fn format_address(addr: usize) -> String {
    format!("{addr:012X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ints_with_range_checks() {
        assert_eq!(
            ValueKind::I32.parse_number("-5").unwrap().value,
            Scalar::Int(-5)
        );
        assert_eq!(
            ValueKind::U16.parse_number("0xFF").unwrap().value,
            Scalar::Int(255)
        );
        assert!(ValueKind::U8.parse_number("256").is_err());
        assert!(ValueKind::U32.parse_number("-1").is_err());
        assert!(ValueKind::I32.parse_number("1.5").is_err());
    }

    #[test]
    fn float_tolerance_follows_typed_decimals() {
        let n = ValueKind::F32.parse_number("87").unwrap();
        assert_eq!(n.tolerance, 0.5);
        let n = ValueKind::F32.parse_number("87,25").unwrap();
        assert_eq!(n.value, Scalar::Float(87.25));
        assert!((n.tolerance - 0.005).abs() < 1e-12);
    }

    #[test]
    fn encode_decode_roundtrip() {
        for (kind, text) in [
            (ValueKind::I8, "-7"),
            (ValueKind::U16, "65535"),
            (ValueKind::I32, "123456"),
            (ValueKind::I64, "-9000000000"),
            (ValueKind::F64, "3.5"),
        ] {
            let bytes = kind.encode(text).unwrap();
            assert_eq!(bytes.len(), kind.size());
            assert_eq!(kind.format(&bytes, false), text);
        }
        let utf16 = ValueKind::Utf16.encode("Hi").unwrap();
        assert_eq!(utf16, vec![b'H', 0, b'i', 0]);
        assert_eq!(ValueKind::Utf16.format(&utf16, false), "Hi");
    }

    #[test]
    fn hex_format_masks_to_width() {
        let bytes = ValueKind::I16.encode("-1").unwrap();
        assert_eq!(ValueKind::I16.format(&bytes, true), "0xFFFF");
    }

    #[test]
    fn pack_roundtrip() {
        let raw = pack(&[1, 2, 3]);
        assert_eq!(&unpack(raw)[..3], &[1, 2, 3]);
    }

    #[test]
    fn address_expressions() {
        let modules = vec![Module {
            name: "Game.exe".into(),
            base: 0x1400_0000,
            size: 0x10_0000,
        }];
        assert_eq!(parse_address("0x10", &modules).unwrap(), 0x10);
        assert_eq!(parse_address("ff", &modules).unwrap(), 0xff);
        assert_eq!(
            parse_address("game.exe+1A2B", &modules).unwrap(),
            0x1400_1A2B
        );
        assert_eq!(
            parse_address("Game.exe + 0x20 - 4", &modules).unwrap(),
            0x1400_001C
        );
        assert!(parse_address("other.dll+4", &modules).is_err());
        assert_eq!(
            module_relative(0x1400_1A2B, &modules).as_deref(),
            Some("Game.exe+1A2B")
        );
        assert!(module_relative(0x10, &modules).is_none());
    }

    #[test]
    fn pointer_expressions() {
        let modules = vec![Module {
            name: "game.exe".into(),
            base: 0x1000,
            size: 0x1000,
        }];
        // По game.exe+10 лежит 0x5000, по 0x5020 — 0x9000.
        let memory = |addr: usize| match addr {
            0x1010 => Some(0x5000),
            0x5020 => Some(0x9000),
            _ => None,
        };
        assert_eq!(
            eval_address("[game.exe+10]", &modules, &memory).unwrap(),
            0x5000
        );
        assert_eq!(
            eval_address("[[\"game.exe\"+10]+20]+8", &modules, &memory).unwrap(),
            0x9008
        );
        assert!(eval_address("[[game.exe+10]+24]", &modules, &memory).is_err());
        assert!(eval_address("[game.exe+10", &modules, &memory).is_err());
        assert!(parse_address("[game.exe+10]", &modules).is_err());
        assert!(is_pointer_expr("[a]+4"));
    }

    #[test]
    fn byte_patterns() {
        let (bytes, mask) = parse_pattern("48 8B ?? 05").unwrap();
        assert_eq!(bytes, vec![0x48, 0x8B, 0, 0x05]);
        assert_eq!(mask, vec![true, true, false, true]);
        let (bytes, mask) = parse_pattern("488B??05").unwrap();
        assert_eq!(bytes, vec![0x48, 0x8B, 0, 0x05]);
        assert_eq!(mask, vec![true, true, false, true]);
        assert!(parse_pattern("?? ??").is_err());
        assert!(parse_pattern("4G").is_err());
        assert!(ValueKind::Bytes.encode("48 ??").is_err());
        assert_eq!(ValueKind::Bytes.encode("de ad").unwrap(), vec![0xDE, 0xAD]);
        assert_eq!(ValueKind::Bytes.format(&[0xDE, 0xAD], false), "DE AD");
    }
}
