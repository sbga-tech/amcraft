//! The localised message resources: `ResourceSet` and `ErrorResource` (`spec/libam/error.md` part I).
//!
//! A language is a pair of resource sets, `error.<language>.*` and `warning.<language>.*`, read from the
//! configured resource folder. Each file is a JSON array of items or the string table of a PE module. Items are
//! kept sorted by `(number << 16) | sub`; a later file overrides an earlier item with the same key.
//!
//! Deviations from the original, all on inputs the original handles by crashing or by undefined behaviour:
//! * a file of 64 KiB or more is not a JSON file (the original aborts the process), so a PE module of that
//!   size is read as a PE module straight away;
//! * a JSON `reset` name of more than 31 UTF-16 units is an invalid element (the original most likely maps it to
//!   the empty name, `any`);
//! * a JSON file with invalid UTF-8 outside strings or inside them is rejected as a whole, the original skips
//!   the element;
//! * a PE string without a `tag=` segment, or with a tag of more than 31 units, has the empty tag;
//! * of several language entries of one string-table block the neutral one, else the first, is read.

use std::path::PathBuf;

use crate::config::AmConfig;
use crate::error::{FATAL_MESSAGE_UNITS, FatalError};
use serde_json::Value;

use backend::Fs;

/// Hard capacity of one resource (`error.md` 1.3).
pub const ITEM_CAPACITY: usize = 222;
/// A JSON file of this many bytes or more is not read as JSON (`error.md` 5.1).
const MAX_JSON_BYTES: usize = 0x10000;
/// Units of a path including the terminator (`wchar[260]`).
const PATH_UNITS: usize = 260;
/// Longest language name (`wchar[16]`).
pub const LANGUAGE_UNITS: usize = 15;
/// `LoadStringW` buffer of the PE reader.
const STRING_BUFFER_UNITS: usize = 1024;
/// Units of the `reset` and `tag` buffers, terminator included.
const SMALL_BUFFER_UNITS: usize = 32;
/// `RT_STRING`.
const RT_STRING: u32 = 6;

/// The item filter of a resource: tags and numbers that are dropped while loading (`error.md` 4.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    tags: Vec<&'static str>,
    numbers: Vec<u32>,
}

impl Filters {
    /// The exclusion lists `ResourceSet::initialize` (`0x140283F80`) builds from the configuration: a module that
    /// is not configured drops its tagged messages, and the USB error slots of devices that are not configured
    /// drop their numbers.
    pub fn from_config(config: &AmConfig) -> Self {
        let mut tags = Vec::new();
        let io = config.jvs.enable || config.can.enable || config.usbio.enable;
        let credit = config.credit.enable && config.credit.coin_selector_as6db.enable;
        let network = config.network.enable;
        let network_mass = config.network.support_standalone;
        let lan_install = config.laninstall.mode != 0;
        let auth = config.allnet_auth.enable;
        let accounting = config.allnet_accounting.mode != 0;
        let accounting_a = config.allnet_accounting.mode == 2;
        let aime = config.aime.enable;
        let usb_count = config.usbdevice.app.required().len() as u64;
        let rules: [(bool, &'static str); 10] = [
            (!io, "io"),
            (!credit, "coin_selector_AS6DB"),
            (!network, "network"),
            (!network || network_mass, "network_mass"),
            (!lan_install, "lan_install"),
            (!auth, "auth"),
            (!accounting, "accounting"),
            (!accounting || accounting_a, "accounting_A_B2"),
            (!aime, "aime"),
            (usb_count == 0, "usb_device"),
        ];
        tags.extend(rules.iter().filter(|(drop, _)| *drop).map(|(_, tag)| *tag));
        let numbers = if usb_count < 9 { (usb_count + 951..=959).map(|n| n as u32).collect() } else { Vec::new() };
        Self { tags, numbers }
    }

    /// `addItem` steps 1 and 2: whether the item is dropped. Empty tags never match.
    fn drops(&self, tag: &str, number: u16) -> bool {
        (!tag.is_empty() && self.tags.contains(&tag)) || self.numbers.contains(&u32::from(number))
    }
}

fn key(item: &FatalError) -> u32 {
    (u32::from(item.number) << 16) | u32::from(item.sub_number)
}

/// `am::util::ErrorResource`: the items of one language, sorted by key, at most [`ITEM_CAPACITY`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorResource {
    items: Vec<FatalError>,
}

impl ErrorResource {
    /// The items in key order.
    pub fn items(&self) -> &[FatalError] {
        &self.items
    }

    /// `addItem` (`0x140282C10`) steps 3 and 4: a filtered item is accepted and dropped, an item with a known
    /// key overwrites it in place, a new key is inserted in order. `false` when the resource is full.
    fn add(&mut self, item: FatalError, tag: &str, filters: &Filters) -> bool {
        if filters.drops(tag, item.number) {
            return true;
        }
        match self.items.binary_search_by_key(&key(&item), key) {
            Ok(index) => self.items[index] = item,
            Err(index) if self.items.len() < ITEM_CAPACITY => self.items.insert(index, item),
            Err(_) => return false,
        }
        true
    }

    /// `get(number, sub, flag)` (`0x140281F40`): the item with exactly this key; unless `exact`, a miss with a
    /// non-zero `sub` retries with `sub = 0`.
    pub fn lookup(&self, number: u16, sub_number: u16, exact: bool) -> Option<&FatalError> {
        let find = |sub| {
            let wanted = (u32::from(number) << 16) | u32::from(sub);
            self.items.binary_search_by_key(&wanted, key).ok().map(|index| &self.items[index])
        };
        find(sub_number).or_else(|| if exact || sub_number == 0 { None } else { find(0) })
    }
}

/// The reset-type name table (`0x1402825C0`).
fn reset_type_from_name(name: &str) -> Option<u32> {
    match name {
        "none" => Some(0),
        "shutdown" => Some(1),
        "operation" => Some(2),
        "test" => Some(3),
        "any" | "" => Some(4),
        _ => None,
    }
}

/// An item of a file: the record and its tag.
type Parsed = (FatalError, String);

fn json_u16(element: &Value, member: &str) -> Option<u16> {
    u16::try_from(element.get(member)?.as_i64()?).ok()
}

fn json_text<'a>(element: &'a Value, member: &str, limit: usize) -> Option<&'a str> {
    let text = element.get(member)?.as_str()?;
    (text.encode_utf16().count() < limit).then_some(text)
}

/// One element of a JSON file (`0x140281150` / `0x140281340`): number, sub, reset, message, tag, all required.
fn json_item(element: &Value) -> Option<Parsed> {
    let number = json_u16(element, "number")?;
    let sub_number = json_u16(element, "sub")?;
    let reset = reset_type_from_name(json_text(element, "reset", SMALL_BUFFER_UNITS)?)?;
    let message = json_text(element, "message", FATAL_MESSAGE_UNITS)?;
    let tag = json_text(element, "tag", SMALL_BUFFER_UNITS)?;
    Some((FatalError::new(number, sub_number, Default::default(), message, reset), tag.to_owned()))
}

/// `loadJson` (`0x140282150`). `false` fails the attempt: not JSON, root not an array, or the resource is full.
fn load_json(bytes: &[u8], resource: &mut ErrorResource, filters: &Filters) -> bool {
    if bytes.len() >= MAX_JSON_BYTES {
        return false;
    }
    let Ok(Value::Array(elements)) = serde_json::from_slice::<Value>(bytes) else {
        return false;
    };
    elements.iter().filter_map(json_item).all(|(item, tag)| resource.add(item, &tag, filters))
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    data.get(offset..offset.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset.checked_add(4)?).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The parts of a PE image the string-table reader needs.
struct Pe<'a> {
    data: &'a [u8],
    /// `(virtual address, virtual size, raw pointer, raw size)` per section.
    sections: Vec<(u32, u32, u32, u32)>,
    /// Resource directory RVA.
    resources: u32,
}

impl<'a> Pe<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.get(..2)? != b"MZ" {
            return None;
        }
        let pe = read_u32(data, 0x3C)? as usize;
        if data.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
            return None;
        }
        let section_count = usize::from(read_u16(data, pe + 6)?);
        let optional_size = usize::from(read_u16(data, pe + 20)?);
        let optional = pe + 24;
        let directories = match read_u16(data, optional)? {
            0x10B => optional + 96,
            0x20B => optional + 112,
            _ => return None,
        };
        let resources = read_u32(data, directories + 2 * 8)?;
        let table = optional + optional_size;
        let sections = (0..section_count)
            .map(|index| {
                let base = table + index * 40;
                Some((
                    read_u32(data, base + 12)?,
                    read_u32(data, base + 8)?,
                    read_u32(data, base + 20)?,
                    read_u32(data, base + 16)?,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self { data, sections, resources })
    }

    /// File offset of a relative virtual address.
    fn offset(&self, rva: u32) -> Option<usize> {
        self.sections
            .iter()
            .find(|&&(va, vsize, _, raw_size)| rva >= va && rva - va < vsize.max(raw_size))
            .map(|&(va, _, raw, _)| (raw + (rva - va)) as usize)
    }

    /// The entries of the resource directory at `offset` (relative to the resource root): `(name, target)`.
    fn directory(&self, root: usize, offset: u32) -> Option<Vec<(u32, u32)>> {
        let base = root.checked_add(offset as usize)?;
        let named = usize::from(read_u16(self.data, base + 12)?);
        let ids = usize::from(read_u16(self.data, base + 14)?);
        (0..named + ids)
            .map(|index| {
                let entry = base + 16 + index * 8;
                Some((read_u32(self.data, entry)?, read_u32(self.data, entry + 4)?))
            })
            .collect()
    }

    /// The `RT_STRING` blocks: `(block id, 16 strings)`. `None` when there is no string table or a block is
    /// malformed; a block with id 0 fails the module (`EnumFunc`, `error.md` 5.2).
    fn string_blocks(&self) -> Option<Vec<(u32, Vec<Vec<u16>>)>> {
        let root = self.offset(self.resources)?;
        let types = self.directory(root, 0)?;
        let (_, types_target) = types.into_iter().find(|&(name, _)| name & 0x8000_0000 == 0 && name == RT_STRING)?;
        let names = self.directory(root, types_target & 0x7FFF_FFFF)?;
        let mut blocks = Vec::new();
        for (name, target) in names {
            if name & 0x8000_0000 != 0 {
                continue;
            }
            if name == 0 {
                return None;
            }
            let languages = self.directory(root, target & 0x7FFF_FFFF)?;
            let (_, entry) = languages
                .iter()
                .copied()
                .find(|&(language, _)| language == 0)
                .or_else(|| languages.first().copied())?;
            let entry = root.checked_add((entry & 0x7FFF_FFFF) as usize)?;
            let rva = read_u32(self.data, entry)?;
            let size = read_u32(self.data, entry + 4)? as usize;
            let start = self.offset(rva)?;
            let block = self.data.get(start..start.checked_add(size)?)?;
            blocks.push((name, split_block(block)?));
        }
        Some(blocks)
    }
}

/// The 16 length-prefixed strings of a string-table block.
fn split_block(block: &[u8]) -> Option<Vec<Vec<u16>>> {
    let mut position = 0;
    let mut strings = Vec::with_capacity(16);
    for _ in 0..16 {
        let length = usize::from(read_u16(block, position)?);
        position += 2;
        let bytes = block.get(position..position.checked_add(length * 2)?)?;
        position += length * 2;
        strings.push(bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect());
    }
    Some(strings)
}

fn strip_prefix<'a>(units: &'a [u16], prefix: &str) -> Option<&'a [u16]> {
    let head = units.get(..prefix.len())?;
    head.iter().zip(prefix.bytes()).all(|(&unit, byte)| unit == u16::from(byte)).then(|| &units[prefix.len()..])
}

/// `wcstoul(text, NULL, 0)` on a 32-bit `unsigned long`: blanks, a sign, then decimal, `0x` hexadecimal or
/// leading-zero octal digits; trailing characters are ignored, overflow saturates.
fn parse_unsigned(text: &[u16]) -> u32 {
    let char_at = |index: usize| text.get(index).and_then(|&unit| char::from_u32(u32::from(unit)));
    let mut position = 0;
    while char_at(position).is_some_and(|c| c.is_ascii_whitespace()) {
        position += 1;
    }
    let negative = char_at(position) == Some('-');
    if matches!(char_at(position), Some('+' | '-')) {
        position += 1;
    }
    let radix = if char_at(position) == Some('0')
        && matches!(char_at(position + 1), Some('x' | 'X'))
        && char_at(position + 2).is_some_and(|c| c.is_ascii_hexdigit())
    {
        position += 2;
        16
    } else if char_at(position) == Some('0') {
        8
    } else {
        10
    };
    let mut value: u32 = 0;
    let mut overflow = false;
    while let Some(digit) = char_at(position).and_then(|c| c.to_digit(radix)) {
        match value.checked_mul(radix).and_then(|v| v.checked_add(digit)) {
            Some(next) => value = next,
            None => overflow = true,
        }
        position += 1;
    }
    match (overflow, negative) {
        (true, _) => u32::MAX,
        (false, true) => value.wrapping_neg(),
        (false, false) => value,
    }
}

/// `loadItem` (`0x1402827B0`): one string of the table as an item. `None` for an invalid item.
fn string_item(units: &[u16]) -> Option<Parsed> {
    let mut segments = units.split(|&unit| unit == 0);
    let message = segments.next()?;
    let (mut number, mut sub_number, mut reset, mut tag) = (0u16, 0u16, 0u32, String::new());
    for segment in segments {
        if let Some(value) = strip_prefix(segment, "number=") {
            number = u16::try_from(parse_unsigned(value)).ok()?;
        } else if let Some(value) = strip_prefix(segment, "sub=") {
            sub_number = u16::try_from(parse_unsigned(value)).ok()?;
        } else if let Some(name) = strip_prefix(segment, "reset=") {
            reset = reset_type_from_name(&String::from_utf16(name).ok()?)?;
        } else if let Some(text) = strip_prefix(segment, "tag=") {
            tag = if text.len() < SMALL_BUFFER_UNITS { String::from_utf16_lossy(text) } else { String::new() };
        } else {
            return None;
        }
    }
    let mut item = FatalError { number, sub_number, reset_type: reset, ..FatalError::default() };
    for (slot, &unit) in item.message.iter_mut().zip(message.iter().take(FATAL_MESSAGE_UNITS - 1)) {
        *slot = unit;
    }
    Some((item, tag))
}

/// The PE attempt (`0x140282080`): every string of every `RT_STRING` block becomes an item; strings that are
/// empty, too long, invalid or without a number are skipped. `false` fails the attempt.
fn load_pe(bytes: &[u8], resource: &mut ErrorResource, filters: &Filters) -> bool {
    let Some(blocks) = Pe::parse(bytes).and_then(|pe| pe.string_blocks()) else {
        return false;
    };
    blocks
        .iter()
        .flat_map(|(_, strings)| strings.iter())
        .filter(|units| !units.is_empty() && units.len() < STRING_BUFFER_UNITS)
        .filter_map(|units| string_item(units))
        .filter(|(item, _)| item.number != 0)
        .all(|(item, tag)| resource.add(item, &tag, filters))
}

/// One path (`0x140281DD0`): the JSON attempt first, the PE attempt when it fails.
fn load_file(path: &std::path::Path, resource: &mut ErrorResource, filters: &Filters) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    load_json(&bytes, resource, filters) || load_pe(&bytes, resource, filters)
}

/// `am::ResourceSet` (`app+24`): the language, the two loaded resources and what is needed to reload them.
#[derive(Debug, Default)]
pub struct ResourceSet {
    initialised: bool,
    folder: String,
    custom_ids: Vec<String>,
    filters: Filters,
    language: Vec<u16>,
    error: Option<ErrorResource>,
    warning: Option<ErrorResource>,
}

impl ResourceSet {
    /// `ResourceSet::initialize` (`0x140283F80`): 0 ok, 1 the language could not be loaded, 2 no folder or no
    /// language. A failed initialisation leaves the set unusable for the rest of the run.
    pub fn initialize(
        &mut self,
        paths: &dyn Fs,
        folder: &str,
        language: &str,
        custom_ids: Vec<String>,
        filters: Filters,
    ) -> u32 {
        if folder.is_empty() {
            return 2;
        }
        *self = Self { initialised: true, folder: folder.to_owned(), custom_ids, filters, ..Self::default() };
        let units: Vec<u16> = language.encode_utf16().collect();
        let result = self.change_language(paths, &units);
        self.initialised = result == 0;
        result
    }

    /// The language as the `wchar[16]` the facade returns.
    pub fn language_array(&self) -> [u16; 16] {
        let mut out = [0; 16];
        out[..self.language.len()].copy_from_slice(&self.language);
        out
    }

    /// `changeLanguage` (`0x140284350`): 0 ok (also for the current language), 1 failed, 2 empty name. Nothing
    /// changes unless both resources load.
    pub fn change_language(&mut self, paths: &dyn Fs, language: &[u16]) -> u32 {
        let language = &language[..language.iter().position(|&unit| unit == 0).unwrap_or(language.len())];
        if language.is_empty() {
            return 2;
        }
        if !self.initialised {
            return 1;
        }
        if language == self.language || language.len() > LANGUAGE_UNITS {
            return u32::from(language != self.language);
        }
        let name = String::from_utf16_lossy(language);
        let Some(error) = self.load(paths, "error", &name, true) else {
            return 1;
        };
        let Some(warning) = self.load(paths, "warning", &name, false) else {
            return 1;
        };
        self.error = Some(error);
        self.warning = Some(warning);
        self.language = language.to_vec();
        0
    }

    /// `folder + name`, an existing file only (`0x140227E50`, `0x140227EB0`).
    fn candidate(&self, paths: &dyn Fs, name: &str) -> Option<PathBuf> {
        let mut full = self.folder.clone();
        if !full.is_empty() && !full.ends_with('\\') {
            full.push('\\');
        }
        let units = |text: &str| text.encode_utf16().count();
        if units(&self.folder) + units(name) + 1 >= PATH_UNITS {
            return None;
        }
        full.push_str(name);
        let host = paths.map(&full);
        host.is_file().then_some(host)
    }

    /// `loadError` / `loadWarning` (`0x140284560`, `0x1402849A0`): the required `<base>.<language>.dll|json`,
    /// then for the error resource the optional files of every custom id; filters apply to the error resource
    /// only.
    fn load(&self, paths: &dyn Fs, base: &str, language: &str, with_custom_ids: bool) -> Option<ErrorResource> {
        let mut files: Vec<PathBuf> = ["dll", "json"]
            .iter()
            .filter_map(|ext| self.candidate(paths, &format!("{base}.{language}.{ext}")))
            .collect();
        if files.is_empty() {
            return None;
        }
        let none = Filters::default();
        let filters = if with_custom_ids {
            for id in &self.custom_ids {
                files.extend(
                    ["dll", "json"]
                        .iter()
                        .filter_map(|ext| self.candidate(paths, &format!("{base}.{id}.{language}.{ext}"))),
                );
            }
            &self.filters
        } else {
            &none
        };
        let mut resource = ErrorResource::default();
        files.iter().all(|file| load_file(file, &mut resource, filters)).then_some(resource)
    }

    /// The error resource when one is loaded.
    pub fn errors(&self) -> Option<&ErrorResource> {
        self.error.as_ref()
    }

    /// The warning resource when one is loaded.
    pub fn warnings(&self) -> Option<&ErrorResource> {
        self.warning.as_ref()
    }
}
