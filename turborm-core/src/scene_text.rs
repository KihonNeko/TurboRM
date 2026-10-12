//! Typed-text (RootText) decoding and approximate layout.
//!
//! Character ordering, paragraph splitting and style association are
//! recovered exactly from the stored records. Glyph metrics are **not** known,
//! so the layout below is an explicit approximation and every rendered block
//! reports it as such.

use anyhow::{anyhow, bail, ensure, Result};
use std::collections::HashMap;

/// Firmware-observed text-box width when the document stores none.
/// All thirteen copied RootText blocks store 936 or 1152 explicitly.
pub const DEFAULT_TEXT_WIDTH: f32 = 936.0;

/// Approximate. Derived from the one copied `CrdtGlyphItem` highlight
/// rectangle, whose height is 45 scene units over a single text line.
pub const LINE_HEIGHT: f32 = 45.0;

/// Approximate. The same highlight rectangle covers six characters in 97.45
/// units (16.24/char); with the 0.58 em advance of a humanist sans this puts
/// the body size near 28 units, and 28 x 1.6 reproduces the 45-unit line.
pub const BODY_FONT_SIZE: f32 = 28.0;

/// Approximate mean advance width as a fraction of the font size, from the
/// same rectangle (16.24 / 28.0).
const MEAN_ADVANCE: f32 = 0.58;

/// Approximate. Indent step for list levels and indented paragraph styles.
pub const INDENT_STEP: f32 = 40.0;

/// Qt's default `QTextDocument::documentMargin`, between the text box and its
/// first block. Recovered from anchored nodes: their stored initial origin x
/// is -464 where the text box starts at x = -468, and the firmware's anchor
/// position (`0xf0a250`) is the block layout position, which includes this
/// margin, plus `cursorToX`.
pub const DOCUMENT_MARGIN: f32 = 4.0;

/// The firmware's body font size in pixels (`0xf012c0`: `reMarkable Sans`,
/// `setPixelSize(32)` for Paragraph; 64, 46 and 36 for the three headings).
/// Paragraph spacing below is expressed in multiples of it.
const FIRMWARE_BODY_PIXELS: f32 = 32.0;

/// Font stack. The device faces are `reMarkable Sans`/`reMarkable Serif`
/// (firmware resources `:reMarkableSans-*.ttf`); they are not redistributed
/// here, so the renderer falls back to common metric-compatible families.
pub const FONT_FAMILY: &str =
    "reMarkable Sans, Noto Sans, DejaVu Sans, Liberation Sans, Helvetica, Arial, sans-serif";

const MAX_TEXT_RECORDS: usize = 200_000;
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STYLE_ENTRIES: usize = 100_000;
const MAX_LINES: usize = 20_000;

/// A compact scene identity: `(author index, counter)`.
pub type TextId = (u16, u64);

/// A run of identities without characters: `(first identity, run length,
/// index in the text (in `char`s) of the character the run precedes)`.
pub type HiddenRun = (TextId, u64, usize);

/// Recovered paragraph style, keyed by the identity of its first character.
///
/// Legacy byte and newer kind/parameter forms are both retained; `kind` and
/// `parameters` are the interpreted pair after the firmware's legacy
/// conversion (`0x103d750`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParagraphStyle {
    pub legacy: u8,
    pub kind: u8,
    pub parameters: u32,
}

impl Default for ParagraphStyle {
    fn default() -> Self {
        // Legacy 1 / Paragraph is the firmware's plain body style.
        Self {
            legacy: 1,
            kind: 2,
            parameters: 0,
        }
    }
}

impl ParagraphStyle {
    /// Apply the legacy byte conversion recovered from `0x103d750`.
    fn from_wire(legacy: u8, kind: u8, parameters: u32) -> Self {
        if kind != 0 {
            return Self {
                legacy,
                kind,
                parameters,
            };
        }
        let (kind, parameters) = match legacy {
            1 => (2, 0),
            2 => (2, 1),
            3 => (2, 2),
            4 => (3, 0),
            5 => (3, 1),
            6 => (3, 0x10),
            7 => (3, 0x110),
            8 => (3, 0x11),
            9 => (3, 0x111),
            10 => (3, 0x20),
            11 => (3, 0x21),
            12 => (4, 0),
            // The converter keeps its (1, 0) default outside 1..=12.
            _ => (1, 0),
        };
        Self {
            legacy,
            kind,
            parameters,
        }
    }

    pub fn is_text(self) -> bool {
        self.kind == 2
    }
    pub fn is_list(self) -> bool {
        self.kind == 3
    }
    /// Text-kind category: 0 Paragraph, 1 Title, 2 Subheading2, 3 Subheading.
    pub fn text_category(self) -> u32 {
        self.parameters & 0xff
    }
    /// List indent level (low nibble) or text indent (bits 8..11).
    pub fn indent_level(self) -> u32 {
        if self.is_list() {
            self.parameters & 0xf
        } else {
            (self.parameters >> 8) & 0xf
        }
    }
    /// List subtype: 0 Bulletpoint, 1 Checkbox, 2 NumberedList.
    pub fn list_subtype(self) -> u32 {
        (self.parameters >> 4) & 0xf
    }
    /// Checkbox state: 0 unchecked, 1 checked, 2 partial, 3 implicitly checked.
    pub fn check_state(self) -> u32 {
        (self.parameters >> 8) & 0x7
    }

    /// Size multiplier relative to [`BODY_FONT_SIZE`]. Approximate.
    pub fn size_scale(self) -> f32 {
        if !self.is_text() {
            return 1.0;
        }
        match self.text_category() {
            1 => 1.75, // Title
            2 => 1.35, // Subheading2
            3 => 1.15, // Subheading
            _ => 1.0,
        }
    }

    /// Space above each paragraph of this style: the `BlockTopMargin` the
    /// firmware's paragraph formats set (`0xf01620`), 0.75 x the 32 px body
    /// size for body text and lists and 2.75, 1.75 and 1.25 x for Title,
    /// Subheading2 and Subheading.
    pub fn top_margin(self) -> f32 {
        let factor = if self.is_text() {
            match self.text_category() {
                1 => 2.75,
                2 => 1.75,
                3 => 1.25,
                _ => 0.75,
            }
        } else {
            0.75
        };
        FIRMWARE_BODY_PIXELS * factor
    }

    /// Whether the firmware draws this style in a bold weight. Approximate.
    pub fn is_bold(self) -> bool {
        self.is_text() && matches!(self.text_category(), 1..=3)
    }
}

/// One stored text record: an ID run of either text or deleted characters.
#[derive(Debug, Clone)]
struct TextRecord {
    item: TextId,
    left: TextId,
    /// Deleted/skipped run length when `text` is absent.
    gap: u32,
    text: Option<String>,
    /// An empty value carrying the trailing `2:4` scalar: an inline
    /// formatting marker that occupies exactly one identity and draws nothing.
    marker: bool,
}

/// A decoded RootText block: ordered characters, their origins and styles.
#[derive(Debug, Clone)]
pub struct RootText {
    /// Character text in reading order.
    pub text: String,
    /// The identity of each character in `text`, by `char` index.
    pub ids: Vec<TextId>,
    /// Top-left of the text box in scene coordinates.
    pub position: (f32, f32),
    /// Text box width in scene units.
    pub width: f32,
    /// Paragraph styles keyed by the identity of the paragraph's first character.
    pub styles: HashMap<TextId, ParagraphStyle>,
    /// A record used an encoding marker other than 1 (UTF-8).
    pub had_unknown_encoding: bool,
    /// Identity runs that hold no character, deleted runs and formatting
    /// markers. An anchor may name one of them.
    pub hidden: Vec<HiddenRun>,
}

impl RootText {
    /// The character index an identity resolves to: its own character, or,
    /// for a deleted character or a formatting marker, the character it sits
    /// before. The firmware resolves anchors on deleted characters too; its
    /// anchor log labels them "deleted" rather than "present" (`0xe5e840`).
    pub fn index_of(&self, id: TextId, visible: &HashMap<TextId, usize>) -> Option<usize> {
        visible.get(&id).copied().or_else(|| {
            self.hidden
                .iter()
                .find(|(start, length, _)| {
                    start.0 == id.0 && start.1 <= id.1 && id.1 - start.1 < *length
                })
                .map(|&(_, _, index)| index)
        })
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| anyhow!("text offset overflow"))?;
        ensure!(end <= self.bytes.len(), "truncated text field");
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }
    fn var(&mut self, bits: u32) -> Result<u64> {
        let mut out = 0u64;
        for shift in (0..bits).step_by(7) {
            let byte = self.take(1)?[0];
            let part = u64::from(byte & 0x7f);
            ensure!(
                shift < 64 && part <= (u64::MAX >> shift),
                "text integer overflow"
            );
            out |= part << shift;
            ensure!(bits == 64 || out < (1u64 << bits), "text integer overflow");
            if byte & 0x80 == 0 {
                return Ok(out);
            }
        }
        bail!("unterminated text integer")
    }
    fn id(&mut self) -> Result<TextId> {
        let author = self.var(16)? as u16;
        let counter = self.var(48)?;
        Ok((author, counter))
    }
    fn u32le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("checked width"),
        ))
    }
}

/// A tagged field inside a text container.
struct TextField<'a> {
    number: u16,
    kind: u8,
    bytes: &'a [u8],
}

fn text_fields(bytes: &[u8]) -> Result<Vec<TextField<'_>>> {
    let mut cursor = Cursor::new(bytes);
    let mut out = Vec::new();
    while !cursor.done() {
        ensure!(out.len() < 4096, "too many fields in a text container");
        let tag = cursor.var(16)? as u16;
        ensure!(tag >> 12 == 0, "text field tag outside supported range");
        let (number, kind) = (tag >> 4, (tag & 15) as u8);
        let bytes = match kind {
            1 | 4 | 8 => cursor.take(kind as usize)?,
            12 => {
                let n = cursor.u32le()? as usize;
                cursor.take(n)?
            }
            15 => {
                let start = cursor.at;
                cursor.var(16)?;
                cursor.var(48)?;
                &cursor.bytes[start..cursor.at]
            }
            _ => bail!("unsupported text wire type {kind}"),
        };
        out.push(TextField {
            number,
            kind,
            bytes,
        });
    }
    Ok(out)
}

fn pick<'a>(fields: &'a [TextField<'a>], number: u16, kind: u8) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|f| f.number == number && f.kind == kind)
        .map(|f| f.bytes)
}

fn decode_id(bytes: &[u8]) -> Result<TextId> {
    let mut cursor = Cursor::new(bytes);
    let id = cursor.id()?;
    ensure!(cursor.done(), "invalid text identity");
    Ok(id)
}

/// Parse the counted record array of the text sequence.
fn parse_records(array: &[u8]) -> Result<(Vec<TextRecord>, bool)> {
    let mut cursor = Cursor::new(array);
    let count = cursor.var(64)?;
    ensure!(
        count <= MAX_TEXT_RECORDS as u64,
        "too many stored text records"
    );
    let mut records = Vec::with_capacity(count as usize);
    let mut unknown_encoding = false;
    let mut total_bytes = 0usize;
    for _ in 0..count {
        let tag = cursor.var(16)? as u16;
        ensure!(tag & 15 == 12, "invalid text record tag");
        let length = cursor.u32le()? as usize;
        let record = cursor.take(length)?;
        let fields = text_fields(record)?;
        let item = pick(&fields, 2, 15)
            .ok_or_else(|| anyhow!("text record without an item identity"))
            .and_then(decode_id)?;
        let left = pick(&fields, 3, 15).map(decode_id).transpose()?;
        let gap = pick(&fields, 5, 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("checked width")))
            .unwrap_or(0);
        let mut text = None;
        let mut format_marker = false;
        if let Some(value) = pick(&fields, 6, 12) {
            let mut inner = Cursor::new(value);
            let byte_count = inner.var(64)? as usize;
            ensure!(byte_count <= MAX_TEXT_BYTES, "text value exceeds the limit");
            total_bytes = total_bytes
                .checked_add(byte_count)
                .ok_or_else(|| anyhow!("text size overflow"))?;
            ensure!(
                total_bytes <= MAX_TEXT_BYTES,
                "stored text exceeds the 4 MiB limit"
            );
            let encoding = inner.take(1)?[0];
            let raw = inner.take(byte_count)?;
            if encoding == 1 {
                text = Some(
                    std::str::from_utf8(raw)
                        .map_err(|_| anyhow!("text record is not valid UTF-8"))?
                        .to_owned(),
                );
            } else {
                // Preserved as an unrenderable run of the same length, so the
                // sequence keeps its identity arithmetic.
                unknown_encoding = true;
                text = Some("\u{fffd}".repeat(raw.len().max(1)));
            }
            // The optional trailing `2:4` scalar is an inline formatting code.
            // On an empty value it makes the record a one-identity marker:
            // copied text brackets a formatted word with codes 1 and 2, and the
            // following run anchors on the marker's identity, so the marker
            // must be counted even though it draws nothing.
            if !inner.done() {
                let trailing = text_fields(&value[inner.at..])?;
                format_marker = raw.is_empty() && pick(&trailing, 2, 4).is_some();
            }
        }
        records.push(TextRecord {
            item,
            left: left.unwrap_or((0, 0)),
            gap,
            text,
            marker: format_marker,
        });
    }
    ensure!(cursor.done(), "text record array has trailing data");
    Ok((records, unknown_encoding))
}

fn parse_styles(map: &[u8]) -> Result<HashMap<TextId, ParagraphStyle>> {
    let mut cursor = Cursor::new(map);
    let count = cursor.var(64)?;
    ensure!(count <= MAX_STYLE_ENTRIES as u64, "too many style entries");
    let mut styles = HashMap::new();
    for _ in 0..count {
        // The key is a bare compact identity: no field tag, no entry length.
        let key = cursor.id()?;
        let tag = cursor.var(16)? as u16;
        ensure!(tag & 15 == 15, "invalid style register tag");
        cursor.id()?;
        let tag = cursor.var(16)? as u16;
        ensure!(tag & 15 == 12, "invalid style value tag");
        let length = cursor.u32le()? as usize;
        let value = cursor.take(length)?;
        let fields = text_fields(value)?;
        let legacy = pick(&fields, 1, 1).map_or(0, |b| b[0]);
        let kind = pick(&fields, 2, 1).map_or(0, |b| b[0]);
        let parameters = pick(&fields, 3, 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("checked width")))
            .unwrap_or(0);
        // A later duplicate key is discarded, matching insertion `0xf0f7a0`.
        styles
            .entry(key)
            .or_insert_with(|| ParagraphStyle::from_wire(legacy, kind, parameters));
    }
    ensure!(cursor.done(), "style map has trailing data");
    Ok(styles)
}

/// Resolve the visible character order from the stored left-origin links.
///
/// Each record is a run of consecutive counters starting at its item ID; a
/// record's left origin is the identity of the character it follows, with
/// `(0, 0)` meaning the start of the document. Concurrent runs sharing one
/// anchor are ordered by descending identity, which reproduces the reading
/// order of every copied sample.
fn order_records(records: &[TextRecord]) -> (String, Vec<TextId>, Vec<HiddenRun>, bool) {
    let mut by_anchor: HashMap<TextId, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        by_anchor.entry(record.left).or_default().push(index);
    }
    for list in by_anchor.values_mut() {
        list.sort_by(|&a, &b| records[b].item.cmp(&records[a].item));
    }
    let mut text = String::new();
    let mut ids = Vec::new();
    let mut hidden = Vec::new();
    let mut visited = vec![false; records.len()];
    let mut emitted = 0usize;
    // An explicit work stack keeps a long insertion chain off the call stack.
    // Each entry is an anchor whose children are still to be visited, plus the
    // index of the next child to take.
    let mut stack: Vec<(TextId, usize)> = vec![((0, 0), 0)];
    while let Some((anchor, cursor)) = stack.pop() {
        let empty = Vec::new();
        let children = by_anchor.get(&anchor).unwrap_or(&empty);
        let Some(&index) = children.get(cursor) else {
            continue;
        };
        // Resume this anchor at its next child once the subtree is finished.
        stack.push((anchor, cursor + 1));
        if visited[index] {
            continue;
        }
        visited[index] = true;
        emitted += 1;
        let record = &records[index];
        let run = if record.marker {
            // A formatting marker: one identity, no character.
            hidden.push((record.item, 1, ids.len()));
            1
        } else if let Some(value) = &record.text {
            text.push_str(value);
            let mut count = 0u64;
            for _ in value.chars() {
                ids.push((record.item.0, record.item.1 + count));
                count += 1;
            }
            count
        } else {
            if record.gap > 0 {
                hidden.push((record.item, u64::from(record.gap), ids.len()));
            }
            u64::from(record.gap)
        };
        // A later character anchors on an earlier one, so the last character
        // of this run is descended into last: push it first.
        for step in (0..run).rev() {
            stack.push(((record.item.0, record.item.1 + step), 0));
        }
        // Guard against a pathological file building an unbounded stack.
        if stack.len() > MAX_TEXT_RECORDS * 2 {
            break;
        }
    }
    (text, ids, hidden, emitted != records.len())
}

/// Decode one RootText block payload.
pub fn parse_root_text(payload: &[u8]) -> Result<RootText> {
    let fields = text_fields(payload)?;
    let mut position = (-DEFAULT_TEXT_WIDTH / 2.0, 0.0);
    if let Some(bytes) = pick(&fields, 3, 12) {
        ensure!(bytes.len() == 16, "invalid text position");
        let x = f64::from_le_bytes(bytes[..8].try_into().expect("checked width"));
        let y = f64::from_le_bytes(bytes[8..].try_into().expect("checked width"));
        ensure!(x.is_finite() && y.is_finite(), "non-finite text position");
        ensure!(
            x.abs() <= 1_000_000.0 && y.abs() <= 1_000_000.0,
            "text position exceeds page bounds"
        );
        position = (x as f32, y as f32);
    }
    // Registered width wins when positive; otherwise the legacy width does.
    let mut width = None;
    if let Some(settings) = pick(&fields, 5, 12) {
        if let Some(register) = pick(&text_fields(settings)?, 1, 12) {
            if let Some(bytes) = pick(&text_fields(register)?, 2, 4) {
                let value = f32::from_le_bytes(bytes.try_into().expect("checked width"));
                if value.is_finite() && value > 0.0 {
                    width = Some(value);
                }
            }
        }
    }
    if width.is_none() {
        if let Some(bytes) = pick(&fields, 4, 4) {
            let value = f32::from_le_bytes(bytes.try_into().expect("checked width"));
            if value.is_finite() && value > 0.0 {
                width = Some(value);
            }
        }
    }
    let width = width.unwrap_or(DEFAULT_TEXT_WIDTH);
    ensure!(width <= 1_000_000.0, "text width exceeds page bounds");

    let mut text = String::new();
    let mut ids = Vec::new();
    let mut hidden = Vec::new();
    let mut styles = HashMap::new();
    let mut had_unknown_encoding = false;
    if let Some(container) = pick(&fields, 2, 12) {
        let container = text_fields(container)?;
        if let Some(sequence) = pick(&container, 1, 12) {
            if let Some(array) = pick(&text_fields(sequence)?, 1, 12) {
                let (records, unknown) = parse_records(array)?;
                had_unknown_encoding = unknown;
                let (ordered, ordered_ids, hidden_runs, _incomplete) = order_records(&records);
                text = ordered;
                ids = ordered_ids;
                hidden = hidden_runs;
            }
        }
        if let Some(style_container) = pick(&container, 2, 12) {
            if let Some(map) = pick(&text_fields(style_container)?, 1, 12) {
                styles = parse_styles(map)?;
            }
        }
    }
    Ok(RootText {
        text,
        ids,
        position,
        width,
        styles,
        had_unknown_encoding,
        hidden,
    })
}

/// One laid-out visual line.
#[derive(Debug, Clone, PartialEq)]
pub struct LaidLine {
    pub text: String,
    /// Left edge in scene coordinates, including the style indent.
    pub x: f32,
    /// Text baseline in scene coordinates.
    pub baseline: f32,
    pub font_size: f32,
    pub bold: bool,
    /// Bullet, checkbox or number marker drawn in the left margin, if any.
    pub marker: Option<String>,
    /// Index into [`RootText::text`] (in `char`s) of the line's first character.
    pub start: usize,
    /// One past the line's last character, counting the paragraph's closing
    /// newline on its last line.
    pub end: usize,
    /// Approximate horizontal advance of one character on this line.
    pub advance: f32,
}

/// The laid-out text block.
#[derive(Debug, Clone, PartialEq)]
pub struct TextLayout {
    pub lines: Vec<LaidLine>,
    /// Bounding box of the laid-out text: `(x, y, width, height)`.
    pub bounds: (f32, f32, f32, f32),
}

fn paragraph_marker(style: ParagraphStyle, number: usize) -> Option<String> {
    if !style.is_list() {
        return None;
    }
    match style.list_subtype() {
        0 => Some("\u{2022}".into()),
        1 => Some(
            match style.check_state() {
                1 | 3 => "\u{2611}",
                2 => "\u{25a3}",
                _ => "\u{2610}",
            }
            .into(),
        ),
        2 => Some(format!("{number}.")),
        _ => None,
    }
}

/// Lay out a decoded text block with the approximate metrics above.
///
/// `x_origin` is added to scene x coordinates, matching the page centering
/// the stroke renderer applies.
pub fn layout(root: &RootText, x_origin: f32) -> Result<TextLayout> {
    let left = root.position.0 + x_origin + DOCUMENT_MARGIN;
    let top = root.position.1 + DOCUMENT_MARGIN;
    let mut lines: Vec<LaidLine> = Vec::new();
    let mut y = top;
    let mut numbering = 0usize;
    let mut max_right: f32 = left;

    // Split into paragraphs at newlines, tracking the identity of each
    // paragraph's first character so its stored style can be looked up.
    let chars: Vec<char> = root.text.chars().collect();
    let mut start = 0usize;
    let mut paragraphs: Vec<(usize, usize)> = Vec::new();
    for (index, &character) in chars.iter().enumerate() {
        if character == '\n' {
            paragraphs.push((start, index));
            start = index + 1;
        }
    }
    paragraphs.push((start, chars.len()));

    for (from, to) in paragraphs {
        ensure!(lines.len() < MAX_LINES, "laid-out text exceeds the limit");
        // A style is keyed by the identity the paragraph *follows*: the
        // preceding newline, or (0, 0) for the first paragraph. This is the
        // same anchoring convention the character sequence uses.
        let anchor = if from == 0 {
            Some((0u16, 0u64))
        } else {
            root.ids.get(from - 1).copied()
        };
        let style = anchor
            .and_then(|id| root.styles.get(&id))
            .copied()
            .unwrap_or_default();
        let font_size = BODY_FONT_SIZE * style.size_scale();
        let line_height = LINE_HEIGHT * style.size_scale();
        let indent = INDENT_STEP * style.indent_level() as f32
            + if style.is_list() { INDENT_STEP } else { 0.0 };
        let paragraph_left = left + indent;
        if style.is_list() && style.list_subtype() == 2 {
            numbering += 1;
        } else if !style.is_list() {
            numbering = 0;
        }
        let marker = paragraph_marker(style, numbering.max(1));
        let advance = (font_size * MEAN_ADVANCE).max(0.01);
        let available = (root.width - indent).max(advance);
        let per_line = ((available / advance).floor() as usize).max(1);

        // Each paragraph is its own block, spaced by its format's top margin.
        y += style.top_margin();
        let paragraph: String = chars[from..to].iter().collect();
        let wrapped = wrap(&paragraph, per_line);
        let pieces = wrapped.len();
        let mut first = true;
        let mut start = from;
        for (number, piece) in wrapped.into_iter().enumerate() {
            ensure!(lines.len() < MAX_LINES, "laid-out text exceeds the limit");
            y += line_height;
            let count = piece.chars().count();
            max_right = max_right.max(paragraph_left + count as f32 * advance);
            // The newline closing a paragraph belongs to its last line, as a
            // block separator does in the firmware's QTextDocument.
            let end = if number + 1 == pieces {
                to + 1
            } else {
                start + count
            };
            lines.push(LaidLine {
                text: piece,
                x: paragraph_left,
                baseline: y - line_height * 0.25,
                font_size,
                bold: style.is_bold(),
                marker: first.then(|| marker.clone()).flatten(),
                start,
                end,
                advance,
            });
            start += count;
            first = false;
        }
    }
    let height = (y - top).max(0.0);
    Ok(TextLayout {
        lines,
        bounds: (
            left,
            top,
            (max_right - left).max(root.width),
            height.max(LINE_HEIGHT),
        ),
    })
}

/// Position of the character at `index` (in `char`s): its left edge and the
/// baseline of the line holding it, in the layout's coordinates.
///
/// This mirrors the firmware's anchor lookup (`0xf0a250`), which returns the
/// text-layout origin plus `QTextLine::cursorToX` and `QTextLine::y() +
/// ascent()` for the line containing the character. Here the line is this
/// approximate layout's, so the result is as approximate as the layout.
pub fn char_position(layout: &TextLayout, index: usize) -> Option<(f32, f32)> {
    let line = layout
        .lines
        .iter()
        .find(|line| line.start <= index && index < line.end)
        .or_else(|| layout.lines.last().filter(|line| index >= line.start))?;
    let column = index.saturating_sub(line.start) as f32;
    Some((line.x + column * line.advance, line.baseline))
}

/// Greedy word wrap at a fixed character budget. Approximate: the device
/// measures real glyph advances.
fn wrap(paragraph: &str, per_line: usize) -> Vec<String> {
    if paragraph.chars().count() <= per_line {
        return vec![paragraph.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in paragraph.split_inclusive(' ') {
        let word_len = word.chars().count();
        if current_len > 0 && current_len + word_len > per_line {
            out.push(std::mem::take(&mut current));
            current_len = 0;
        }
        // A single word longer than a line is hard-broken.
        if word_len > per_line {
            let mut chunk = String::new();
            let mut chunk_len = 0;
            for character in word.chars() {
                if chunk_len == per_line {
                    out.push(std::mem::take(&mut chunk));
                    chunk_len = 0;
                }
                chunk.push(character);
                chunk_len += 1;
            }
            current = chunk;
            current_len = chunk_len;
            continue;
        }
        current.push_str(word);
        current_len += word_len;
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (n & 0x7f) as u8;
            n >>= 7;
            if n > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if n == 0 {
                return out;
            }
        }
    }
    fn field(number: u16, kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = var(u64::from((number << 4) | u16::from(kind)));
        if kind == 12 {
            out.extend((payload.len() as u32).to_le_bytes());
        }
        out.extend(payload);
        out
    }
    fn id(author: u16, counter: u64) -> Vec<u8> {
        let mut out = var(u64::from(author));
        out.extend(var(counter));
        out
    }
    fn record(item: (u16, u64), left: (u16, u64), gap: u32, text: Option<&str>) -> Vec<u8> {
        let mut body = field(2, 15, &id(item.0, item.1));
        body.extend(field(3, 15, &id(left.0, left.1)));
        body.extend(field(4, 15, &id(0, 0)));
        body.extend(field(5, 4, &gap.to_le_bytes()));
        if let Some(text) = text {
            let mut value = var(text.len() as u64);
            value.push(1);
            value.extend(text.as_bytes());
            body.extend(field(6, 12, &value));
        }
        let mut out = var(0x0c);
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }
    fn style_entry(key: (u16, u64), legacy: u8) -> Vec<u8> {
        let mut out = id(key.0, key.1);
        out.extend(field(1, 15, &id(1, 7)));
        out.extend(field(2, 12, &field(1, 1, &[legacy])));
        out
    }
    /// Build a RootText payload from records and style entries.
    fn root_text(records: &[Vec<u8>], styles: &[Vec<u8>], width: f32) -> Vec<u8> {
        let mut array = var(records.len() as u64);
        for record in records {
            array.extend(record);
        }
        let mut map = var(styles.len() as u64);
        for entry in styles {
            map.extend(entry);
        }
        let mut container = field(1, 12, &field(1, 12, &array));
        container.extend(field(2, 12, &field(1, 12, &map)));
        let mut payload = field(1, 15, &id(0, 0));
        payload.extend(field(2, 12, &container));
        let mut position = (-468.0f64).to_le_bytes().to_vec();
        position.extend(234.0f64.to_le_bytes());
        payload.extend(field(3, 12, &position));
        payload.extend(field(4, 4, &width.to_le_bytes()));
        payload
    }

    #[test]
    fn orders_a_split_insertion_by_left_origin() {
        // "Hello" typed, then " world" inserted after its last character.
        let bytes = root_text(
            &[
                record((1, 20), (0, 0), 0, Some("Hello")),
                record((1, 30), (1, 24), 0, Some(" world")),
            ],
            &[],
            936.0,
        );
        let parsed = parse_root_text(&bytes).unwrap();
        assert_eq!(parsed.text, "Hello world");
        assert_eq!(parsed.ids.len(), 11);
        assert_eq!(parsed.ids[0], (1, 20));
        assert_eq!(parsed.ids[5], (1, 30));
        assert_eq!(parsed.position, (-468.0, 234.0));
        assert_eq!(parsed.width, 936.0);
    }

    #[test]
    fn deleted_runs_reserve_identity_space_without_text() {
        // A five-character gap sits between two runs; nothing of it renders,
        // but the following run still anchors on its last identity.
        let bytes = root_text(
            &[
                record((1, 10), (0, 0), 0, Some("ab")),
                record((1, 20), (1, 11), 5, None),
                record((1, 40), (1, 24), 0, Some("cd")),
            ],
            &[],
            936.0,
        );
        let parsed = parse_root_text(&bytes).unwrap();
        assert_eq!(parsed.text, "abcd");
        assert_eq!(parsed.ids, vec![(1, 10), (1, 11), (1, 40), (1, 41)]);
        // A deleted identity resolves to the character it now precedes, as
        // an anchor on a deleted character does in the firmware.
        let visible = parsed
            .ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect::<HashMap<_, _>>();
        assert_eq!(parsed.index_of((1, 41), &visible), Some(3));
        assert_eq!(parsed.index_of((1, 22), &visible), Some(2));
        assert_eq!(parsed.index_of((1, 25), &visible), None);
        assert_eq!(parsed.index_of((2, 22), &visible), None);
    }

    #[test]
    fn reads_every_named_legacy_paragraph_style() {
        for (legacy, kind, parameters) in [
            (0u8, 1u8, 0u32),
            (1, 2, 0),
            (2, 2, 1),
            (3, 2, 2),
            (4, 3, 0),
            (5, 3, 1),
            (6, 3, 0x10),
            (7, 3, 0x110),
            (8, 3, 0x11),
            (9, 3, 0x111),
            (10, 3, 0x20),
            (11, 3, 0x21),
            (12, 4, 0),
            (200, 1, 0),
        ] {
            let style = ParagraphStyle::from_wire(legacy, 0, 0);
            assert_eq!(
                (style.kind, style.parameters),
                (kind, parameters),
                "{legacy}"
            );
        }
        // A nonzero newer kind takes precedence over the legacy byte.
        let newer = ParagraphStyle::from_wire(2, 3, 0x111);
        assert_eq!((newer.kind, newer.parameters), (3, 0x111));
        assert!(newer.is_list());
        assert_eq!(newer.list_subtype(), 1);
        assert_eq!(newer.check_state(), 1);
        assert_eq!(newer.indent_level(), 1);
    }

    #[test]
    fn styles_apply_to_the_paragraph_they_key() {
        // A style is keyed by the identity the paragraph follows: (0, 0) for
        // the first, the preceding newline's identity for the second. Here
        // "Title\n" occupies (1, 10)..(1, 15), so the newline is (1, 15).
        let bytes = root_text(
            &[
                record((1, 10), (0, 0), 0, Some("Title\n")),
                record((1, 20), (1, 15), 0, Some("item")),
            ],
            &[style_entry((0, 0), 2), style_entry((1, 15), 4)],
            936.0,
        );
        let parsed = parse_root_text(&bytes).unwrap();
        assert_eq!(parsed.text, "Title\nitem");
        let laid = layout(&parsed, 702.0).unwrap();
        assert_eq!(laid.lines.len(), 2);
        assert!(laid.lines[0].bold);
        assert!(laid.lines[0].font_size > BODY_FONT_SIZE);
        assert_eq!(laid.lines[1].marker.as_deref(), Some("\u{2022}"));
        // The bullet paragraph is indented; the title is not.
        assert!(laid.lines[1].x > laid.lines[0].x);
    }

    #[test]
    fn checkbox_and_numbered_markers_follow_their_parameters() {
        let unchecked = ParagraphStyle::from_wire(6, 0, 0);
        let checked = ParagraphStyle::from_wire(7, 0, 0);
        let numbered = ParagraphStyle::from_wire(10, 0, 0);
        assert_eq!(paragraph_marker(unchecked, 1).as_deref(), Some("\u{2610}"));
        assert_eq!(paragraph_marker(checked, 1).as_deref(), Some("\u{2611}"));
        assert_eq!(paragraph_marker(numbered, 3).as_deref(), Some("3."));
        assert_eq!(paragraph_marker(ParagraphStyle::default(), 1), None);
    }

    #[test]
    fn wraps_long_paragraphs_and_hard_breaks_long_words() {
        assert_eq!(wrap("one two three", 8), vec!["one two ", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("short", 40), vec!["short"]);
    }

    #[test]
    fn registered_width_beats_the_legacy_width() {
        let mut bytes = root_text(&[record((1, 10), (0, 0), 0, Some("x"))], &[], 936.0);
        let register = field(1, 15, &id(1, 3))
            .into_iter()
            .chain(field(2, 4, &1152.0f32.to_le_bytes()))
            .collect::<Vec<_>>();
        bytes.extend(field(5, 12, &field(1, 12, &register)));
        assert_eq!(parse_root_text(&bytes).unwrap().width, 1152.0);
    }

    #[test]
    fn rejects_malformed_text_payloads() {
        let good = root_text(&[record((1, 10), (0, 0), 0, Some("hi"))], &[], 936.0);
        assert!(parse_root_text(&good).is_ok());
        // Every truncation of the record array is rejected rather than
        // silently producing partial text.
        let mut array = var(1);
        array.extend(record((1, 10), (0, 0), 0, Some("hi")));
        assert!(parse_records(&array).is_ok());
        for cut in 1..array.len() {
            assert!(
                parse_records(&array[..cut]).is_err(),
                "prefix of {cut} array bytes parsed"
            );
        }
        // Trailing bytes after the last record are not ignored.
        let mut trailing = array.clone();
        trailing.push(0);
        assert!(parse_records(&trailing).is_err());
        // A record count larger than the stored records is rejected.
        let mut lying = var(9);
        lying.extend(record((1, 10), (0, 0), 0, Some("hi")));
        assert!(parse_records(&lying).is_err());
        // Invalid UTF-8 under marker 1 is rejected rather than replaced.
        let mut value = var(2);
        value.push(1);
        value.extend([0xff, 0xfe]);
        let mut body = field(2, 15, &id(1, 1));
        body.extend(field(6, 12, &value));
        let mut bad = var(1);
        bad.extend(var(0x0c));
        bad.extend((body.len() as u32).to_le_bytes());
        bad.extend(body);
        assert!(parse_records(&bad).is_err());
    }

    #[test]
    fn a_cyclic_anchor_chain_cannot_loop_forever() {
        // Two records anchored on each other never reach the (0, 0) root, so
        // neither is emitted and the walk still terminates.
        let bytes = root_text(
            &[
                record((1, 10), (1, 20), 0, Some("a")),
                record((1, 20), (1, 10), 0, Some("b")),
            ],
            &[],
            936.0,
        );
        assert_eq!(parse_root_text(&bytes).unwrap().text, "");
    }

    #[test]
    fn layout_places_lines_on_the_approximate_grid() {
        let bytes = root_text(&[record((1, 10), (0, 0), 0, Some("one\ntwo"))], &[], 936.0);
        let parsed = parse_root_text(&bytes).unwrap();
        let laid = layout(&parsed, 702.0).unwrap();
        assert_eq!(laid.lines.len(), 2);
        // The text box origin plus Qt's document margin.
        assert_eq!(laid.lines[0].x, 234.0 + DOCUMENT_MARGIN);
        // Each paragraph is a block spaced by the body top margin, 0.75 x 32.
        assert_eq!(
            laid.lines[1].baseline - laid.lines[0].baseline,
            LINE_HEIGHT + 24.0
        );
        assert_eq!(
            laid.lines[0].baseline,
            parsed.position.1 + DOCUMENT_MARGIN + 24.0 + LINE_HEIGHT * 0.75
        );
        assert_eq!(laid.lines[0].font_size, BODY_FONT_SIZE);
    }

    #[test]
    fn wrapped_lines_of_one_paragraph_share_its_single_top_margin() {
        let long = "word ".repeat(40);
        let bytes = root_text(&[record((1, 10), (0, 0), 0, Some(&long))], &[], 936.0);
        let laid = layout(&parse_root_text(&bytes).unwrap(), 702.0).unwrap();
        assert!(laid.lines.len() > 1);
        assert_eq!(laid.lines[1].baseline - laid.lines[0].baseline, LINE_HEIGHT);
    }

    #[test]
    fn heading_paragraphs_use_the_firmware_heading_margins() {
        let style = |legacy| ParagraphStyle::from_wire(legacy, 0, 0);
        assert_eq!(style(1).top_margin(), 24.0);
        assert_eq!(style(2).top_margin(), 88.0);
        assert_eq!(style(3).top_margin(), 56.0);
        assert_eq!(ParagraphStyle::from_wire(0, 2, 3).top_margin(), 40.0);
        assert_eq!(style(4).top_margin(), 24.0);
    }

    /// A record holding an empty value with the trailing `2:4` formatting code.
    fn format_marker(item: (u16, u64), left: (u16, u64), code: u32) -> Vec<u8> {
        let mut body = field(2, 15, &id(item.0, item.1));
        body.extend(field(3, 15, &id(left.0, left.1)));
        body.extend(field(4, 15, &id(0, 0)));
        body.extend(field(5, 4, &0u32.to_le_bytes()));
        let mut value = vec![0, 1];
        value.extend(field(2, 4, &code.to_le_bytes()));
        body.extend(field(6, 12, &value));
        let mut out = var(0x0c);
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    #[test]
    fn a_formatting_marker_occupies_one_identity_that_later_text_anchors_on() {
        // A word bracketed by codes 1 and 2, as in copied text: the run
        // after the closing marker anchors on the marker's own identity.
        let bytes = root_text(
            &[
                record((1, 10), (0, 0), 0, Some("a ")),
                format_marker((1, 20), (1, 11), 1),
                record((1, 12), (1, 11), 0, Some("bold")),
                format_marker((1, 21), (1, 15), 2),
                record((1, 22), (1, 21), 0, Some(" c")),
            ],
            &[],
            936.0,
        );
        let parsed = parse_root_text(&bytes).unwrap();
        assert_eq!(parsed.text, "a bold c");
        assert_eq!(parsed.ids.len(), parsed.text.chars().count());
        assert_eq!(parsed.ids[2..6], [(1, 12), (1, 13), (1, 14), (1, 15)]);
        assert_eq!(parsed.ids[6], (1, 22));
    }

    #[test]
    fn a_character_position_is_its_line_start_plus_advances_and_the_baseline() {
        let bytes = root_text(&[record((1, 10), (0, 0), 0, Some("ab\n\ncd"))], &[], 936.0);
        let laid = layout(&parse_root_text(&bytes).unwrap(), 702.0).unwrap();
        let advance = BODY_FONT_SIZE * MEAN_ADVANCE;
        let (x, y) = char_position(&laid, 1).unwrap();
        assert_eq!((x, y), (laid.lines[0].x + advance, laid.lines[0].baseline));
        // A paragraph's closing newline belongs to its last line, so an empty
        // paragraph is found by its newline.
        assert_eq!(char_position(&laid, 2).unwrap().1, laid.lines[0].baseline);
        assert_eq!(char_position(&laid, 3).unwrap().1, laid.lines[1].baseline);
        assert_eq!(char_position(&laid, 5).unwrap().1, laid.lines[2].baseline);
        assert!(
            char_position(&laid, 99).is_some(),
            "clamped to the last line"
        );
    }
}
