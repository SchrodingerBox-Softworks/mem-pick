//! Таблицы Cheat Engine (`.CT`): импорт готовых таблиц из интернета и экспорт
//! своих, чтобы ими можно было поделиться с теми, кто сидит на CE.
//!
//! Переносятся адреса, цепочки указателей, типы и описания. Скрипты
//! Auto Assembler и Lua — это код, который CE внедряет в игру; MemPick так не
//! делает, поэтому такие записи пропускаются.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::table::Entry;
use crate::value::ValueKind;

#[derive(Default)]
struct RawEntry {
    description: String,
    variable_type: String,
    address: String,
    offsets: Vec<String>,
    length: Option<usize>,
    unicode: bool,
    byte_length: Option<usize>,
    show_as_signed: Option<bool>,
}

pub struct Imported {
    pub entries: Vec<Entry>,
    /// Сколько записей пропущено (скрипты, группы без адреса, неизвестные типы).
    pub skipped: usize,
}

pub fn import(xml: &str) -> Result<Imported, String> {
    let mut reader = Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut open: Vec<RawEntry> = Vec::new();
    let mut text = String::new();
    let mut raw_entries = Vec::new();
    let mut seen_table = false;

    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("Файл не похож на таблицу Cheat Engine: {e}"))?;
        match event {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "CheatTable" {
                    seen_table = true;
                }
                if name == "CheatEntry" {
                    open.push(RawEntry::default());
                }
                stack.push(name);
                text.clear();
            }
            Event::Text(e) => {
                text.push_str(&e.decode().map_err(|e| e.to_string())?);
            }
            Event::GeneralRef(e) => {
                if let Ok(Some(c)) = e.resolve_char_ref() {
                    text.push(c);
                } else {
                    text.push(match e.decode().map_err(|e| e.to_string())?.as_ref() {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "quot" => '"',
                        "apos" => '\'',
                        _ => '?',
                    });
                }
            }
            Event::End(_) => {
                let name = stack.pop().unwrap_or_default();
                let parent = stack.last().map(String::as_str);
                if name == "CheatEntry" {
                    if let Some(entry) = open.pop() {
                        raw_entries.push(entry);
                    }
                } else if let Some(entry) = open.last_mut() {
                    let value = text.trim().to_string();
                    match (parent, name.as_str()) {
                        (Some("CheatEntry"), "Description") => entry.description = value,
                        (Some("CheatEntry"), "VariableType") => entry.variable_type = value,
                        (Some("CheatEntry"), "Address") => entry.address = value,
                        (Some("CheatEntry"), "Length") => entry.length = value.parse().ok(),
                        (Some("CheatEntry"), "Unicode") => entry.unicode = value == "1",
                        (Some("CheatEntry"), "ByteLength") => {
                            entry.byte_length = value.parse().ok()
                        }
                        (Some("CheatEntry"), "ShowAsSigned") => {
                            entry.show_as_signed = Some(value == "1")
                        }
                        (Some("Offsets"), "Offset") => entry.offsets.push(value),
                        _ => {}
                    }
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !seen_table {
        return Err("В файле нет <CheatTable> — это не таблица Cheat Engine".into());
    }

    let mut entries = Vec::new();
    let mut skipped = 0;
    for raw in raw_entries {
        match convert(raw) {
            Some(entry) => entries.push(entry),
            None => skipped += 1,
        }
    }
    Ok(Imported { entries, skipped })
}

fn convert(raw: RawEntry) -> Option<Entry> {
    if raw.address.is_empty() {
        return None;
    }
    let signed = raw.show_as_signed != Some(false);
    let pick = |s: ValueKind, u: ValueKind| if signed { s } else { u };
    let (kind, len) = match raw.variable_type.as_str() {
        "Byte" => (pick(ValueKind::I8, ValueKind::U8), 0),
        "2 Bytes" => (pick(ValueKind::I16, ValueKind::U16), 0),
        "4 Bytes" => (pick(ValueKind::I32, ValueKind::U32), 0),
        "8 Bytes" => (pick(ValueKind::I64, ValueKind::U64), 0),
        "Pointer" => (ValueKind::U64, 0),
        "Float" => (ValueKind::F32, 0),
        "Double" => (ValueKind::F64, 0),
        "String" if raw.unicode => (ValueKind::Utf16, raw.length.unwrap_or(10) * 2),
        "String" => (ValueKind::Utf8, raw.length.unwrap_or(10)),
        "Unicode String" => (ValueKind::Utf16, raw.length.unwrap_or(10) * 2),
        "Array of byte" => (ValueKind::Bytes, raw.byte_length.unwrap_or(4)),
        _ => return None,
    };

    // В .CT смещения записаны от последнего к первому: первым применяется
    // то, что стоит в файле последним.
    let mut address = raw.address.replace('"', "");
    for offset in raw.offsets.iter().rev() {
        let offset = offset.trim();
        address = if offset.starts_with('-') {
            format!("[{address}]{offset}")
        } else {
            format!("[{address}]+{offset}")
        };
    }

    let description = raw.description.trim().trim_matches('"').to_string();
    Some(Entry::new(description, address, kind, len))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Раскладывает `[[base]+a]+b` обратно на базу и смещения в порядке .CT
/// (последнее применяемое — первым). Если выражение другой формы, отдаёт
/// его целиком как адрес: Cheat Engine тоже понимает скобки в адресе.
fn split_pointer(expr: &str) -> (String, Vec<String>) {
    let mut rest = expr.trim().to_string();
    let mut offsets = Vec::new();
    loop {
        if !rest.starts_with('[') {
            break;
        }
        // Ищем скобку, парную первой.
        let mut depth = 0;
        let mut close = None;
        for (i, c) in rest.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { break };
        let tail = rest[close + 1..].trim();
        let offset = if tail.is_empty() {
            "0".to_string()
        } else if let Some(off) = tail.strip_prefix('+') {
            off.trim().to_string()
        } else if tail.starts_with('-') {
            tail.to_string()
        } else {
            break;
        };
        if offset.contains(['[', ']', '+']) {
            break;
        }
        offsets.push(offset);
        rest = rest[1..close].to_string();
    }
    (rest, offsets)
}

pub fn export(entries: &[Entry]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <CheatTable CheatEngineTableVersion=\"45\">\n  <CheatEntries>\n",
    );
    for (id, e) in entries.iter().enumerate() {
        let (var_type, signed) = match e.kind {
            ValueKind::I8 => ("Byte", true),
            ValueKind::U8 => ("Byte", false),
            ValueKind::I16 => ("2 Bytes", true),
            ValueKind::U16 => ("2 Bytes", false),
            ValueKind::I32 => ("4 Bytes", true),
            ValueKind::U32 => ("4 Bytes", false),
            ValueKind::I64 => ("8 Bytes", true),
            ValueKind::U64 => ("8 Bytes", false),
            ValueKind::F32 => ("Float", true),
            ValueKind::F64 => ("Double", true),
            ValueKind::Utf8 | ValueKind::Utf16 => ("String", false),
            ValueKind::Bytes => ("Array of byte", false),
        };
        let (base, offsets) = split_pointer(&e.address);
        out.push_str("    <CheatEntry>\n");
        out.push_str(&format!("      <ID>{id}</ID>\n"));
        out.push_str(&format!(
            "      <Description>\"{}\"</Description>\n",
            escape(&e.description)
        ));
        if !e.kind.is_float() && !e.kind.is_variable() {
            out.push_str(&format!(
                "      <ShowAsSigned>{}</ShowAsSigned>\n",
                if signed { 1 } else { 0 }
            ));
        }
        out.push_str(&format!("      <VariableType>{var_type}</VariableType>\n"));
        match e.kind {
            ValueKind::Utf8 => out.push_str(&format!(
                "      <Length>{}</Length>\n      <Unicode>0</Unicode>\n",
                e.len
            )),
            ValueKind::Utf16 => out.push_str(&format!(
                "      <Length>{}</Length>\n      <Unicode>1</Unicode>\n",
                e.len / 2
            )),
            ValueKind::Bytes => {
                out.push_str(&format!("      <ByteLength>{}</ByteLength>\n", e.len))
            }
            _ => {}
        }
        out.push_str(&format!("      <Address>{}</Address>\n", escape(&base)));
        if !offsets.is_empty() {
            out.push_str("      <Offsets>\n");
            for off in &offsets {
                out.push_str(&format!("        <Offset>{}</Offset>\n", escape(off)));
            }
            out.push_str("      </Offsets>\n");
        }
        out.push_str("    </CheatEntry>\n");
    }
    out.push_str("  </CheatEntries>\n</CheatTable>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<CheatTable CheatEngineTableVersion="45">
  <CheatEntries>
    <CheatEntry>
      <ID>1</ID>
      <Description>"Health &amp; armor"</Description>
      <VariableType>Float</VariableType>
      <Address>"game.exe"+01A2B3C</Address>
      <Offsets>
        <Offset>8</Offset>
        <Offset>10</Offset>
      </Offsets>
    </CheatEntry>
    <CheatEntry>
      <ID>2</ID>
      <Description>"Group"</Description>
      <GroupHeader>1</GroupHeader>
      <Address></Address>
      <CheatEntries>
        <CheatEntry>
          <ID>3</ID>
          <Description>"Name"</Description>
          <VariableType>String</VariableType>
          <Length>12</Length>
          <Unicode>1</Unicode>
          <Address>7FF000</Address>
        </CheatEntry>
      </CheatEntries>
    </CheatEntry>
    <CheatEntry>
      <ID>4</ID>
      <Description>"Script"</Description>
      <VariableType>Auto Assembler Script</VariableType>
      <AssemblerScript>[ENABLE]</AssemblerScript>
    </CheatEntry>
    <CheatEntry>
      <ID>5</ID>
      <Description>"Money"</Description>
      <ShowAsSigned>0</ShowAsSigned>
      <VariableType>4 Bytes</VariableType>
      <Address>game.exe+100</Address>
    </CheatEntry>
  </CheatEntries>
</CheatTable>"#;

    #[test]
    fn imports_entries_groups_and_skips_scripts() {
        let imported = import(SAMPLE).unwrap();
        // Группа без адреса и скрипт пропущены.
        assert_eq!(imported.skipped, 2);
        let e = &imported.entries;
        assert_eq!(e.len(), 3);

        let health = e
            .iter()
            .find(|x| x.description == "Health & armor")
            .unwrap();
        assert_eq!(health.kind, ValueKind::F32);
        // Последнее смещение в файле применяется первым.
        assert_eq!(health.address, "[[game.exe+01A2B3C]+10]+8");

        let name = e.iter().find(|x| x.description == "Name").unwrap();
        assert_eq!(name.kind, ValueKind::Utf16);
        assert_eq!(name.len, 24);

        let money = e.iter().find(|x| x.description == "Money").unwrap();
        assert_eq!(money.kind, ValueKind::U32);
    }

    #[test]
    fn export_roundtrips_through_import() {
        let mut entries = vec![
            Entry::new(
                "Ammo <main>".into(),
                "[[game.exe+1A2B]+10]+8".into(),
                ValueKind::I32,
                0,
            ),
            Entry::new("Code".into(), "game.exe+500".into(), ValueKind::Bytes, 6),
        ];
        entries[0].frozen = true;
        let xml = export(&entries);
        let back = import(&xml).unwrap();
        assert_eq!(back.skipped, 0);
        assert_eq!(back.entries[0].address, "[[game.exe+1A2B]+10]+8");
        assert_eq!(back.entries[0].description, "Ammo <main>");
        assert_eq!(back.entries[0].kind, ValueKind::I32);
        assert_eq!(back.entries[1].kind, ValueKind::Bytes);
        assert_eq!(back.entries[1].len, 6);
    }

    #[test]
    fn rejects_non_tables() {
        assert!(import("<html></html>").is_err());
    }
}
