//! ZIP entry names must honor the language flag, not UTF-8 autodetection.

use std::{borrow::Cow, io::Read, str::Utf8Error};

// Unicode mappings for CP437 bytes 0x80..=0xff. ASCII bytes map unchanged.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ',
    'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ',
    'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕',
    '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦',
    '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐',
    '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±',
    '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Decode the effective name using bit 11 (UTF-8) or CP437.
///
/// ZIP 9 applies valid UnicodePath extra fields, including their CRC and UTF-8
/// checks, to name_raw() and sets bit 11 before exposing the entry. Using these
/// effective bytes and flags preserves that override without guessing encodings.
/// Malformed names explicitly marked UTF-8 are rejected, not decoded as CP437.
pub fn entry_name<'a, R: Read>(
    entry: &'a zip::read::ZipFile<'_, R>,
) -> Result<Cow<'a, str>, Utf8Error> {
    let raw = entry.name_raw();
    if entry.flags().as_u16() & (1 << 11) != 0 || raw.is_ascii() {
        return std::str::from_utf8(raw).map(Cow::Borrowed);
    }
    Ok(Cow::Owned(
        raw.iter()
            .map(|&byte| {
                if byte < 0x80 {
                    char::from(byte)
                } else {
                    CP437_HIGH[usize::from(byte - 0x80)]
                }
            })
            .collect(),
    ))
}
