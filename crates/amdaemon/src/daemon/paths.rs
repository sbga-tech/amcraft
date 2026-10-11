//! Path string helpers of the original (`0x14005B220`, `0x14005BAB0`, `0x14005B950`, `0x14005B3C0`).
//!
//! The original works on `std::wstring` with the separator set `L"\\/"` (`0x1405E66A8`). Both separators are
//! ASCII, so byte indices of a UTF-8 string are valid split points. Lengths that the binary compares against
//! `wchar_t` limits are counted in UTF-16 units ([`utf16_len`]).

/// `L"\\/"`: the two path separators (`0x1405E66A8`).
const SEPARATORS: [char; 2] = ['\\', '/'];

/// Length in UTF-16 code units, i.e. `std::wstring::size()`.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Whether `ch` is `\` or `/`.
fn is_separator(ch: char) -> bool {
    SEPARATORS.contains(&ch)
}

/// Parent directory helper `0x14005B220`: the path up to the last non-separator character before the last
/// separator run, **without** a trailing separator.
///
/// Empty when there is no separator, when the separators lead the string (`\a.log`), or when the result would be
/// a bare drive prefix `X:` (`0x14005B150`).
pub(crate) fn parent_dir(path: &str) -> &str {
    let Some(last_separator) = path.rfind(is_separator) else {
        return "";
    };
    let before = &path[..last_separator];
    let trimmed = before.trim_end_matches(is_separator);
    if trimmed.is_empty() || is_bare_drive(trimmed) {
        return "";
    }
    trimmed
}

/// `0x14005B150`: exactly a drive letter and a colon.
fn is_bare_drive(text: &str) -> bool {
    let mut chars = text.chars();
    matches!((chars.next(), chars.next(), chars.next()), (Some(drive), Some(':'), None) if drive.is_ascii_alphabetic())
}

/// `0x14005BAB0`: an empty string becomes `.\`, a string not ending in a separator gets `\` appended.
pub(crate) fn ensure_trailing_separator(path: &mut String) {
    match path.chars().next_back() {
        None => path.push_str(".\\"),
        Some(last) if !is_separator(last) => path.push('\\'),
        Some(_) => {}
    }
}

/// `0x14005B950` (`join`): appends `relative` to `path`.
///
/// An empty `relative` leaves `path` unchanged; an empty `path` becomes `relative`; otherwise `path` gets its
/// trailing separator ensured and the leading separators of `relative` are skipped.
pub(crate) fn join(path: &mut String, relative: &str) {
    if relative.is_empty() {
        return;
    }
    if path.is_empty() {
        path.push_str(relative);
        return;
    }
    ensure_trailing_separator(path);
    path.push_str(relative.trim_start_matches(is_separator));
}

/// The part after the last separator (`0x14005B3C0`), or the whole string without a separator.
pub(crate) fn file_name(path: &str) -> &str {
    path.rfind(is_separator).map_or(path, |index| &path[index + 1..])
}

/// The directory part of `path` including its trailing separator (`0x14005BE50`); empty without a separator.
pub(crate) fn directory_with_separator(path: &str) -> &str {
    path.rfind(is_separator).map_or("", |index| &path[..=index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_skips_leading_separators_of_the_relative_part() {
        let mut path = String::from(r"Y:\game");
        join(&mut path, r"\\abaas\log");
        assert_eq!(path, r"Y:\game\abaas\log");

        let mut empty = String::new();
        join(&mut empty, r"\rel");
        assert_eq!(empty, r"\rel", "an empty base is replaced verbatim");

        let mut unchanged = String::from("base");
        join(&mut unchanged, "");
        assert_eq!(unchanged, "base", "an empty relative part keeps the base without a separator");

        let mut only_separators = String::from("base");
        join(&mut only_separators, r"\/");
        assert_eq!(only_separators, r"base\");
    }
}
