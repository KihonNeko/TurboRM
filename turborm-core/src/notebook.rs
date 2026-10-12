//! Bounded, independent reader for reMarkable v6 scene files and page maps.
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use uuid::Uuid;

const HEADER: &[u8] = b"reMarkable .lines file, version=6          ";
const MAX_SCENE: usize = 100 * 1024 * 1024;
const MAX_BLOCKS: usize = 1_000_000;
const MAX_POINTS: usize = 250_000;
const MAX_FIELDS: usize = 4_096;
const MAX_SVG: usize = 32 * 1024 * 1024;
const MAX_PAGES: usize = 100_000;
const WIDTH: f32 = 1404.0;
const HEIGHT: f32 = 1872.0;
const MAX_CANVAS_DIMENSION: f32 = 1_000_000.0;
/// Largest inserted image file this reader embeds.
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// Most inserted image data one page embeds, summed over its image items.
pub const MAX_PAGE_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_ENTRIES: u64 = 4_096;
const MAX_IMAGE_DIMENSION: u32 = 16_384;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanvasBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SceneGeometry {
    pub paper_size: Option<(f32, f32)>,
    /// Persisted scene extent in centered scene coordinates. This is not the
    /// PDF background placement rectangle.
    pub scene_rect: Option<CanvasBounds>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedPage {
    pub svg: String,
    /// Rendering notes and warnings. Entries beginning with [`NOTE_PREFIX`] are
    /// informational (an approximation or a standing fact about this reader);
    /// every other entry is a content problem worth the reader's attention.
    /// Use [`severity`] rather than matching the prefix by hand.
    pub warnings: Vec<String>,
    pub bounds: CanvasBounds,
}

/// Marker prefix distinguishing an informational rendering note from a
/// content problem. Presentation layers strip it before display.
pub const NOTE_PREFIX: &str = "note: ";

/// Body of the note saying the page template is not part of the scene SVG.
/// A caller that composes the template itself removes this entry.
pub const TEMPLATE_OUTSIDE_SCENE: &str =
    "Page templates are supplied outside the scene and are not included in this SVG";

/// How a rendering note should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// An approximation or a standing fact about this reader: nothing is
    /// missing from the page and the user needs to take no action.
    Note,
    /// Something in the file could not be rendered faithfully.
    Warning,
}

/// A rendering note split into its severity and its display text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderNote {
    pub severity: Severity,
    pub text: String,
}

/// Classify one raw entry of [`RenderedPage::warnings`].
pub fn severity(warning: &str) -> RenderNote {
    match warning.strip_prefix(NOTE_PREFIX) {
        Some(text) => RenderNote {
            severity: Severity::Note,
            text: text.to_owned(),
        },
        None => RenderNote {
            severity: Severity::Warning,
            text: warning.to_owned(),
        },
    }
}

/// Build an informational note entry.
fn note(text: impl std::fmt::Display) -> String {
    format!("{NOTE_PREFIX}{text}")
}

/// Collapse identical entries, keeping first-seen order and appending a
/// `(×N)` multiplicity to anything that occurred more than once.
///
/// A page with forty groups that share one ordering ambiguity is one fact
/// about the page, not forty separate problems.
pub fn collapse_notes(warnings: Vec<String>) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for warning in warnings {
        if counts.contains_key(&warning) {
            *counts.get_mut(&warning).expect("counted entry") += 1;
        } else {
            counts.insert(warning.clone(), 1);
            order.push(warning);
        }
    }
    order
        .into_iter()
        .map(|warning| match counts[&warning] {
            1 => warning,
            n => format!("{warning} (×{n})"),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub id: String,
    /// Zero-based page in the document PDF, when this notebook page redirects to one.
    pub background: Option<usize>,
}

#[derive(Clone)]
struct Field<'a> {
    number: u16,
    kind: u8,
    bytes: &'a [u8],
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| anyhow!("scene offset overflow"))?;
        ensure!(end <= self.bytes.len(), "truncated scene field");
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
                "scene integer overflow"
            );
            out |= part << shift;
            ensure!(bits == 64 || out < (1u64 << bits), "scene integer overflow");
            if byte & 0x80 == 0 {
                return Ok(out);
            }
        }
        bail!("unterminated scene integer")
    }
    fn fields(&mut self) -> Result<Vec<Field<'a>>> {
        let mut result = Vec::new();
        while self.at < self.bytes.len() {
            ensure!(result.len() < MAX_FIELDS, "too many fields in scene block");
            let tag = self.var(16)? as u16;
            ensure!(tag >> 12 == 0, "scene field tag outside supported range");
            let (number, kind) = (tag >> 4, (tag & 15) as u8);
            let bytes = match kind {
                1 | 4 | 8 => self.take(kind as usize)?,
                12 => {
                    let n = le_u32(self.take(4)?) as usize;
                    self.take(n)?
                }
                15 => {
                    let start = self.at;
                    self.var(16)?;
                    self.var(48)?;
                    &self.bytes[start..self.at]
                }
                _ => bail!("unsupported scene wire type {kind}"),
            };
            result.push(Field {
                number,
                kind,
                bytes,
            });
        }
        Ok(result)
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().expect("checked field width"))
}
fn f32le(b: &[u8]) -> f32 {
    f32::from_le_bytes(b.try_into().expect("checked field width"))
}
fn f64le(b: &[u8]) -> f64 {
    f64::from_le_bytes(b.try_into().expect("checked field width"))
}
fn id_key(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(bytes);
    r.var(16)?;
    r.var(48)?;
    ensure!(r.at == bytes.len(), "invalid scene id");
    Ok(bytes.to_vec())
}

fn decoded_id(bytes: &[u8]) -> Result<(u16, u64)> {
    let mut r = Reader::new(bytes);
    let author = r.var(16)? as u16;
    let counter = r.var(48)?;
    ensure!(r.at == bytes.len(), "invalid scene id");
    Ok((author, counter))
}

fn scene_authors(bytes: &[u8]) -> Result<HashMap<u16, [u8; 16]>> {
    let mut at = HEADER.len();
    let mut authors = HashMap::new();
    let mut uuids = HashSet::new();
    while at < bytes.len() {
        ensure!(bytes.len() - at >= 8, "truncated scene block header");
        let len = le_u32(&bytes[at..at + 4]) as usize;
        let extra = bytes[at + 4] as usize;
        let minimum = bytes[at + 5];
        let tag = bytes[at + 7];
        let start = at
            .checked_add(8 + extra)
            .ok_or_else(|| anyhow!("scene block overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| anyhow!("scene block overflow"))?;
        ensure!(end <= bytes.len(), "truncated scene block");
        // A table that needs a newer reader is skipped exactly as
        // `scene_blocks` skips it; its authors then stay unmapped.
        if tag == 9 && minimum <= supported_schema(tag) {
            let mut table = Reader::new(&bytes[start..end]);
            let count = table.var(64)?;
            ensure!(count <= u64::from(u16::MAX), "too many scene authors");
            for _ in 0..count {
                ensure!(table.var(16)? == 12, "invalid scene author entry tag");
                let nested_len = le_u32(table.take(4)?) as usize;
                let mut entry = Reader::new(table.take(nested_len)?);
                ensure!(entry.var(16)? == 16, "invalid scene author UUID length");
                let uuid: [u8; 16] = entry.take(16)?.try_into().expect("checked UUID width");
                let index =
                    u16::from_le_bytes(entry.take(2)?.try_into().expect("checked index width"));
                ensure!(
                    entry.at == entry.bytes.len(),
                    "scene author entry has trailing data"
                );
                ensure!(
                    authors.insert(index, uuid).is_none(),
                    "duplicate scene author index"
                );
                ensure!(uuids.insert(uuid), "duplicate scene author UUID");
            }
            ensure!(
                table.at == table.bytes.len(),
                "scene author table has trailing data"
            );
        }
        at = end;
    }
    Ok(authors)
}

/// `(tag, version, parsed fields, raw payload)` for each scene block.
type SceneBlock<'a> = (u8, u8, Vec<Field<'a>>, &'a [u8]);

/// The newest block schema this reader understands for `tag`.
///
/// Byte 5 of a block header is the **minimum** reader version the block
/// requires and byte 6 its **current** version. The firmware reads a block
/// when the current version is within what it supports, still reads it when
/// only the minimum is (a forward-compatible extension), and otherwise skips
/// it with "Lines file is created with a higher version than supported"
/// (Xochitl `0xe87cc4..0xe87d90`, `0xe88f08`). This reader applies the same
/// rule with its own per-tag limits: the v3 `SceneImagesTable` is decoded, and
/// every other tag is read up to schema 2 as before.
fn supported_schema(tag: u8) -> u8 {
    match tag {
        14 => 3,
        _ => 2,
    }
}

/// The blocks of a v6 scene, and those skipped because they need a newer reader.
struct SceneBlocks<'a> {
    blocks: Vec<SceneBlock<'a>>,
    /// `(tag, minimum, version)` of each block whose minimum reader version is
    /// newer than [`supported_schema`].
    skipped: Vec<(u8, u8, u8)>,
}

fn scene_blocks(bytes: &[u8]) -> Result<SceneBlocks<'_>> {
    ensure!(bytes.len() <= MAX_SCENE, "scene exceeds 100 MiB limit");
    ensure!(bytes.len() >= HEADER.len(), "truncated scene header");
    ensure!(
        bytes.starts_with(HEADER),
        "unsupported scene version (expected v6)"
    );
    let mut at = HEADER.len();
    let mut blocks = Vec::new();
    let mut skipped = Vec::new();
    while at < bytes.len() {
        ensure!(
            blocks.len() + skipped.len() < MAX_BLOCKS,
            "too many scene blocks"
        );
        ensure!(bytes.len() - at >= 8, "truncated scene block header");
        let len = le_u32(&bytes[at..at + 4]) as usize;
        let extra = bytes[at + 4] as usize;
        let minimum = bytes[at + 5];
        let version = bytes[at + 6];
        let tag = bytes[at + 7];
        let start = at
            .checked_add(8 + extra)
            .ok_or_else(|| anyhow!("scene block overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| anyhow!("scene block overflow"))?;
        ensure!(end <= bytes.len(), "truncated scene block");
        at = end;
        if minimum > supported_schema(tag) {
            // A newer writer declared that older readers cannot interpret
            // this block. Skip it rather than guess at its payload; the
            // rest of the page is still drawn and the gap is reported.
            skipped.push((tag, minimum, version));
            continue;
        }
        let fields = if matches!(tag, 2 | 3 | 4 | 5 | 6 | 7 | 8 | 13 | 14 | 15) {
            Reader::new(&bytes[start..end])
                .fields()
                .with_context(|| format!("invalid block tag {tag}"))?
        } else {
            Vec::new()
        };
        blocks.push((tag, version, fields, &bytes[start..end]));
    }
    Ok(SceneBlocks { blocks, skipped })
}

/// One content warning for every block skipped by [`scene_blocks`].
fn skipped_blocks_warning(skipped: &[(u8, u8, u8)]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let mut kinds = Vec::<(u8, u8, u8)>::new();
    for &kind in skipped {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    let kinds = kinds
        .iter()
        .map(|(tag, minimum, version)| format!("tag {tag} schema {minimum}/{version}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(match skipped.len() {
        1 => format!("1 scene block uses a newer format and was not drawn ({kinds})"),
        n => format!("{n} scene blocks use a newer format and were not drawn ({kinds})"),
    })
}

fn checked_geometry_number(value: f64, dimension: bool, name: &str) -> Result<f32> {
    ensure!(value.is_finite(), "non-finite scene {name}");
    if dimension {
        ensure!(value > 0.0, "scene {name} must be positive");
        ensure!(
            value <= f64::from(MAX_CANVAS_DIMENSION),
            "scene {name} exceeds limit"
        );
    } else {
        ensure!(
            value.abs() <= f64::from(MAX_CANVAS_DIMENSION),
            "scene {name} exceeds limit"
        );
    }
    Ok(value as f32)
}

fn geometry_from_blocks(blocks: &[SceneBlock<'_>]) -> Result<SceneGeometry> {
    let mut geometry = SceneGeometry::default();
    for (tag, _, fields, _) in blocks {
        if *tag != 13 {
            continue;
        }
        let fallback = fields.iter().find(|f| f.number == 5 && f.kind == 12);
        let preferred = fields.iter().find(|f| f.number == 7 && f.kind == 12);
        geometry.paper_size = if let Some(field) = preferred {
            ensure!(field.bytes.len() == 16, "invalid SceneInfo paper size");
            Some((
                checked_geometry_number(f64le(&field.bytes[..8]), true, "paper width")?,
                checked_geometry_number(f64le(&field.bytes[8..]), true, "paper height")?,
            ))
        } else if let Some(field) = fallback {
            ensure!(
                field.bytes.len() == 8,
                "invalid legacy SceneInfo paper size"
            );
            Some((
                checked_geometry_number(f64::from(le_u32(&field.bytes[..4])), true, "paper width")?,
                checked_geometry_number(
                    f64::from(le_u32(&field.bytes[4..])),
                    true,
                    "paper height",
                )?,
            ))
        } else {
            None
        };
        geometry.scene_rect = None;
        if let Some(field) = fields.iter().find(|f| f.number == 6 && f.kind == 12) {
            let nested = Reader::new(field.bytes).fields()?;
            if let Some(rect) = nested.iter().find(|f| f.number == 2 && f.kind == 12) {
                ensure!(rect.bytes.len() == 32, "invalid SceneInfo scene rectangle");
                // QRectF's default null rectangle means that no extent is
                // saved. It is present on ordinary notebook pages as well.
                if rect
                    .bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .all(|part| f64le(part) == 0.0)
                {
                    continue;
                }
                geometry.scene_rect = Some(CanvasBounds {
                    x: checked_geometry_number(f64le(&rect.bytes[..8]), false, "extent x")?,
                    y: checked_geometry_number(f64le(&rect.bytes[8..16]), false, "extent y")?,
                    width: checked_geometry_number(
                        f64le(&rect.bytes[16..24]),
                        true,
                        "extent width",
                    )?,
                    height: checked_geometry_number(
                        f64le(&rect.bytes[24..]),
                        true,
                        "extent height",
                    )?,
                });
            }
        }
    }
    Ok(geometry)
}

/// Read the nominal paper size and optional persisted scene extent.
pub fn scene_geometry(bytes: &[u8]) -> Result<SceneGeometry> {
    if crate::legacy_notebook::version(bytes).is_some() {
        return crate::legacy_notebook::geometry(bytes);
    }
    geometry_from_blocks(&scene_blocks(bytes)?.blocks)
}

fn xml_num(value: f32) -> String {
    let mut s = format!("{value:.3}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn color(index: u32, explicit: Option<u32>) -> &'static str {
    if index == 9 {
        return "#000000";
    } // emitted separately for explicit ARGB
    match index {
        0 => "#111111",
        1 => "#777777",
        2 => "#ffffff",
        3 | 13 => "#f5dd42",
        4 | 10 => "#55a868",
        5 | 12 => "#e78ac3",
        6 | 14 => "#377eb8",
        7 => "#d94841",
        8 => "#999999",
        11 => "#30bfc9",
        15 => "#ee8a32",
        _ => {
            let _ = explicit;
            "#111111"
        }
    }
}

fn xml_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters are not representable in XML 1.0 text.
            c if (c as u32) < 0x20 && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Render a laid-out RootText block as SVG `<text>` runs.
///
/// Layout metrics are approximate; the caller reports that in a warning.
fn text_svg(laid: &crate::scene_text::TextLayout) -> Result<(String, CanvasBounds)> {
    use crate::scene_text as text;
    let mut svg = String::new();
    for line in &laid.lines {
        if let Some(marker) = &line.marker {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\" fill=\"#111111\" xml:space=\"preserve\">{}</text>\n",
                xml_num(line.x - text::INDENT_STEP),
                xml_num(line.baseline),
                text::FONT_FAMILY,
                xml_num(line.font_size),
                xml_text(marker)
            ));
        }
        if line.text.is_empty() {
            continue;
        }
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" font-family=\"{}\" font-size=\"{}\"{} fill=\"#111111\" xml:space=\"preserve\">{}</text>\n",
            xml_num(line.x),
            xml_num(line.baseline),
            text::FONT_FAMILY,
            xml_num(line.font_size),
            if line.bold { " font-weight=\"bold\"" } else { "" },
            xml_text(&line.text)
        ));
        ensure!(svg.len() <= MAX_SVG, "rendered SVG exceeds 32 MiB limit");
    }
    let (x, y, w, h) = laid.bounds;
    Ok((
        svg,
        CanvasBounds {
            x,
            y,
            width: w,
            height: h,
        },
    ))
}

/// A decoded `CrdtGlyphItem` payload: a text highlight over typed text.
///
/// Subtype 1 dispatches to firmware reader `0xea6770`. Field 4 is the Color
/// enum, field 5 an encoded string and field 6 a counted array of QRectF
/// rectangles in scene coordinates.
struct GlyphItem {
    color: u32,
    rectangles: Vec<(f32, f32, f32, f32)>,
}

fn parse_glyph(payload: &[u8]) -> Result<Option<GlyphItem>> {
    let mut nested = Reader::new(payload);
    if nested.take(1)? != [1] {
        return Ok(None);
    }
    let fields = nested.fields()?;
    let color = fields
        .iter()
        .find(|f| f.number == 4 && f.kind == 4)
        .map_or(0, |f| le_u32(f.bytes));
    let mut rectangles = Vec::new();
    if let Some(field) = fields.iter().find(|f| f.number == 6 && f.kind == 12) {
        let mut cursor = Reader::new(field.bytes);
        let count = cursor.var(64)?;
        ensure!(count <= 4_096, "too many glyph rectangles");
        for _ in 0..count {
            let bytes = cursor.take(32)?;
            let value = |range: std::ops::Range<usize>| f64le(&bytes[range]);
            let (x, y, w, h) = (value(0..8), value(8..16), value(16..24), value(24..32));
            ensure!(
                x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite(),
                "non-finite glyph rectangle"
            );
            ensure!(
                x.abs() <= 1_000_000.0
                    && y.abs() <= 1_000_000.0
                    && (0.0..=1_000_000.0).contains(&w)
                    && (0.0..=1_000_000.0).contains(&h),
                "glyph rectangle exceeds page bounds"
            );
            rectangles.push((x as f32, y as f32, w as f32, h as f32));
        }
        ensure!(
            cursor.at == cursor.bytes.len(),
            "glyph rectangle array has trailing data"
        );
    }
    Ok(Some(GlyphItem { color, rectangles }))
}

/// An anchored `SceneTreeNode`: the node follows a typed-text character.
///
/// Firmware names the four fields `anchorId`, `anchorMode`, `anchorThreshold`
/// and `AnchorInitialOriginX`, in that order (7..10), in the same string run
/// that names the SceneInfo fields 1..9 in field order. The anchor manager
/// (`0xe5e460`) sets the node's origin to the position of the anchor
/// character: its x and the **baseline** of its line (`0xf0a250`). In
/// vertical mode 2 the x is replaced by the stored initial origin x, so the
/// node follows the text vertically only. The threshold is not used there.
#[derive(Debug, Clone, Copy)]
struct AnchorRecord {
    anchor: Option<(u16, u64)>,
    mode: u8,
    initial_x: Option<f32>,
}

/// Reserved anchor identities for the start and end of the typed text
/// (`0xe5e4dc..0xe5e514`): author 0 with counter 2^48-2 or 2^48-1.
const ANCHOR_TEXT_START: (u16, u64) = (0, (1 << 48) - 2);
const ANCHOR_TEXT_END: (u16, u64) = (0, (1 << 48) - 1);

fn anchor_register_id(fields: &[Field<'_>], number: u16) -> Result<Option<(u16, u64)>> {
    let Some(field) = fields.iter().find(|f| f.number == number && f.kind == 12) else {
        return Ok(None);
    };
    let inner = Reader::new(field.bytes).fields()?;
    inner
        .iter()
        .find(|f| f.number == 2 && f.kind == 15)
        .map(|f| decoded_id(f.bytes))
        .transpose()
}

/// A scene string: a varint byte length, an encoding byte (1 is UTF-8) and
/// the bytes, as in RootText values.
fn scene_string(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    let length = reader.var(32)? as usize;
    ensure!(reader.take(1)? == [1], "unsupported scene string encoding");
    let raw = reader.take(length)?;
    ensure!(reader.at == bytes.len(), "scene string has trailing data");
    Ok(std::str::from_utf8(raw)
        .map_err(|_| anyhow!("scene string is not valid UTF-8"))?
        .to_owned())
}

/// One `SceneImagesTable` entry: the image file an image reference names.
struct ImageEntry {
    /// LWW timestamp of the file-name register.
    timestamp: (u16, u64),
    name: String,
}

/// Decode a v3 `SceneImagesTable` (tag 14) into `table`.
///
/// Field 1 holds a varint count and that many entries, each written as a
/// field-0 length-delimited record (`0c`, `u32` length): the raw 16-byte
/// `QUuid` image reference, then field 1, an LWW register (`1`: timestamp
/// identity, `2`: scene string) holding the file name inside the page's
/// image directory, and field 2, an LWW register whose value record holds one
/// byte (`1:1`). That byte is 0 in every recovered entry and its meaning is
/// not established; it is not needed to draw. The empty table other pages
/// carry is the same record with a zero count.
fn parse_image_table(
    fields: &[Field<'_>],
    table: &mut HashMap<[u8; 16], ImageEntry>,
) -> Result<()> {
    let Some(field) = fields.iter().find(|f| f.number == 1 && f.kind == 12) else {
        return Ok(());
    };
    let mut cursor = Reader::new(field.bytes);
    let count = cursor.var(64)?;
    ensure!(count <= MAX_IMAGE_ENTRIES, "too many image table entries");
    for _ in 0..count {
        ensure!(cursor.var(16)? == 0x0c, "invalid image table entry tag");
        let length = le_u32(cursor.take(4)?) as usize;
        let mut entry = Reader::new(cursor.take(length)?);
        let key: [u8; 16] = entry.take(16)?.try_into().expect("checked key width");
        let fields = entry.fields()?;
        let Some(register) = fields.iter().find(|f| f.number == 1 && f.kind == 12) else {
            continue;
        };
        let register = Reader::new(register.bytes).fields()?;
        let timestamp = register
            .iter()
            .find(|f| f.number == 1 && f.kind == 15)
            .map(|f| decoded_id(f.bytes))
            .transpose()?
            .unwrap_or((0, 0));
        let Some(value) = register.iter().find(|f| f.number == 2 && f.kind == 12) else {
            continue;
        };
        let name = scene_string(value.bytes)?;
        // Last writer wins: the newer (counter, author) timestamp, and the
        // later block on a tie.
        let newer = table.get(&key).is_none_or(|existing| {
            (timestamp.1, timestamp.0) >= (existing.timestamp.1, existing.timestamp.0)
        });
        if newer {
            table.insert(key, ImageEntry { timestamp, name });
        }
    }
    ensure!(
        cursor.at == cursor.bytes.len(),
        "image table has trailing data"
    );
    Ok(())
}

/// Whether an image-table file name is safe to look up and of a drawable type.
///
/// Recovered names are `<uuid>.jpg`; anything with a path separator or an
/// unexpected character is refused rather than resolved.
pub fn valid_image_name(name: &str) -> bool {
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && ["png", "jpg", "jpeg"]
            .iter()
            .any(|allowed| extension.eq_ignore_ascii_case(allowed))
}

/// A decoded `CrdtImageItem` payload (subtype 7, firmware reader `0xeb40d0`).
///
/// Field 1 is the `imageRef` LWW register (value: the 16-byte `QUuid` key of
/// the image table), field 2 the `boundsTimestamp` identity, field 3 the
/// `vertices` (a varint count of `f32`s: four `(x, y, u, v)` vertices in
/// scene coordinates), field 4 the `indices` (a varint count of `u32`s: two
/// triangles) and an optional field 5 `moveId`. The firmware accepts only four
/// points and two triangles whose texture coordinates are the unit-square
/// corners and which form a rectangle with equal, parallel opposite sides and
/// right angles (`0xeac630`); the same rules are applied here. The rectangle
/// may be rotated, so it is kept as an origin and two edge vectors.
struct ImageItem {
    key: [u8; 16],
    /// Scene position of texture coordinate (0, 0).
    origin: (f32, f32),
    /// Edge from (0, 0) to (1, 0).
    across: (f32, f32),
    /// Edge from (0, 0) to (0, 1).
    down: (f32, f32),
}

impl ImageItem {
    fn extent(&self) -> CanvasBounds {
        let corners = [
            self.origin,
            (self.origin.0 + self.across.0, self.origin.1 + self.across.1),
            (self.origin.0 + self.down.0, self.origin.1 + self.down.1),
            (
                self.origin.0 + self.across.0 + self.down.0,
                self.origin.1 + self.across.1 + self.down.1,
            ),
        ];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        CanvasBounds {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }
}

/// Why an image item was not drawn. Each is a distinct content warning.
const IMAGE_UNREADABLE: &str =
    "An inserted image's placement could not be read, so it was not drawn";
const IMAGE_GEOMETRY: &str =
    "An inserted image has invalid placement geometry, so it was not drawn";
const IMAGE_UNLISTED: &str =
    "An inserted image is not listed in the page's image table, so it was not drawn";
const IMAGE_MISSING: &str = "An inserted image's file is not available, so it was not drawn";
const IMAGE_UNREADABLE_FILE: &str =
    "An inserted image's file could not be read, so it was not drawn";
const IMAGE_TOO_LARGE: &str = "An inserted image exceeds the 32 MiB limit, so it was not drawn";
const IMAGE_PAGE_BUDGET: &str = "Inserted images beyond the 64 MiB per-page limit were not drawn";
const IMAGE_FORMAT: &str = "An inserted image is not a PNG or JPEG file, so it was not drawn";
const IMAGE_NAME: &str =
    "An inserted image's file name is not a plain PNG or JPEG name, so it was not drawn";
const IMAGE_UNDECODABLE: &str = "An inserted image could not be decoded, so it was not drawn";

fn parse_image_item(payload: &[u8]) -> std::result::Result<ImageItem, &'static str> {
    let mut nested = Reader::new(payload);
    if nested.take(1).map_err(|_| IMAGE_UNREADABLE)? != [7] {
        return Err(IMAGE_UNREADABLE);
    }
    let fields = nested.fields().map_err(|_| IMAGE_UNREADABLE)?;
    let key = fields
        .iter()
        .find(|f| f.number == 1 && f.kind == 12)
        .and_then(|register| Reader::new(register.bytes).fields().ok())
        .and_then(|register| {
            register
                .iter()
                .find(|f| f.number == 2 && f.kind == 12)
                .and_then(|value| <[u8; 16]>::try_from(value.bytes).ok())
        })
        .ok_or(IMAGE_UNREADABLE)?;
    let counted = |number: u16, expected: u64| -> std::result::Result<Vec<[u8; 4]>, &'static str> {
        let field = fields
            .iter()
            .find(|f| f.number == number && f.kind == 12)
            .ok_or(IMAGE_UNREADABLE)?;
        let mut cursor = Reader::new(field.bytes);
        if cursor.var(64).map_err(|_| IMAGE_UNREADABLE)? != expected {
            return Err(IMAGE_GEOMETRY);
        }
        let values = cursor
            .take(expected as usize * 4)
            .map_err(|_| IMAGE_UNREADABLE)?
            .as_chunks::<4>()
            .0
            .to_vec();
        if cursor.at != cursor.bytes.len() {
            return Err(IMAGE_UNREADABLE);
        }
        Ok(values)
    };
    // "Expected 4 points and 2 triangles".
    let vertices = counted(3, 16)?
        .into_iter()
        .map(f32::from_le_bytes)
        .collect::<Vec<_>>();
    let indices = counted(4, 6)?
        .into_iter()
        .map(u32::from_le_bytes)
        .collect::<Vec<_>>();
    if vertices
        .iter()
        .any(|value| !value.is_finite() || value.abs() > MAX_CANVAS_DIMENSION)
    {
        return Err(IMAGE_GEOMETRY);
    }
    // "Points need to have text coordinates of (0, 0) (1, 0) (1, 1) and (0, 1)":
    // the third and fourth value of each vertex are texture coordinates.
    let mut corner = [None::<(f32, f32)>; 4];
    for point in vertices.as_chunks::<4>().0 {
        let slot = match (point[2], point[3]) {
            (0.0, 0.0) => 0,
            (1.0, 0.0) => 1,
            (1.0, 1.0) => 2,
            (0.0, 1.0) => 3,
            _ => return Err(IMAGE_GEOMETRY),
        };
        if corner[slot].replace((point[0], point[1])).is_some() {
            return Err(IMAGE_GEOMETRY);
        }
    }
    let [Some(p00), Some(p10), Some(p11), Some(p01)] = corner else {
        return Err(IMAGE_GEOMETRY);
    };
    // "Provided triangles need to form a rectangle": two triangles over the
    // four vertices that share exactly one diagonal.
    let texture = |index: u32| -> std::result::Result<(f32, f32), &'static str> {
        let point = vertices
            .as_chunks::<4>()
            .0
            .get(index as usize)
            .ok_or(IMAGE_GEOMETRY)?;
        Ok((point[2], point[3]))
    };
    let (first, second) = indices.split_at(3);
    let distinct = |triangle: &[u32]| {
        triangle[0] != triangle[1] && triangle[1] != triangle[2] && triangle[0] != triangle[2]
    };
    if !distinct(first) || !distinct(second) {
        return Err(IMAGE_GEOMETRY);
    }
    let shared = first
        .iter()
        .filter(|index| second.contains(index))
        .copied()
        .collect::<Vec<_>>();
    let covered = (0..4).all(|index| indices.contains(&index));
    if shared.len() != 2 || !covered {
        return Err(IMAGE_GEOMETRY);
    }
    let (a, b) = (texture(shared[0])?, texture(shared[1])?);
    if a.0 + b.0 != 1.0 || a.1 + b.1 != 1.0 {
        return Err(IMAGE_GEOMETRY);
    }
    let across = (p10.0 - p00.0, p10.1 - p00.1);
    let down = (p01.0 - p00.0, p01.1 - p00.1);
    let length = |v: (f32, f32)| v.0.hypot(v.1);
    let (width, height) = (length(across), length(down));
    if width <= 0.0 || height <= 0.0 {
        return Err(IMAGE_GEOMETRY);
    }
    // "Opposing sides are not equal and parallel" and "The rectangle does not
    // have right angles", with a tolerance for f32 rounding.
    let tolerance = 1e-3 * width.max(height).max(1.0);
    let far = (p11.0 - p01.0, p11.1 - p01.1);
    if length((far.0 - across.0, far.1 - across.1)) > tolerance
        || (across.0 * down.0 + across.1 * down.1).abs() > 1e-3 * width * height
    {
        return Err(IMAGE_GEOMETRY);
    }
    Ok(ImageItem {
        key,
        origin: p00,
        across,
        down,
    })
}

/// A validated, embeddable image file.
struct EmbeddedImage {
    /// Size of the source file, which the per-page budget counts.
    bytes: usize,
    mime: &'static str,
    width: u32,
    height: u32,
    base64: String,
}

/// Validate one inserted image file: PNG or JPEG, within the size limits and
/// fully decodable by the `image` crate.
fn embeddable_image(bytes: &[u8]) -> std::result::Result<EmbeddedImage, &'static str> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(IMAGE_TOO_LARGE);
    }
    let (format, mime) = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => (image::ImageFormat::Png, "image/png"),
        Ok(image::ImageFormat::Jpeg) => (image::ImageFormat::Jpeg, "image/jpeg"),
        _ => return Err(IMAGE_FORMAT),
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| IMAGE_UNDECODABLE)?;
    let (width, height) = (decoded.width(), decoded.height());
    if width == 0 || height == 0 {
        return Err(IMAGE_UNDECODABLE);
    }
    Ok(EmbeddedImage {
        bytes: bytes.len(),
        mime,
        width,
        height,
        base64: STANDARD.encode(bytes),
    })
}

/// Coefficient formatting for an image transform: finer than [`xml_num`],
/// since a scale such as 936/1024 is multiplied by the image's pixel size.
fn matrix_num(value: f32) -> String {
    let mut s = format!("{value:.6}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// The SVG `<image>` for one item, in the item's group coordinates.
fn image_element(item: &ImageItem, x_origin: f32, image: &EmbeddedImage) -> String {
    let href = format!("data:{};base64,{}", image.mime, image.base64);
    let (x, y) = (item.origin.0 + x_origin, item.origin.1);
    if item.across.1 == 0.0 && item.down.0 == 0.0 && item.across.0 > 0.0 && item.down.1 > 0.0 {
        format!(
            "<image x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" xlink:href=\"{href}\"/>\n",
            matrix_num(x),
            matrix_num(y),
            matrix_num(item.across.0),
            matrix_num(item.down.1)
        )
    } else {
        // A rotated or mirrored rectangle: map the image's pixel grid onto
        // the two stored edges.
        let (w, h) = (image.width as f32, image.height as f32);
        format!(
            "<image width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" transform=\"matrix({} {} {} {} {} {})\" xlink:href=\"{href}\"/>\n",
            image.width,
            image.height,
            matrix_num(item.across.0 / w),
            matrix_num(item.across.1 / w),
            matrix_num(item.down.0 / h),
            matrix_num(item.down.1 / h),
            matrix_num(x),
            matrix_num(y)
        )
    }
}

fn anchor_register_f32(fields: &[Field<'_>], number: u16) -> Result<Option<f32>> {
    let Some(field) = fields.iter().find(|f| f.number == number && f.kind == 12) else {
        return Ok(None);
    };
    let inner = Reader::new(field.bytes).fields()?;
    let Some(value) = inner.iter().find(|f| f.number == 2 && f.kind == 4) else {
        return Ok(None);
    };
    let value = f32le(value.bytes);
    ensure!(value.is_finite(), "non-finite anchor value");
    ensure!(value.abs() <= 1_000_000.0, "anchor value exceeds bounds");
    Ok(Some(value))
}

fn anchor_register_u8(fields: &[Field<'_>], number: u16) -> Result<Option<u8>> {
    let Some(field) = fields.iter().find(|f| f.number == number && f.kind == 12) else {
        return Ok(None);
    };
    let inner = Reader::new(field.bytes).fields()?;
    Ok(inner
        .iter()
        .find(|f| f.number == 2 && f.kind == 1)
        .map(|f| f.bytes[0]))
}

/// Cheap per-tool brush approximation.
///
/// The firmware's brush routines are not recovered, so each family is approximated with
/// a stroke opacity, a pressure-driven opacity range and an optional
/// multiply blend. Every tool that uses one still reports a warning.
#[derive(Clone, Copy)]
struct Brush {
    /// Base stroke opacity at full pressure.
    opacity: f32,
    /// How much of `opacity` a zero-pressure sample keeps. `1.0` disables the
    /// per-point variation and emits one opacity for the whole stroke.
    pressure_floor: f32,
    /// Multiply blending, so overlapping passes darken instead of replacing.
    multiply: bool,
    /// Extra stroke width as a fraction of the stored width.
    width_scale: f32,
}

fn brush_for(tool: u32) -> Brush {
    match tool {
        // Highlighter: translucent and multiplied, so crossings darken.
        5 | 18 => Brush {
            opacity: 0.35,
            pressure_floor: 1.0,
            multiply: true,
            width_scale: 1.0,
        },
        // Pencil and mechanical pencil: a grainy stroke approximated by
        // letting the recorded pressure drive per-segment opacity.
        1 | 7 | 13 | 14 => Brush {
            opacity: 0.85,
            pressure_floor: 0.35,
            multiply: true,
            width_scale: 1.0,
        },
        // Marker and shading marker: soft, slightly translucent and wide.
        // Close to solid: a whole stroke shares one opacity, so anything
        // lower turns a black marker title grey.
        3 | 16 | 23 => Brush {
            opacity: 0.95,
            pressure_floor: 0.85,
            multiply: true,
            width_scale: 1.1,
        },
        // Paintbrush and calligraphy: near opaque, with pressure-driven
        // weight. A light touch fades the stroke only a little: a whole
        // stroke shares one opacity, so a lower floor greys out a title.
        0 | 12 | 21 => Brush {
            opacity: 1.0,
            pressure_floor: 0.9,
            multiply: false,
            width_scale: 1.0,
        },
        // Ballpoint, fineliner, solid pen and everything else: solid ink.
        _ => Brush {
            opacity: 1.0,
            pressure_floor: 1.0,
            multiply: false,
            width_scale: 1.0,
        },
    }
}

#[derive(Debug)]
struct GroupPaint {
    item: Vec<u8>,
    left: Option<Vec<u8>>,
    right: Option<Vec<u8>>,
    content: GroupContent,
    erase: String,
    /// Painted extent of this item, when it draws anything. Two items whose
    /// extents do not intersect look identical in either paint order, so an
    /// unresolved tie between them changes nothing on screen.
    extent: Option<CanvasBounds>,
}

impl CanvasBounds {
    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

#[derive(Debug)]
enum GroupContent {
    Empty,
    Paint(String),
    /// An inserted image. Its data URI is bounded by the per-page image
    /// budget, so it does not count against the vector SVG limit.
    Image(String),
    Group(Vec<u8>),
}

fn optional_id(fields: &[Field<'_>], number: u16) -> Result<Option<Vec<u8>>> {
    fields
        .iter()
        .find(|f| f.number == number && f.kind == 15)
        .map(|f| id_key(f.bytes))
        .transpose()
}

/// Order a group's items by their left/right CRDT links.
///
/// Concurrent siblings that the links leave tied are ordered by author
/// identity, then by sequence counter: the same key the firmware's ordered-item
/// envelope stores (`crdt::Id` = author UUID index + counter, translated
/// through the file's `Uuid2Index` table).
///
/// Returns the paint order and whether any tie had to be left unresolved —
/// because the file's identity table is missing an author's UUID, or because
/// the left/right links form a cycle — *and* the tied items paint overlapping
/// extents. Non-overlapping items render identically in either order, so a tie
/// between them is not reported.
fn constrained_order(
    items: &[GroupPaint],
    authors: &HashMap<u16, [u8; 16]>,
) -> Result<(Vec<usize>, bool)> {
    let positions = items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.item.as_slice(), index))
        .collect::<HashMap<_, _>>();
    let mut edges = vec![Vec::<usize>::new(); items.len()];
    let mut edge_pairs = HashSet::new();
    let mut incoming = vec![0usize; items.len()];
    for (index, item) in items.iter().enumerate() {
        let constraints = item
            .left
            .as_deref()
            .and_then(|id| positions.get(id).copied())
            .map(|left| (left, index))
            .into_iter()
            .chain(
                item.right
                    .as_deref()
                    .and_then(|id| positions.get(id).copied())
                    .map(|right| (index, right)),
            );
        for (from, to) in constraints {
            if from != to && edge_pairs.insert((from, to)) {
                edges[from].push(to);
                incoming[to] += 1;
            }
        }
    }
    let mut ready = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, &degree)| (degree == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(items.len());
    let mut ambiguous = false;
    while order.len() < items.len() {
        if ready.is_empty() {
            // A cycle in the left/right links: no order satisfies them all.
            let emitted = order.iter().copied().collect::<HashSet<_>>();
            let rest = (0..items.len())
                .filter(|index| !emitted.contains(index))
                .collect::<BTreeSet<_>>();
            if overlapping_tie(items, &rest) {
                ambiguous = true;
            }
            order.extend(rest);
            break;
        }
        let fallback = *ready.first().expect("nonempty ready set");
        let index = if ready.len() > 1 {
            // Every remaining tie is genuine concurrency, and the firmware
            // resolves it with the same key: the author's 16-byte UUID
            // compared byte-lexicographically, then the 48-bit sequence
            // counter, both ascending. An unmapped author index sorts as the
            // all-zero UUID exactly as the firmware's map lookup does.
            let mut keys = Vec::with_capacity(ready.len());
            let mut complete = true;
            for &candidate in &ready {
                let (author, counter) = decoded_id(&items[candidate].item)?;
                let uuid = if author == 0 {
                    [0; 16]
                } else if let Some(uuid) = authors.get(&author) {
                    *uuid
                } else {
                    // The file's identity table has no entry for this index, so
                    // its UUID is unknown and the firmware's key cannot be
                    // reproduced. Fall back to stored order.
                    complete = false;
                    break;
                };
                keys.push(((uuid, counter), candidate));
            }
            if !complete && overlapping_tie(items, &ready) {
                ambiguous = true;
            }
            if complete {
                keys.into_iter()
                    .min_by_key(|(key, _)| *key)
                    .map_or(fallback, |(_, candidate)| candidate)
            } else {
                fallback
            }
        } else {
            fallback
        };
        ready.remove(&index);
        order.push(index);
        for &next in &edges[index] {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                ready.insert(next);
            }
        }
    }
    Ok((order, ambiguous))
}

/// True when at least two of `candidates` paint overlapping extents.
///
/// Items whose bounding boxes do not intersect produce the same image in
/// either paint order, so leaving their relative order unresolved is not a
/// rendering problem worth reporting.
fn overlapping_tie(items: &[GroupPaint], candidates: &BTreeSet<usize>) -> bool {
    let boxes = candidates
        .iter()
        .filter_map(|&index| items[index].extent)
        .collect::<Vec<_>>();
    boxes
        .iter()
        .enumerate()
        .any(|(i, a)| boxes[i + 1..].iter().any(|b| a.intersects(*b)))
}

fn append_limited(target: &mut String, value: &str, used: &mut usize) -> Result<()> {
    append_counted(target, value, 0, used)
}

/// Append `value`, counting all but `exempt` of its bytes against the SVG
/// limit. The exempt bytes are content already counted, or embedded image
/// data, which is bounded separately.
fn append_counted(target: &mut String, value: &str, exempt: usize, used: &mut usize) -> Result<()> {
    *used = used
        .checked_add(value.len().saturating_sub(exempt))
        .ok_or_else(|| anyhow!("rendered SVG size overflow"))?;
    ensure!(*used <= MAX_SVG, "rendered SVG exceeds 32 MiB limit");
    target.push_str(value);
    Ok(())
}

struct SceneEmission {
    bounds: CanvasBounds,
    definitions: String,
    warnings: Vec<String>,
    visiting: HashSet<Vec<u8>>,
    emitted: HashSet<Vec<u8>>,
    mask_number: usize,
    used: usize,
    authors: HashMap<u16, [u8; 16]>,
    /// Each anchored group's own translation in scene units.
    translations: HashMap<Vec<u8>, (f32, f32)>,
    /// Each reachable group's accumulated translation from the root.
    offsets: HashMap<Vec<u8>, (f32, f32)>,
}

/// Emit one group and its descendants. Returns the number of embedded image
/// bytes written to `body`, which the caller exempts from the SVG limit.
fn emit_group(
    group: &[u8],
    groups: &HashMap<Vec<u8>, Vec<GroupPaint>>,
    body: &mut String,
    state: &mut SceneEmission,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 1024, "scene group nesting exceeds limit");
    if !state.visiting.insert(group.to_vec()) {
        state
            .warnings
            .push("A cyclic group reference was not painted".into());
        return Ok(());
    }
    if !state.emitted.insert(group.to_vec()) {
        state
            .warnings
            .push("A multiply referenced group was painted only once".into());
        state.visiting.remove(group);
        return Ok(());
    }
    let Some(items) = groups.get(group) else {
        state.visiting.remove(group);
        return Ok(());
    };
    let (order, ambiguous) = constrained_order(items, &state.authors)?;
    if ambiguous && items.len() > 1 {
        // Only reached when the firmware's (author UUID, sequence) key could
        // not be applied and the tied items actually overlap on the page, so
        // the paint order here may genuinely differ from the device's.
        state.warnings.push(
            "Overlapping items in a group could not be ordered and use their stored order".into(),
        );
    }
    // Every byte of `paint` and `erase` is counted against the SVG limit as
    // it is added (embedded images are bounded separately), and each wrapper
    // below is counted as it is made. Counting the wrapped content again
    // charged each stroke once per level of nesting.
    //
    // The group's contents are in its own coordinates. An eraser mask is
    // therefore defined in those coordinates too: inside the group's anchor
    // translation, with its rectangle shifted to still cover the canvas.
    let offset = state.offsets.get(group).copied().unwrap_or((0.0, 0.0));
    let mut paint = String::new();
    let mut erase = String::new();
    for index in order {
        let item = &items[index];
        append_limited(&mut erase, &item.erase, &mut state.used)?;
        if matches!(item.content, GroupContent::Empty) {
            continue;
        }
        // An eraser hides only what was painted before it, as on the tablet:
        // the run of eraser strokes that just ended masks the paint so far,
        // and whatever is written over the erased place afterwards shows.
        if !erase.is_empty() {
            paint = mask_paint(paint, &std::mem::take(&mut erase), offset, state)?;
        }
        match &item.content {
            GroupContent::Empty => {}
            GroupContent::Paint(svg) => append_limited(&mut paint, svg, &mut state.used)?,
            GroupContent::Image(svg) => paint.push_str(svg),
            GroupContent::Group(child) => {
                emit_group(child, groups, &mut paint, state, depth + 1)?;
            }
        }
    }
    let masked = mask_paint(paint, &erase, offset, state)?;
    let content = masked.len();
    // An anchored node sits at the position of its typed-text anchor.
    let placed = match state.translations.get(group).copied() {
        Some((tx, ty)) if tx != 0.0 || ty != 0.0 => format!(
            "<g transform=\"translate({} {})\">\n{masked}</g>\n",
            xml_num(tx),
            xml_num(ty)
        ),
        _ => masked,
    };
    append_counted(body, &placed, content, &mut state.used)?;
    state.visiting.remove(group);
    Ok(())
}

/// `paint` with the eraser strokes `erase` cut out of it: a group masked by
/// the canvas in white with the strokes in black. `paint` and `erase` are
/// already counted against the SVG limit; only the new markup is added.
fn mask_paint(
    paint: String,
    erase: &str,
    offset: (f32, f32),
    state: &mut SceneEmission,
) -> Result<String> {
    // Erasing before anything is painted hides nothing.
    if erase.is_empty() || paint.is_empty() {
        return Ok(paint);
    }
    let mask = format!("eraser-{}", state.mask_number);
    state.mask_number += 1;
    let (x, y) = (state.bounds.x - offset.0, state.bounds.y - offset.1);
    let (width, height) = (state.bounds.width, state.bounds.height);
    let definition = format!(
        "<mask id=\"{mask}\" maskUnits=\"userSpaceOnUse\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"white\"/>\n{erase}</mask>\n",
        xml_num(x),
        xml_num(y),
        xml_num(width),
        xml_num(height),
        xml_num(x),
        xml_num(y),
        xml_num(width),
        xml_num(height)
    );
    append_counted(
        &mut state.definitions,
        &definition,
        erase.len(),
        &mut state.used,
    )?;
    let mut masked = String::new();
    append_counted(
        &mut masked,
        &format!("<g mask=\"url(#{mask})\">\n{paint}</g>\n"),
        paint.len(),
        &mut state.used,
    )?;
    Ok(masked)
}

/// Render the supported drawing subset as an SVG in the 1404×1872 page space.
pub fn render_svg(bytes: &[u8]) -> Result<RenderedPage> {
    render_svg_with_size(bytes, WIDTH, HEIGHT)
}

/// Render the supported drawing subset with the supplied document page size.
///
/// Inserted images are reported as unavailable; use
/// [`render_svg_with_images`] to supply their files.
pub fn render_svg_with_size(bytes: &[u8], width: f32, height: f32) -> Result<RenderedPage> {
    render_svg_with_images(bytes, width, height, &mut |_| Ok(None))
}

/// File names, inside the page's image directory `<document>/<page>/`, of the
/// inserted images this page draws.
///
/// Only names that pass [`valid_image_name`] and are referenced by the newest
/// envelope of an image item are returned, sorted and without duplicates.
/// Legacy scene versions have no inserted images.
pub fn image_files(bytes: &[u8]) -> Result<Vec<String>> {
    if crate::legacy_notebook::version(bytes).is_some() {
        return Ok(Vec::new());
    }
    let blocks = scene_blocks(bytes)?.blocks;
    let mut table = HashMap::new();
    let mut latest = HashMap::<Vec<u8>, usize>::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if *tag == 14 {
            // A table that cannot be read draws no images; rendering reports it.
            let _ = parse_image_table(fields, &mut table);
        }
        if matches!(tag, 3 | 4 | 5 | 6 | 8 | 15) {
            if let Some(f) = fields.iter().find(|f| f.number == 2 && f.kind == 15) {
                latest.insert(id_key(f.bytes)?, index);
            }
        }
    }
    let mut names = BTreeSet::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if *tag != 15 {
            continue;
        }
        let Some(item) = fields.iter().find(|f| f.number == 2 && f.kind == 15) else {
            continue;
        };
        if latest.get(&id_key(item.bytes)?) != Some(&index) {
            continue;
        }
        let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) else {
            continue;
        };
        if let Ok(item) = parse_image_item(payload.bytes) {
            if let Some(entry) = table.get(&item.key) {
                if valid_image_name(&entry.name) {
                    names.insert(entry.name.clone());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// Group references make the visible tree: which groups are reachable from
/// the root `(0, 1)`, and each group's child groups. A hidden group and
/// everything under it is unreachable.
type GroupTree = (HashSet<Vec<u8>>, HashMap<Vec<u8>, Vec<Vec<u8>>>);
fn group_tree(
    blocks: &[SceneBlock<'_>],
    latest: &HashMap<Vec<u8>, usize>,
    root: &[u8],
) -> Result<GroupTree> {
    let root = root.to_vec();
    let mut hidden = HashSet::new();
    for (tag, _, fields, _) in blocks {
        if *tag != 2 {
            continue;
        }
        let Some(node) = fields.iter().find(|f| f.number == 1 && f.kind == 15) else {
            continue;
        };
        let Some(register) = fields.iter().find(|f| f.number == 3 && f.kind == 12) else {
            continue;
        };
        let values = Reader::new(register.bytes).fields()?;
        if let Some(value) = values.iter().find(|f| f.number == 2 && f.kind == 1) {
            let node = id_key(node.bytes)?;
            if value.bytes == [0] {
                hidden.insert(node);
            } else {
                hidden.remove(&node);
            }
        }
    }
    let mut children = HashMap::<Vec<u8>, Vec<Vec<u8>>>::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if *tag != 4 {
            continue;
        }
        let Some(identity) = fields.iter().find(|f| f.number == 2 && f.kind == 15) else {
            continue;
        };
        if latest.get(&id_key(identity.bytes)?) != Some(&index) {
            continue;
        }
        let Some(parent) = fields.iter().find(|f| f.number == 1 && f.kind == 15) else {
            continue;
        };
        let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) else {
            continue;
        };
        let mut nested = Reader::new(payload.bytes);
        if nested.take(1)? != [2] {
            continue;
        }
        if let Some(child) = nested
            .fields()?
            .iter()
            .find(|f| f.number == 2 && f.kind == 15)
        {
            children
                .entry(id_key(parent.bytes)?)
                .or_default()
                .push(id_key(child.bytes)?);
        }
    }
    let mut reachable = HashSet::new();
    let mut queue = VecDeque::new();
    if !hidden.contains(&root) {
        reachable.insert(root.clone());
        queue.push_back(root.clone());
    }
    while let Some(parent) = queue.pop_front() {
        for child in children.get(&parent).into_iter().flatten() {
            if !hidden.contains(child) && reachable.insert(child.clone()) {
                queue.push_back(child.clone());
            }
        }
    }
    Ok((reachable, children))
}

/// One text highlight: the string it covers and its colour name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub text: String,
    /// See [`color_name`].
    pub color: String,
}

/// What a reader can quote from a v6 scene, without rendering it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SceneAnnotations {
    /// The string each live text highlight (`CrdtGlyphItem` field 5) covers,
    /// in reading order: by the top of its first rectangle, then its left.
    pub highlights: Vec<Highlight>,
    /// Non-empty paragraphs of the page's typed text, in document order.
    pub typed_text: Vec<String>,
    /// Live strokes in the visible tree by colour name, with a
    /// `highlighter ` or `shader ` prefix for those tools. Erasers and
    /// control strokes are not counted.
    pub stroke_colors: std::collections::BTreeMap<String, usize>,
    /// Items that could not be read. Never fatal.
    pub warnings: Vec<String>,
}

/// The name of a scene colour index, matching the colour the renderer paints
/// for it. Index 9 carries an explicit ARGB value, named after the nearest
/// palette colour when it is known.
pub fn color_name(index: u32, argb: Option<u32>) -> String {
    const PALETTE: [(&str, [i32; 3]); 10] = [
        ("black", [0x11, 0x11, 0x11]),
        ("gray", [0x77, 0x77, 0x77]),
        ("white", [0xff, 0xff, 0xff]),
        ("yellow", [0xf5, 0xdd, 0x42]),
        ("green", [0x55, 0xa8, 0x68]),
        ("pink", [0xe7, 0x8a, 0xc3]),
        ("blue", [0x37, 0x7e, 0xb8]),
        ("red", [0xd9, 0x48, 0x41]),
        ("cyan", [0x30, 0xbf, 0xc9]),
        ("orange", [0xee, 0x8a, 0x32]),
    ];
    let name = match index {
        0 => "black",
        1 | 8 => "gray",
        2 => "white",
        3 | 13 => "yellow",
        4 | 10 => "green",
        5 | 12 => "pink",
        6 | 14 => "blue",
        7 => "red",
        11 => "cyan",
        15 => "orange",
        9 => {
            let Some(argb) = argb else {
                return "custom".into();
            };
            let rgb = [(argb >> 16) & 255, (argb >> 8) & 255, argb & 255].map(|c| c as i32);
            let nearest = PALETTE
                .iter()
                .min_by_key(|(_, p)| (0..3).map(|i| (p[i] - rgb[i]).pow(2)).sum::<i32>())
                .map_or("custom", |(name, _)| name);
            return format!("{nearest} (#{:06x})", argb & 0xff_ffff);
        }
        _ => return format!("color {index}"),
    };
    name.into()
}

/// The highlighted strings and typed-text paragraphs of one scene.
///
/// A highlight counts only when its newest envelope still carries a payload,
/// so a highlight removed on the tablet is not reported. Legacy v3/v5 scenes
/// have neither and return an empty result.
pub fn scene_annotations(bytes: &[u8]) -> Result<SceneAnnotations> {
    let mut result = SceneAnnotations::default();
    if crate::legacy_notebook::version(bytes).is_some() {
        return Ok(result);
    }
    let blocks = scene_blocks(bytes)?.blocks;
    let mut latest = HashMap::<Vec<u8>, usize>::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if matches!(tag, 3 | 4 | 5 | 6 | 8 | 15) {
            if let Some(f) = fields.iter().find(|f| f.number == 2 && f.kind == 15) {
                latest.insert(id_key(f.bytes)?, index);
            }
        }
    }
    let root = id_key(&[0, 1])?;
    let (reachable, _) = group_tree(&blocks, &latest, &root)?;
    let mut highlights = Vec::new();
    for (index, (tag, _, fields, raw)) in blocks.iter().enumerate() {
        match tag {
            5 => {
                let live = fields
                    .iter()
                    .find(|f| f.number == 2 && f.kind == 15)
                    .map(|item| id_key(item.bytes))
                    .transpose()?
                    .is_some_and(|item| latest.get(&item) == Some(&index));
                let visible = fields
                    .iter()
                    .find(|f| f.number == 1 && f.kind == 15)
                    .map(|parent| id_key(parent.bytes))
                    .transpose()?
                    .is_some_and(|parent| reachable.contains(&parent));
                let payload = fields.iter().find(|f| f.number == 6 && f.kind == 12);
                if let (true, true, Some(payload)) = (live, visible, payload) {
                    if let Some(name) = stroke_color(payload.bytes) {
                        *result.stroke_colors.entry(name).or_default() += 1;
                    }
                }
            }
            3 => {
                let Some(item) = fields.iter().find(|f| f.number == 2 && f.kind == 15) else {
                    continue;
                };
                if latest.get(&id_key(item.bytes)?) != Some(&index) {
                    continue;
                }
                let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) else {
                    continue;
                };
                match glyph_text(payload.bytes) {
                    Some(Ok(text)) if !text.is_empty() => {
                        let glyph = parse_glyph(payload.bytes).ok().flatten();
                        let origin = glyph
                            .as_ref()
                            .and_then(|glyph| glyph.rectangles.first().map(|r| (r.1, r.0)))
                            .unwrap_or((f32::MAX, f32::MAX));
                        let color = glyph.map_or_else(
                            || "unknown".into(),
                            |glyph| color_name(glyph.color, None),
                        );
                        highlights.push((origin, Highlight { text, color }));
                    }
                    Some(Err(error)) => result
                        .warnings
                        .push(format!("A text highlight could not be read: {error:#}")),
                    _ => {}
                }
            }
            7 => match crate::scene_text::parse_root_text(raw) {
                Ok(root) => result.typed_text.extend(
                    root.text
                        .split('\n')
                        .map(str::trim)
                        .filter(|paragraph| !paragraph.is_empty())
                        .map(str::to_owned),
                ),
                Err(error) => result
                    .warnings
                    .push(format!("Typed text could not be read: {error:#}")),
            },
            _ => {}
        }
    }
    highlights.sort_by(|a, b| a.0 .0.total_cmp(&b.0 .0).then(a.0 .1.total_cmp(&b.0 .1)));
    result.highlights = highlights.into_iter().map(|(_, text)| text).collect();
    Ok(result)
}

/// The colour name of a drawn line payload, or `None` for an eraser or
/// control stroke, or a payload that cannot be read.
fn stroke_color(payload: &[u8]) -> Option<String> {
    let mut nested = Reader::new(payload);
    if nested.take(1).ok()? != [3] {
        return None;
    }
    let inner = nested.fields().ok()?;
    let get = |n, k| {
        inner
            .iter()
            .find(|f: &&Field| f.number == n && f.kind == k)
            .map(|f| le_u32(f.bytes))
    };
    let tool = get(1, 4)?;
    if matches!(tool, 6 | 8 | 9 | 10 | 11 | 22 | 24) {
        return None;
    }
    let name = color_name(get(2, 4)?, get(8, 4));
    Some(match tool {
        5 | 18 => format!("highlighter {name}"),
        23 => format!("shader {name}"),
        _ => name,
    })
}

/// Field 5 of a glyph item: a counted string with a one-byte encoding marker
/// (1 = UTF-8). `None` when the item is not a glyph range or has no string.
fn glyph_text(payload: &[u8]) -> Option<Result<String>> {
    let mut nested = Reader::new(payload);
    if nested.take(1).ok()? != [1] {
        return None;
    }
    let fields = match nested.fields() {
        Ok(fields) => fields,
        Err(error) => return Some(Err(error)),
    };
    let value = fields.iter().find(|f| f.number == 5 && f.kind == 12)?;
    Some((|| {
        let mut cursor = Reader::new(value.bytes);
        let length = usize::try_from(cursor.var(64)?)?;
        ensure!(length <= 4 * 1024 * 1024, "highlight text exceeds 4 MiB");
        let encoding = cursor.take(1)?[0];
        let raw = cursor.take(length)?;
        ensure!(encoding == 1, "highlight text uses encoding {encoding}");
        Ok(String::from_utf8_lossy(raw).trim().to_owned())
    })())
}

/// Render the supported drawing subset, drawing inserted images from the
/// files `images` supplies by image-table name.
///
/// `images` returns `Ok(None)` for a file that is not available. Each file is
/// bounded by [`MAX_IMAGE_BYTES`] and a page's images together by
/// [`MAX_PAGE_IMAGE_BYTES`]; files must be PNG or JPEG and decode cleanly.
/// An image that cannot be drawn is reported, never fatal.
pub fn render_svg_with_images(
    bytes: &[u8],
    width: f32,
    height: f32,
    images: &mut dyn FnMut(&str) -> Result<Option<Vec<u8>>>,
) -> Result<RenderedPage> {
    if crate::legacy_notebook::version(bytes).is_some() {
        return crate::legacy_notebook::render(bytes, width, height);
    }
    ensure!(
        width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0
            && width <= MAX_CANVAS_DIMENSION
            && height <= MAX_CANVAS_DIMENSION,
        "invalid page dimensions"
    );
    let authors = scene_authors(bytes)?;
    let SceneBlocks { blocks, skipped } = scene_blocks(bytes)?;
    let geometry = geometry_from_blocks(&blocks)?;
    let x_origin = geometry.paper_size.map_or(WIDTH / 2.0, |size| size.0 / 2.0);
    let mut warnings = Vec::new();
    warnings.extend(skipped_blocks_warning(&skipped));
    // An item identity may occur again as merge state arrives. Only its last
    // envelope is authoritative; a payload-free replacement stays absent.
    let mut latest = HashMap::<Vec<u8>, usize>::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if matches!(tag, 3 | 4 | 5 | 6 | 8 | 15) {
            if let Some(f) = fields.iter().find(|f| f.number == 2 && f.kind == 15) {
                latest.insert(id_key(f.bytes)?, index);
            }
        }
    }
    // Typed text is decoded and laid out first: anchored groups are placed
    // relative to its characters.
    let mut text_blocks = Vec::new();
    let mut approximate_text = false;
    for (tag, _, _, raw) in &blocks {
        if *tag != 7 {
            continue;
        }
        let root = crate::scene_text::parse_root_text(raw)?;
        if root.had_unknown_encoding {
            warnings.push(
                "A typed-text run uses an unrecognised encoding and shows replacement characters"
                    .into(),
            );
        }
        if !root.text.trim().is_empty() {
            approximate_text = true;
        }
        let laid = crate::scene_text::layout(&root, x_origin)?;
        text_blocks.push((root, laid));
    }
    // The image table maps each image reference to a file of this page.
    let mut image_table = HashMap::new();
    let mut unreadable_table = false;
    for (tag, _, fields, _) in &blocks {
        if *tag == 14 && parse_image_table(fields, &mut image_table).is_err() {
            unreadable_table = true;
        }
    }
    if unreadable_table {
        warnings
            .push("The page's image table could not be read, so its images were not drawn".into());
        image_table.clear();
    }
    // Group references make the visible tree. The root is the reserved (0,1)
    // ID. A line in an unlinked node is detached merge history and must not be
    // resurrected merely because its payload remains in the file.
    let root = id_key(&[0, 1])?;
    let (reachable, children) = group_tree(&blocks, &latest, &root)?;
    let mut groups = HashMap::<Vec<u8>, Vec<GroupPaint>>::new();
    for (index, (tag, _, fields, _)) in blocks.iter().enumerate() {
        if *tag != 4 {
            continue;
        }
        let Some(item_field) = fields.iter().find(|f| f.number == 2 && f.kind == 15) else {
            warnings.push("A group reference without an item identity was ignored".into());
            continue;
        };
        let item = id_key(item_field.bytes)?;
        if latest.get(&item) != Some(&index) {
            continue;
        }
        let Some(parent_field) = fields.iter().find(|f| f.number == 1 && f.kind == 15) else {
            warnings.push("A group reference without a parent was ignored".into());
            continue;
        };
        let parent = id_key(parent_field.bytes)?;
        if !reachable.contains(&parent) {
            continue;
        }
        let mut content = GroupContent::Empty;
        if let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) {
            let mut nested = Reader::new(payload.bytes);
            if nested.take(1)? == [2] {
                if let Some(child) = nested
                    .fields()?
                    .iter()
                    .find(|f| f.number == 2 && f.kind == 15)
                {
                    let child = id_key(child.bytes)?;
                    if reachable.contains(&child) {
                        content = GroupContent::Group(child);
                    }
                }
            }
        }
        groups.entry(parent).or_default().push(GroupPaint {
            item,
            left: optional_id(fields, 3)?,
            right: optional_id(fields, 4)?,
            content,
            erase: String::new(),
            extent: None,
        });
    }
    let mut points = 0usize;
    let mut fragment_bytes = 0usize;
    let mut min_x = 0.0f32;
    let mut min_y = 0.0f32;
    let mut max_x = width;
    let mut max_y = height;
    // Anchored nodes follow a typed-text character. Fields 7..10 hold the
    // anchor identity, mode byte, threshold and initial origin x. The node
    // is translated to the anchor character's position in the laid-out text
    // (only vertically, at the stored x, in mode 2). The text layout is this
    // reader's approximation, so anchored content is placed as approximately
    // as the text around it.
    let mut anchors = HashMap::<Vec<u8>, AnchorRecord>::new();
    for (tag, _, fields, _) in &blocks {
        if *tag != 2 {
            continue;
        }
        let Some(node) = fields.iter().find(|f| f.number == 1 && f.kind == 15) else {
            continue;
        };
        if !fields.iter().any(|f| (7..=10).contains(&f.number)) {
            continue;
        }
        let record = AnchorRecord {
            anchor: anchor_register_id(fields, 7)?,
            mode: anchor_register_u8(fields, 8)?.unwrap_or(0),
            initial_x: anchor_register_f32(fields, 10)?,
        };
        // Field 9 is the anchor threshold. The placement does not use it,
        // but a malformed value is still rejected like any other.
        anchor_register_f32(fields, 9)?;
        anchors.insert(id_key(node.bytes)?, record);
    }
    let mut translations = HashMap::<Vec<u8>, (f32, f32)>::new();
    let mut anchored = 0usize;
    let mut character_index = Vec::new();
    if anchors.values().any(|record| record.anchor.is_some()) {
        for (root, _) in &text_blocks {
            character_index.push(
                root.ids
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (*id, index))
                    .collect::<HashMap<_, _>>(),
            );
        }
    }
    for (node, record) in &anchors {
        let Some(anchor) = record.anchor else {
            continue;
        };
        // A node outside the visible tree draws nothing, so its anchor is
        // neither placed nor reported.
        if anchor == (0, 0) || !reachable.contains(node) {
            continue;
        }
        anchored += 1;
        // The firmware clamps the character index to the text and uses the
        // reserved start/end identities as its first and last character.
        let position =
            text_blocks
                .iter()
                .zip(&character_index)
                .find_map(|((root, laid), index)| {
                    let length = root.ids.len();
                    let at = if anchor == ANCHOR_TEXT_START {
                        Some(0)
                    } else if anchor == ANCHOR_TEXT_END {
                        Some(length.saturating_sub(1))
                    } else {
                        root.index_of(anchor, index)
                    }?;
                    crate::scene_text::char_position(laid, at.min(length.saturating_sub(1)))
                });
        let Some((x, baseline)) = position else {
            // "anchor ... is not present in text": the firmware leaves the
            // node where its stored coordinates put it.
            warnings.push(
                "An anchored group's typed-text anchor is missing, so it is drawn at its stored position"
                    .into(),
            );
            continue;
        };
        let x = if record.mode == 2 {
            record.initial_x.unwrap_or(x - x_origin)
        } else {
            x - x_origin
        };
        translations.insert(node.clone(), (x, baseline));
    }
    // Accumulate the translations from the root so bounds and eraser masks
    // can be expressed in page coordinates.
    let mut offsets = HashMap::<Vec<u8>, (f32, f32)>::new();
    if reachable.contains(&root) {
        offsets.insert(
            root.clone(),
            translations.get(&root).copied().unwrap_or((0.0, 0.0)),
        );
        let mut queue = VecDeque::from([root.clone()]);
        while let Some(parent) = queue.pop_front() {
            let base = offsets[&parent];
            for child in children.get(&parent).into_iter().flatten() {
                if !reachable.contains(child) || offsets.contains_key(child) {
                    continue;
                }
                let own = translations.get(child).copied().unwrap_or((0.0, 0.0));
                offsets.insert(child.clone(), (base.0 + own.0, base.1 + own.1));
                queue.push_back(child.clone());
            }
        }
    }
    let offset_of = |parent: &[u8]| offsets.get(parent).copied().unwrap_or((0.0, 0.0));
    let mut saw_unrendered_item = false;
    // Tool values whose brush family actually used the pressure/multiply
    // approximation on this page. Ballpoint/fineliner-only pages stay empty.
    let mut approximate_brushes = BTreeSet::<u32>::new();
    let mut glyph_svg = String::new();
    // Validated image files by name, so an image placed twice decodes once.
    let mut loaded_images =
        HashMap::<String, std::result::Result<EmbeddedImage, &'static str>>::new();
    let mut page_image_bytes = 0usize;
    for (index, (tag, version, fields, _)) in blocks.into_iter().enumerate() {
        if matches!(tag, 4 | 7) {
            continue;
        }
        if tag == 3 {
            // A glyph item highlights a range of typed text.
            let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) else {
                continue;
            };
            let Some(glyph) = parse_glyph(payload.bytes)? else {
                saw_unrendered_item = true;
                continue;
            };
            let css = color(glyph.color, None);
            for (x, y, w, h) in glyph.rectangles {
                let x = x + x_origin;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x + w);
                max_y = max_y.max(y + h);
                glyph_svg.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{css}\" fill-opacity=\"0.35\" style=\"mix-blend-mode:multiply\"/>\n",
                    xml_num(x), xml_num(y), xml_num(w), xml_num(h)
                ));
                ensure!(
                    glyph_svg.len() <= MAX_SVG,
                    "rendered SVG exceeds 32 MiB limit"
                );
            }
            approximate_text = true;
            continue;
        }
        if tag == 6 {
            // A standalone text item. Its payload grammar is not decoded, so
            // report it rather than letting it vanish silently.
            saw_unrendered_item = true;
            continue;
        }
        if !matches!(tag, 5 | 8 | 15) {
            continue;
        }
        let noun = if tag == 15 { "An image" } else { "A line" };
        let Some(item) = fields.iter().find(|f| f.number == 2 && f.kind == 15) else {
            warnings.push(format!("{noun} without an item identity was ignored"));
            continue;
        };
        if latest.get(&id_key(item.bytes)?) != Some(&index) {
            continue;
        }
        let Some(parent) = fields.iter().find(|f| f.number == 1 && f.kind == 15) else {
            warnings.push(format!("{noun} without a parent group was ignored"));
            continue;
        };
        if !reachable.contains(&id_key(parent.bytes)?) {
            warnings.push(format!("{noun} in a detached group was not painted"));
            continue;
        }
        let parent_key = id_key(parent.bytes)?;
        let (group_x, group_y) = offset_of(&parent_key);
        let Some(payload) = fields.iter().find(|f| f.number == 6 && f.kind == 12) else {
            // Field 6 is optional by construction. A structural survey
            // found 884 of 1,880 line-tagged
            // blocks with no payload, plus every sampled tombstone block. A
            // payload-free envelope is the normal representation of a deleted
            // stroke or image, or of a CRDT replacement whose newest envelope
            // carries no drawing data. Nothing is missing, so nothing is
            // reported; the record still joins the group so its left/right
            // links keep ordering its surviving neighbours.
            groups.entry(parent_key).or_default().push(GroupPaint {
                item: id_key(item.bytes)?,
                left: optional_id(&fields, 3)?,
                right: optional_id(&fields, 4)?,
                content: GroupContent::Empty,
                erase: String::new(),
                extent: None,
            });
            continue;
        };
        if tag == 15 {
            // An inserted image: its placement is in the item, its file is
            // named by the page's image table.
            let placed = parse_image_item(payload.bytes).and_then(|image| {
                let name = image_table
                    .get(&image.key)
                    .map(|entry| entry.name.clone())
                    .ok_or(IMAGE_UNLISTED)?;
                if !valid_image_name(&name) {
                    return Err(IMAGE_NAME);
                }
                if !loaded_images.contains_key(&name) {
                    let loaded = match images(&name) {
                        Ok(Some(bytes)) => embeddable_image(&bytes),
                        Ok(None) => Err(IMAGE_MISSING),
                        Err(_) => Err(IMAGE_UNREADABLE_FILE),
                    };
                    loaded_images.insert(name.clone(), loaded);
                }
                let embedded = loaded_images[&name].as_ref().map_err(|reason| *reason)?;
                // Every placement embeds the file again, so each one counts.
                let cost = embedded.bytes;
                if page_image_bytes + cost > MAX_PAGE_IMAGE_BYTES {
                    return Err(IMAGE_PAGE_BUDGET);
                }
                page_image_bytes += cost;
                Ok((image_element(&image, x_origin, embedded), image.extent()))
            });
            let (content, extent) = match placed {
                Ok((svg, extent)) => {
                    min_x = min_x.min(extent.x + x_origin + group_x);
                    min_y = min_y.min(extent.y + group_y);
                    max_x = max_x.max(extent.x + extent.width + x_origin + group_x);
                    max_y = max_y.max(extent.y + extent.height + group_y);
                    (GroupContent::Image(svg), Some(extent))
                }
                Err(reason) => {
                    warnings.push(reason.into());
                    (GroupContent::Empty, None)
                }
            };
            groups.entry(parent_key).or_default().push(GroupPaint {
                item: id_key(item.bytes)?,
                left: optional_id(&fields, 3)?,
                right: optional_id(&fields, 4)?,
                content,
                erase: String::new(),
                extent,
            });
            continue;
        }
        // A newer, forward-compatible line block keeps the schema-2 point
        // layout its minimum version promises.
        ensure!(version >= 1, "unsupported point schema {version}");
        let mut nested = Reader::new(payload.bytes);
        ensure!(nested.take(1)? == [3], "invalid line subtype");
        let inner = nested.fields()?;
        let get = |n, k| {
            inner
                .iter()
                .find(|f| f.number == n && f.kind == k)
                .map(|f| f.bytes)
        };
        let tool = le_u32(get(1, 4).ok_or_else(|| anyhow!("line has no tool"))?);
        let ink = le_u32(get(2, 4).ok_or_else(|| anyhow!("line has no color"))?);
        let thick = f64le(get(3, 8).ok_or_else(|| anyhow!("line has no thickness"))?);
        ensure!(
            thick.is_finite() && (0.0..=16384.0).contains(&thick),
            "invalid line thickness"
        );
        let raw = get(5, 12).ok_or_else(|| anyhow!("line has no points"))?;
        let point_size = if version == 1 { 24 } else { 14 };
        ensure!(raw.len() % point_size == 0, "misaligned point buffer");
        points = points
            .checked_add(raw.len() / point_size)
            .ok_or_else(|| anyhow!("too many points"))?;
        ensure!(points <= MAX_POINTS, "too many scene points");
        if tool == 8 {
            groups
                .entry(id_key(parent.bytes)?)
                .or_default()
                .push(GroupPaint {
                    item: id_key(item.bytes)?,
                    left: optional_id(&fields, 3)?,
                    right: optional_id(&fields, 4)?,
                    content: GroupContent::Empty,
                    erase: String::new(),
                    extent: None,
                });
            continue;
        }
        if tool == 6 {
            let mut positions = raw
                .chunks_exact(point_size)
                .map(|point| (f32le(&point[..4]), f32le(&point[4..8])));
            let first = positions.next();
            let drawable = first.is_some_and(|first| positions.any(|point| point != first));
            if !drawable {
                groups
                    .entry(id_key(parent.bytes)?)
                    .or_default()
                    .push(GroupPaint {
                        item: id_key(item.bytes)?,
                        left: optional_id(&fields, 3)?,
                        right: optional_id(&fields, 4)?,
                        content: GroupContent::Empty,
                        erase: String::new(),
                        extent: None,
                    });
                continue;
            }
        }
        if matches!(tool, 9 | 10 | 11 | 22 | 24) {
            warnings.push(format!(
                "Tool {tool} is not painted because its erase/control semantics are unsupported"
            ));
            groups
                .entry(id_key(parent.bytes)?)
                .or_default()
                .push(GroupPaint {
                    item: id_key(item.bytes)?,
                    left: optional_id(&fields, 3)?,
                    right: optional_id(&fields, 4)?,
                    content: GroupContent::Empty,
                    erase: String::new(),
                    extent: None,
                });
            continue;
        }
        // Tools with an unmapped brush routine fall through to solid ink.
        if !matches!(
            tool,
            0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19 | 21 | 23
        ) {
            warnings.push(format!("Tool {tool} uses approximate brush rendering"));
        }
        let brush = brush_for(tool);
        // Ballpoint, fineliner and solid pen render as plain solid ink, so a
        // page drawn only with those needs no approximation note. Only a tool
        // whose brush uses the pressure/multiply simulation records one.
        if tool != 6 && (brush.pressure_floor < 1.0 || brush.multiply) {
            approximate_brushes.insert(tool);
        }
        let explicit = get(8, 4).map(le_u32);
        let css = if ink == 9 {
            format!("#{:06x}", explicit.unwrap_or(0) & 0xffffff)
        } else {
            color(ink, explicit).into()
        };
        let mut line_svg = String::new();
        // The segments are drawn solid and the stroke's opacity and blend go
        // on one group around them, as the tablet paints a stroke: segments
        // that each carried them would darken wherever their round ends
        // overlap, turning a highlighter or marker into a string of beads.
        let mut opacity_sum = 0.0f64;
        let mut samples = 0u32;
        let mut previous: Option<(f32, f32, f32, f32)> = None;
        // Per-stroke extent, used to decide whether an unresolved ordering tie
        // between two siblings could change what is visible.
        let mut line_box: Option<(f32, f32, f32, f32)> = None;
        let mut grow = |x0: f32, y0: f32, x1: f32, y1: f32| {
            let b = line_box.get_or_insert((x0, y0, x1, y1));
            b.0 = b.0.min(x0);
            b.1 = b.1.min(y0);
            b.2 = b.2.max(x1);
            b.3 = b.3.max(y1);
        };
        for p in raw.chunks_exact(point_size) {
            let x = f32le(&p[0..4]) + x_origin;
            let y = f32le(&p[4..8]);
            let packed = if version == 1 {
                for component in p.as_chunks::<4>().0 {
                    ensure!(f32le(component).is_finite(), "non-finite version-1 point");
                }
                let width = f32le(&p[16..20]);
                ensure!(
                    (0.0..=16383.75).contains(&width),
                    "version-1 point width exceeds conversion range"
                );
                (width * 4.0).round() / 4.0
            } else {
                f32::from(u16::from_le_bytes([p[10], p[11]])) / 4.0
            };
            ensure!(
                x.is_finite() && y.is_finite() && packed.is_finite(),
                "non-finite point"
            );
            ensure!(
                x.abs() <= 1_000_000.0 && y.abs() <= 1_000_000.0,
                "scene coordinates exceed preview bounds"
            );
            // Pressure is the last byte of a v2 record and the sixth f32 of a
            // v1 one. It drives the brush's per-segment opacity.
            let pressure = if version == 1 {
                f32le(&p[20..24]).clamp(0.0, 1.0)
            } else {
                f32::from(p[13]) / 255.0
            };
            let stroke_opacity = if brush.pressure_floor >= 1.0 {
                brush.opacity
            } else {
                brush.opacity * (brush.pressure_floor + (1.0 - brush.pressure_floor) * pressure)
            };
            opacity_sum += f64::from(stroke_opacity.clamp(0.0, 1.0));
            samples += 1;
            let w =
                ((if packed > 0.0 { packed } else { thick as f32 }) * brush.width_scale).max(0.25);
            if let Some((px, py, pw, _)) = previous {
                let radius = (pw + w) / 4.0;
                grow(
                    px.min(x) - radius,
                    py.min(y) - radius,
                    px.max(x) + radius,
                    py.max(y) + radius,
                );
                if tool != 6 {
                    min_x = min_x.min(px.min(x) - radius + group_x);
                    min_y = min_y.min(py.min(y) - radius + group_y);
                    max_x = max_x.max(px.max(x) + radius + group_x);
                    max_y = max_y.max(py.max(y) + radius + group_y);
                }
                line_svg.push_str(&format!("<path d=\"M {} {} L {} {}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n",xml_num(px),xml_num(py),xml_num(x),xml_num(y),if tool == 6 { "#000000" } else { &css },xml_num((pw+w)/2.0)));
            } else {
                let radius = w / 2.0;
                grow(x - radius, y - radius, x + radius, y + radius);
                if tool != 6 {
                    min_x = min_x.min(x - radius + group_x);
                    min_y = min_y.min(y - radius + group_y);
                    max_x = max_x.max(x + radius + group_x);
                    max_y = max_y.max(y + radius + group_y);
                }
                line_svg.push_str(&format!(
                    "<circle cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{}\"/>\n",
                    xml_num(x),
                    xml_num(y),
                    xml_num(w / 2.0),
                    if tool == 6 { "#000000" } else { &css },
                ));
            }
            previous = Some((x, y, w, stroke_opacity));
            ensure!(
                line_svg.len() <= MAX_SVG,
                "rendered SVG exceeds 32 MiB limit"
            );
        }
        // The eraser's mask geometry stays solid; solid ink needs no group.
        let opacity = if samples == 0 {
            1.0
        } else {
            (opacity_sum / f64::from(samples)) as f32
        };
        if tool != 6 && (opacity < 1.0 || brush.multiply) {
            line_svg = format!(
                "<g opacity=\"{}\"{}>\n{line_svg}</g>\n",
                xml_num(opacity),
                if brush.multiply {
                    " style=\"mix-blend-mode:multiply\""
                } else {
                    ""
                }
            );
        }
        fragment_bytes = fragment_bytes
            .checked_add(line_svg.len())
            .ok_or_else(|| anyhow!("rendered SVG size overflow"))?;
        ensure!(
            fragment_bytes <= MAX_SVG,
            "rendered SVG exceeds 32 MiB limit"
        );
        groups
            .entry(id_key(parent.bytes)?)
            .or_default()
            .push(GroupPaint {
                item: id_key(item.bytes)?,
                left: optional_id(&fields, 3)?,
                right: optional_id(&fields, 4)?,
                content: if tool == 6 {
                    GroupContent::Empty
                } else {
                    GroupContent::Paint(line_svg.clone())
                },
                erase: if tool == 6 { line_svg } else { String::new() },
                extent: line_box.map(|(x0, y0, x1, y1)| CanvasBounds {
                    x: x0,
                    y: y0,
                    width: x1 - x0,
                    height: y1 - y0,
                }),
            });
    }
    // Typed text is laid out after the strokes so its extent can grow the page.
    let mut text_svg_body = String::new();
    for (_, laid) in &text_blocks {
        let (svg, extent) = text_svg(laid)?;
        if !svg.is_empty() {
            min_x = min_x.min(extent.x);
            min_y = min_y.min(extent.y);
            max_x = max_x.max(extent.x + extent.width);
            max_y = max_y.max(extent.y + extent.height);
            text_svg_body.push_str(&svg);
            ensure!(
                text_svg_body.len() <= MAX_SVG,
                "rendered SVG exceeds 32 MiB limit"
            );
        }
    }
    if anchored > 0 {
        // A standing caveat about approximate placement, not a failure.
        warnings.push(note(format!(
            "{anchored} anchored group(s) follow their typed-text anchor in the approximate text layout, so they sit where that layout puts the anchor line"
        )));
    }
    if approximate_text {
        warnings.push(note(
            "Typed text uses approximate layout: glyph advances, line height and heading sizes are estimated, not the device's own font metrics",
        ));
    }
    if saw_unrendered_item {
        warnings
            .push("A standalone text item is not drawn because its format is not decoded".into());
    }
    // The page template lives outside the scene. This is only worth saying if
    // the caller does not supply one; `turborm` drops it whenever it
    // composed a template or a PDF background under the ink.
    warnings.push(note(TEMPLATE_OUTSIDE_SCENE));
    if !approximate_brushes.is_empty() {
        warnings.push(note(
            "Pen brush texture and blending are approximated with variable-width round strokes",
        ));
    }
    let bounds = CanvasBounds {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    };
    let mut body = String::new();
    let mut emission = SceneEmission {
        bounds,
        definitions: String::new(),
        warnings,
        visiting: HashSet::new(),
        emitted: HashSet::new(),
        mask_number: 0,
        used: 0,
        authors,
        translations,
        offsets,
    };
    // Glyph highlights sit under the ink; typed text sits above the strokes,
    // matching the device's own painting order for a text page.
    append_limited(&mut body, &glyph_svg, &mut emission.used)?;
    emit_group(&root, &groups, &mut body, &mut emission, 0)?;
    append_limited(&mut body, &text_svg_body, &mut emission.used)?;
    let SceneEmission {
        definitions,
        warnings,
        ..
    } = emission;
    Ok(RenderedPage {
        svg: format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{}\" height=\"{}\" viewBox=\"{} {} {} {}\">\n<defs>\n{definitions}</defs>\n{body}</svg>\n",
            xml_num(bounds.width),
            xml_num(bounds.height),
            xml_num(bounds.x),
            xml_num(bounds.y),
            xml_num(bounds.width),
            xml_num(bounds.height)
        ),
        warnings,
        bounds,
    })
}

fn valid_page_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
fn register<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.get(key)?.get("value")
}
fn timestamp_author(obj: &Value, key: &str, authors: &HashMap<u16, [u8; 16]>) -> [u8; 16] {
    let index = obj
        .get(key)
        .and_then(|v| v.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(|s| s.split_once(':'))
        .and_then(|(a, _)| a.parse::<u16>().ok())
        .unwrap_or(0);
    authors.get(&index).copied().unwrap_or([0; 16])
}

/// Resolve active content pages in display order and their optional PDF backgrounds.
pub fn pages(content: &Value) -> Result<Vec<PageRef>> {
    let root = content
        .as_object()
        .ok_or_else(|| anyhow!("content must be a JSON object"))?;
    let version = root
        .get("formatVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing formatVersion"))?;
    if version == 1 {
        let ids = root
            .get("pages")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("v1 content has no pages array"))?;
        ensure!(ids.len() <= MAX_PAGES, "too many pages");
        let file_type = root
            .get("fileType")
            .and_then(Value::as_str)
            .unwrap_or("notebook");
        let owned_redirects;
        // Some documents store an empty map beside their pages; that is no
        // map at all, not a map of the wrong length. A notebook has no PDF
        // behind its pages, so a map left over from fewer pages is ignored
        // rather than refusing the whole notebook.
        let redirects = if let Some(values) = root
            .get("redirectionPageMap")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .filter(|values| file_type != "notebook" || values.len() == ids.len())
        {
            values
        } else {
            if file_type == "pdf" {
                // Without a map, page n shows source page n. An unknown
                // (negative or absent) original count is not "too small": a
                // missing source page is reported on that page alone.
                if let Some(original) = root.get("originalPageCount").and_then(Value::as_u64) {
                    ensure!(
                        original as usize >= ids.len(),
                        "PDF page map is missing and originalPageCount is too small"
                    );
                }
                owned_redirects = (0..ids.len())
                    .map(|n| Value::from(n as u64))
                    .collect::<Vec<_>>();
            } else {
                owned_redirects = (0..ids.len()).map(|_| Value::from(-1)).collect::<Vec<_>>();
            }
            &owned_redirects
        };
        ensure!(
            redirects.len() == ids.len(),
            "v1 page and redirection counts differ"
        );
        return ids
            .iter()
            .zip(redirects)
            .map(|(id, bg)| {
                let id = id
                    .as_str()
                    .ok_or_else(|| anyhow!("page ID is not a string"))?;
                ensure!(valid_page_id(id), "invalid page ID");
                let n = bg
                    .as_i64()
                    .ok_or_else(|| anyhow!("page redirection is not an integer"))?;
                Ok(PageRef {
                    id: id.into(),
                    background: if n < 0 {
                        None
                    } else {
                        Some(usize::try_from(n)?)
                    },
                })
            })
            .collect();
    }
    ensure!(version == 2, "unsupported content formatVersion {version}");
    let cp = root
        .get("cPages")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("v2 content has no cPages"))?;
    let mut authors = HashMap::new();
    let mut author_uuids = HashSet::new();
    for pair in cp
        .get("uuids")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cPages has no UUID map"))?
    {
        let text = pair
            .get("first")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("invalid cPages UUID"))?;
        let uuid = Uuid::parse_str(text).context("invalid cPages UUID")?;
        ensure!(uuid.to_string() == text, "noncanonical cPages UUID");
        ensure!(
            author_uuids.insert(*uuid.as_bytes()),
            "duplicate cPages UUID"
        );
        let index = pair
            .get("second")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("invalid cPages author index"))?;
        ensure!(index <= u16::MAX as u64, "cPages author index overflow");
        ensure!(
            authors.insert(index as u16, *uuid.as_bytes()).is_none(),
            "duplicate cPages author index"
        );
    }
    let source = cp
        .get("pages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cPages has no pages"))?;
    ensure!(source.len() <= MAX_PAGES, "too many pages");
    let mut active = Vec::new();
    let mut seen = HashSet::new();
    for page in source {
        let id = page
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("page has no ID"))?;
        ensure!(valid_page_id(id), "invalid page ID");
        ensure!(seen.insert(id), "duplicate page ID");
        // Firmware serializes this LWW flag as either a JSON boolean or 0/1.
        // Reject other representations instead of resurrecting deleted pages.
        let deleted = match register(page, "deleted") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(value) if value.as_u64() == Some(0) => false,
            Some(value) if value.as_u64() == Some(1) => true,
            Some(_) => bail!("invalid page deletion flag"),
        };
        if deleted {
            continue;
        }
        let idx = register(page, "idx").and_then(Value::as_str).unwrap_or("");
        ensure!(
            idx.bytes().all(|b| b.is_ascii_lowercase()),
            "invalid page position"
        );
        let background = match register(page, "redir") {
            None => None,
            Some(v) => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| anyhow!("invalid page redirection"))?;
                if n < 0 {
                    None
                } else {
                    Some(usize::try_from(n)?)
                }
            }
        };
        active.push((
            idx.to_owned(),
            timestamp_author(page, "idx", &authors),
            id.to_owned(),
            background,
        ));
    }
    active.sort_by(|a, b| {
        let order = a.0.cmp(&b.0);
        if order != Ordering::Equal {
            return order;
        }
        let order = a.1.cmp(&b.1);
        if order != Ordering::Equal {
            return order;
        }
        a.2.cmp(&b.2)
    });
    Ok(active
        .into_iter()
        .map(|(_, _, id, background)| PageRef { id, background })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn var(mut n: u64) -> Vec<u8> {
        let mut v = vec![];
        loop {
            let mut b = (n & 127) as u8;
            n >>= 7;
            if n > 0 {
                b |= 128
            }
            v.push(b);
            if n == 0 {
                return v;
            }
        }
    }
    fn field(n: u16, k: u8, p: &[u8]) -> Vec<u8> {
        let mut v = var(((n << 4) | u16::from(k)) as u64);
        if k == 12 {
            v.extend((p.len() as u32).to_le_bytes())
        }
        v.extend(p);
        v
    }
    fn id(counter: u64) -> Vec<u8> {
        let mut v = var(0);
        v.extend(var(counter));
        v
    }
    fn authored_id(author: u16, counter: u64) -> Vec<u8> {
        let mut v = var(u64::from(author));
        v.extend(var(counter));
        v
    }
    fn author_table(entries: &[(u16, [u8; 16])]) -> Vec<u8> {
        let mut payload = var(entries.len() as u64);
        for &(index, uuid) in entries {
            let mut nested = var(16);
            nested.extend(uuid);
            nested.extend(index.to_le_bytes());
            payload.extend(var(12));
            payload.extend((nested.len() as u32).to_le_bytes());
            payload.extend(nested);
        }
        block(9, &payload)
    }
    fn block(tag: u8, p: &[u8]) -> Vec<u8> {
        let mut v = (p.len() as u32).to_le_bytes().to_vec();
        v.extend([0, 2, 2, tag]);
        v.extend(p);
        v
    }
    fn scene(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut v = HEADER.to_vec();
        for b in blocks {
            v.extend(b)
        }
        v
    }
    fn line(item: u64, x: f32) -> Vec<u8> {
        line_in(1, item, x)
    }
    fn line_in(parent: u64, item: u64, x: f32) -> Vec<u8> {
        line_points(parent, item, &[(x, 10.0, 8), (x + 5.0, 15.0, 16)])
    }
    fn line_points(parent: u64, item: u64, points: &[(f32, f32, u16)]) -> Vec<u8> {
        line_points_tool(parent, item, 2, points)
    }
    fn line_points_tool(parent: u64, item: u64, tool: u32, points: &[(f32, f32, u16)]) -> Vec<u8> {
        let mut point = vec![];
        for &(px, py, w) in points {
            point.extend(px.to_le_bytes());
            point.extend(py.to_le_bytes());
            point.extend(0u16.to_le_bytes());
            point.extend(w.to_le_bytes());
            point.extend([0, 255]);
        }
        let mut inner = vec![3];
        inner.extend(field(1, 4, &tool.to_le_bytes()));
        inner.extend(field(2, 4, &0u32.to_le_bytes()));
        inner.extend(field(3, 8, &2f64.to_le_bytes()));
        inner.extend(field(4, 4, &0f32.to_le_bytes()));
        inner.extend(field(5, 12, &point));
        let mut env = field(1, 15, &id(parent));
        env.extend(field(2, 15, &id(item)));
        env.extend(field(6, 12, &inner));
        block(5, &env)
    }
    fn line_ordered(
        parent: u64,
        item: u64,
        left: Option<u64>,
        right: Option<u64>,
        x: f32,
    ) -> Vec<u8> {
        let original = line_points_tool(parent, item, 2, &[(x, 10.0, 8), (x + 5.0, 15.0, 8)]);
        let fields = Reader::new(&original[8..]).fields().unwrap();
        let mut env = Vec::new();
        for value in fields {
            env.extend(field(value.number, value.kind, value.bytes));
            if value.number == 2 {
                if let Some(left) = left {
                    env.extend(field(3, 15, &id(left)));
                }
                if let Some(right) = right {
                    env.extend(field(4, 15, &id(right)));
                }
            }
        }
        block(5, &env)
    }
    fn line_v1(parent: u64, item: u64, tool: u32, width: f32) -> Vec<u8> {
        let mut points = Vec::new();
        for (x, y) in [(0.0f32, 10.0f32), (5.0, 15.0)] {
            for value in [x, y, 0.0, 0.0, width, 1.0] {
                points.extend(value.to_le_bytes());
            }
        }
        let mut inner = vec![3];
        inner.extend(field(1, 4, &tool.to_le_bytes()));
        inner.extend(field(2, 4, &0u32.to_le_bytes()));
        inner.extend(field(3, 8, &2f64.to_le_bytes()));
        inner.extend(field(4, 4, &0f32.to_le_bytes()));
        inner.extend(field(5, 12, &points));
        let mut env = field(1, 15, &id(parent));
        env.extend(field(2, 15, &id(item)));
        env.extend(field(6, 12, &inner));
        let mut result = (env.len() as u32).to_le_bytes().to_vec();
        result.extend([0, 1, 1, 5]);
        result.extend(env);
        result
    }
    fn group(item: u64, child: u64) -> Vec<u8> {
        group_in(1, item, child)
    }
    fn group_in(parent: u64, item: u64, child: u64) -> Vec<u8> {
        let mut inner = vec![2];
        inner.extend(field(2, 15, &id(child)));
        let mut env = field(1, 15, &id(parent));
        env.extend(field(2, 15, &id(item)));
        env.extend(field(6, 12, &inner));
        block(4, &env)
    }
    fn visibility(node: u64, value: bool) -> Vec<u8> {
        let mut reg = field(1, 15, &id(50));
        reg.extend(field(2, 1, &[u8::from(value)]));
        let mut body = field(1, 15, &id(node));
        body.extend(field(3, 12, &reg));
        block(2, &body)
    }
    fn scene_info(
        legacy: Option<(u32, u32)>,
        paper: Option<(f64, f64)>,
        rect: Option<(f64, f64, f64, f64)>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some((width, height)) = legacy {
            let mut pair = width.to_le_bytes().to_vec();
            pair.extend(height.to_le_bytes());
            body.extend(field(5, 12, &pair));
        }
        if let Some((x, y, width, height)) = rect {
            let mut values = x.to_le_bytes().to_vec();
            values.extend(y.to_le_bytes());
            values.extend(width.to_le_bytes());
            values.extend(height.to_le_bytes());
            let mut register = field(1, 15, &id(8));
            register.extend(field(2, 12, &values));
            body.extend(field(6, 12, &register));
        }
        if let Some((width, height)) = paper {
            let mut pair = width.to_le_bytes().to_vec();
            pair.extend(height.to_le_bytes());
            body.extend(field(7, 12, &pair));
        }
        block(13, &body)
    }
    #[test]
    fn reads_scene_geometry_with_preferred_paper_and_separate_background() {
        let geometry = scene_geometry(&scene(&[scene_info(
            Some((1404, 1872)),
            Some((1620.0, 2160.0)),
            Some((-2682.0, -468.0, 5364.0, 5363.0)),
        )]))
        .unwrap();
        assert_eq!(geometry.paper_size, Some((1620.0, 2160.0)));
        assert_eq!(
            geometry.scene_rect,
            Some(CanvasBounds {
                x: -2682.0,
                y: -468.0,
                width: 5364.0,
                height: 5363.0,
            })
        );
    }
    #[test]
    fn reads_legacy_paper_size_and_allows_absent_geometry() {
        assert_eq!(
            scene_geometry(&scene(&[scene_info(Some((1404, 1872)), None, None)]))
                .unwrap()
                .paper_size,
            Some((1404.0, 1872.0))
        );
        assert_eq!(
            scene_geometry(&scene(&[])).unwrap(),
            SceneGeometry::default()
        );
    }
    #[test]
    fn rejects_malformed_and_unsafe_scene_geometry() {
        let malformed = block(13, &field(7, 12, &[0; 15]));
        assert!(scene_geometry(&scene(&[malformed])).is_err());
        let malformed_rect = block(13, &field(6, 12, &field(2, 12, &[0; 31])));
        assert!(scene_geometry(&scene(&[malformed_rect])).is_err());
        for invalid in [
            scene_info(None, Some((f64::NAN, 1872.0)), None),
            scene_info(None, Some((1_000_001.0, 1872.0)), None),
            scene_info(None, None, Some((0.0, 0.0, 0.0, 10.0))),
            scene_info(None, None, Some((-1_000_001.0, 0.0, 10.0, 10.0))),
        ] {
            assert!(scene_geometry(&scene(&[invalid])).is_err());
        }
        let mut truncated = scene(&[scene_info(None, Some((1404.0, 1872.0)), None)]);
        truncated.pop();
        assert!(scene_geometry(&truncated).is_err());
    }
    #[test]
    fn declared_paper_width_sets_centered_x_origin() {
        let rendered = render_svg_with_size(
            &scene(&[scene_info(None, Some((1620.0, 2160.0)), None), line(1, 0.0)]),
            1620.0,
            2160.0,
        )
        .unwrap();
        assert!(rendered.svg.contains("cx=\"810\""));
    }
    #[test]
    fn renders_variable_width_and_centered_x() {
        let r = render_svg(&scene(&[line(1, 0.)])).unwrap();
        assert!(r.svg.contains("cx=\"702\""));
        assert!(r.svg.contains("stroke-width=\"3\""));
        assert_eq!(
            r.bounds,
            CanvasBounds {
                x: 0.0,
                y: 0.0,
                width: 1404.0,
                height: 1872.0
            }
        );
    }
    #[test]
    fn eraser_six_masks_only_its_group_and_eight_is_control_only() {
        let rendered = render_svg(&scene(&[
            line_points_tool(1, 1, 2, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
            line_points_tool(1, 2, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
            line_points_tool(1, 3, 8, &[(20.0, 10.0, 12), (25.0, 15.0, 12)]),
            line_points_tool(9, 4, 2, &[(40.0, 10.0, 8), (45.0, 15.0, 8)]),
        ]))
        .unwrap();
        assert!(rendered.svg.contains("<mask id=\"eraser-"));
        assert!(rendered.svg.contains("mask=\"url(#eraser-"));
        assert!(!rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("Tool 6")));
        assert!(!rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("Tool 8")));
        // Tool 8's distant points never become black mask geometry.
        assert!(!rendered.svg.contains("M 722 10 L 727 15"));
    }
    /// Nesting does not multiply a stroke's share of the SVG limit: a page
    /// whose SVG is well under the limit renders however deep its layers are.
    /// Each stroke used to be charged once per level, so a real 14 MB page
    /// with layers failed as if it were 42 MB.
    #[test]
    fn nested_groups_count_each_stroke_once_against_the_svg_limit() {
        let points = (0..90_000)
            .map(|i| ((i % 1000) as f32, (i / 1000) as f32 * 20.0, 8))
            .collect::<Vec<_>>();
        let rendered = render_svg(&scene(&[
            group(20, 30),
            group_in(30, 21, 40),
            line_points_tool(40, 1, 2, &points),
            line_points_tool(40, 2, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
        ]))
        .unwrap();
        assert!(
            (MAX_SVG / 3..MAX_SVG / 2).contains(&rendered.svg.len()),
            "the stroke alone must fit, and three counts of it must not: {}",
            rendered.svg.len()
        );
        assert_eq!(rendered.svg.matches("<path").count(), 90_000);
    }
    #[test]
    fn an_eraser_hides_only_what_was_drawn_before_it() {
        let rendered = render_svg(&scene(&[
            line_points_tool(1, 1, 2, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
            line_points_tool(1, 2, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
            line_points_tool(1, 3, 6, &[(1.0, 10.0, 12), (6.0, 15.0, 12)]),
            line_points_tool(1, 4, 2, &[(40.0, 10.0, 8), (45.0, 15.0, 8)]),
            line_points_tool(1, 5, 6, &[(40.0, 10.0, 12), (45.0, 15.0, 12)]),
        ]))
        .unwrap();
        let svg = &rendered.svg;
        // Two eraser runs: the first two strokes share one mask, which holds
        // only the stroke written before them; the later stroke is outside it
        // and inside the second mask, with the first one nested there.
        assert_eq!(svg.matches("<mask id=").count(), 2, "{svg}");
        let first = svg.find("<g mask=\"url(#eraser-0)\">").expect(svg);
        let second = svg.find("<g mask=\"url(#eraser-1)\">").expect(svg);
        let early = svg.rfind("M 702 10 L 707 15").expect(svg);
        let late = svg.rfind("M 742 10 L 747 15").expect(svg);
        let first_end = first + svg[first..].find("</g>").unwrap();
        assert!(second < first && first < early && early < first_end);
        assert!(first_end < late, "{svg}");
        // An eraser that comes first hides nothing and adds no mask.
        let leading = render_svg(&scene(&[
            line_points_tool(1, 1, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
            line_points_tool(1, 2, 2, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
        ]))
        .unwrap();
        assert!(!leading.svg.contains("<mask"), "{}", leading.svg);
    }
    #[test]
    fn nested_group_mask_is_emitted_through_group_references() {
        let rendered = render_svg(&scene(&[
            group(20, 30),
            group_in(30, 21, 40),
            line_points_tool(40, 1, 2, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
            line_points_tool(40, 2, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
        ]))
        .unwrap();
        assert_eq!(rendered.svg.matches("<g mask=").count(), 1);
        assert!(rendered.svg.contains("<g mask=\"url(#eraser-0)\">"));
    }
    #[test]
    fn constraints_order_paint_across_a_payload_free_tombstone_link() {
        let mut tombstone = field(1, 15, &id(1));
        tombstone.extend(field(2, 15, &id(2)));
        tombstone.extend(field(3, 15, &id(1)));
        tombstone.extend(field(4, 15, &id(3)));
        let rendered = render_svg(&scene(&[
            line_ordered(1, 3, Some(2), None, 30.0),
            line_ordered(1, 1, None, Some(2), 10.0),
            block(8, &tombstone),
        ]))
        .unwrap();
        assert!(rendered.svg.find("M 712 10").unwrap() < rendered.svg.find("M 732 10").unwrap());
        assert!(!rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("unresolved concurrent")));
    }
    /// A tied sibling with no painted geometry.
    fn tied(item: Vec<u8>) -> GroupPaint {
        GroupPaint {
            item,
            left: None,
            right: None,
            content: GroupContent::Empty,
            erase: String::new(),
            extent: None,
        }
    }
    /// A tied sibling that paints the given rectangle.
    fn tied_at(item: Vec<u8>, x: f32, y: f32) -> GroupPaint {
        GroupPaint {
            extent: Some(CanvasBounds {
                x,
                y,
                width: 10.0,
                height: 10.0,
            }),
            ..tied(item)
        }
    }
    #[test]
    fn concurrent_ties_use_the_firmware_author_uuid_then_counter_key() {
        // Ascending author UUID bytes, then ascending 48-bit counter: the key
        // the firmware's comparator applies to concurrent siblings.
        let authors = HashMap::from([(7, [0xff; 16]), (9, [1; 16])]);
        let items = vec![
            tied(authored_id(7, 1)),
            tied(authored_id(9, 99)),
            tied(authored_id(9, 4)),
        ];
        let (order, ambiguous) = constrained_order(&items, &authors).unwrap();
        assert_eq!(order, [2, 1, 0]);
        assert!(!ambiguous);
    }
    #[test]
    fn an_unmapped_author_index_sorts_as_the_all_zero_uuid() {
        // The firmware's map lookup resolves index 0 to the zero UUID, which
        // therefore sorts before every real author.
        let authors = HashMap::from([(7, [0x01; 16])]);
        let items = vec![tied(authored_id(7, 1)), tied(authored_id(0, 9))];
        let (order, ambiguous) = constrained_order(&items, &authors).unwrap();
        assert_eq!(order, [1, 0]);
        assert!(!ambiguous);
    }
    #[test]
    fn a_missing_author_uuid_is_reported_only_when_the_tied_items_overlap() {
        // Same rectangle: the paint order is visible, so say so.
        let overlapping = vec![
            tied_at(authored_id(7, 2), 0.0, 0.0),
            tied_at(authored_id(8, 1), 5.0, 5.0),
        ];
        let (order, ambiguous) = constrained_order(&overlapping, &HashMap::new()).unwrap();
        assert_eq!(order, [0, 1]);
        assert!(ambiguous);
        // Far apart: either order draws the same page, so stay quiet.
        let apart = vec![
            tied_at(authored_id(7, 2), 0.0, 0.0),
            tied_at(authored_id(8, 1), 900.0, 900.0),
        ];
        let (order, ambiguous) = constrained_order(&apart, &HashMap::new()).unwrap();
        assert_eq!(order, [0, 1]);
        assert!(!ambiguous);
    }
    #[test]
    fn scene_author_table_preserves_firmware_uuid_byte_order() {
        let uuid_a = [0x10; 16];
        let uuid_b = [0xe0; 16];
        let bytes = scene(&[author_table(&[(9, uuid_b), (7, uuid_a)])]);
        let authors = scene_authors(&bytes).unwrap();
        assert_eq!(authors.get(&7), Some(&uuid_a));
        assert_eq!(authors.get(&9), Some(&uuid_b));
    }
    #[test]
    fn renders_version_one_points_with_firmware_quarter_width_conversion() {
        let rendered = render_svg(&scene(&[line_v1(1, 1, 2, 2.13)])).unwrap();
        assert!(rendered.svg.contains("stroke-width=\"2.25\""));
        assert!(render_svg(&scene(&[line_v1(1, 1, 2, f32::NAN)])).is_err());
    }
    #[test]
    fn grows_for_long_wide_and_negative_ink() {
        let r = render_svg(&scene(&[
            line_points(1, 1, &[(0.0, 10.0, 8), (0.0, 2000.0, 8)]),
            line_points(1, 2, &[(1000.0, 10.0, 8), (1100.0, 10.0, 8)]),
            line_points(1, 3, &[(-710.0, -20.0, 8), (-705.0, -10.0, 8)]),
        ]))
        .unwrap();
        assert_eq!(r.bounds.x, -9.0);
        assert_eq!(r.bounds.y, -21.0);
        assert_eq!(r.bounds.width, 1812.0);
        assert_eq!(r.bounds.height, 2022.0);
        assert!(r.svg.contains("viewBox=\"-9 -21 1812 2022\""));
        assert!(!r.warnings.iter().any(|w| w.contains("clips")));
    }
    #[test]
    fn hidden_far_stroke_does_not_grow_bounds() {
        let r = render_svg(&scene(&[
            group(20, 30),
            visibility(30, false),
            line_points(30, 4, &[(5000.0, 5000.0, 8)]),
        ]))
        .unwrap();
        assert_eq!(r.bounds.width, WIDTH);
        assert_eq!(r.bounds.height, HEIGHT);
    }
    #[test]
    fn includes_round_stroke_edge_and_custom_base_size() {
        let r = render_svg_with_size(
            &scene(&[line_points(1, 1, &[(1170.0, 1400.0, 16)])]),
            1872.0,
            1404.0,
        )
        .unwrap();
        assert_eq!(r.bounds.width, 1874.0);
        assert_eq!(r.bounds.height, 1404.0);
        assert!(r.svg.contains("cx=\"1872\""));
        assert!(r.svg.contains("viewBox=\"0 0 1874 1404\""));
    }
    #[test]
    fn rejects_invalid_custom_page_size() {
        let s = scene(&[]);
        assert!(render_svg_with_size(&s, 0.0, HEIGHT).is_err());
        assert!(render_svg_with_size(&s, f32::NAN, HEIGHT).is_err());
        assert!(render_svg_with_size(&s, WIDTH, 1_000_001.0).is_err());
    }
    #[test]
    fn tombstone_never_paints_deleted_line() {
        let mut e = field(1, 15, &id(1));
        e.extend(field(2, 15, &id(7)));
        let tomb = block(8, &e);
        let r = render_svg(&scene(&[line(7, 0.), tomb])).unwrap();
        assert!(!r.svg.contains("<path"));
    }
    #[test]
    fn accepts_realistic_initial_blocks() {
        let table = block(9, &[0]);
        let conversion = block(0, &[0x1f, 0, 0, 1, 1, 0xaa, 0xbb]);
        let r = render_svg(&scene(&[table, conversion, line(1, 0.)])).unwrap();
        assert!(r.svg.contains("<path"));
    }
    #[test]
    fn payload_free_replacement_does_not_resurrect_line() {
        let mut empty = field(1, 15, &id(1));
        empty.extend(field(2, 15, &id(4)));
        let r = render_svg(&scene(&[line(4, 0.), block(5, &empty)])).unwrap();
        assert!(!r.svg.contains("<path"));
    }
    #[test]
    fn a_payload_free_line_record_is_expected_and_reports_nothing() {
        // Field 6 is optional: a payload-free envelope is the normal shape of
        // a deleted stroke or a CRDT replacement, not a decoding failure.
        let mut tombstone = field(1, 15, &id(1));
        tombstone.extend(field(2, 15, &id(4)));
        for tag in [5, 8] {
            let r = render_svg(&scene(&[block(tag, &tombstone)])).unwrap();
            assert!(
                r.warnings
                    .iter()
                    .all(|warning| !warning.contains("payload-free")),
                "tag {tag}: {:?}",
                r.warnings
            );
            // Nothing at all is reported for a page that is only tombstones.
            assert!(
                r.warnings
                    .iter()
                    .all(|warning| severity(warning).severity == Severity::Note),
                "tag {tag}: {:?}",
                r.warnings
            );
        }
        // A replacement still suppresses the stroke it replaces, silently.
        let r = render_svg(&scene(&[line(4, 0.), block(5, &tombstone)])).unwrap();
        assert!(!r.svg.contains("<path"));
        assert!(r
            .warnings
            .iter()
            .all(|warning| !warning.contains("payload-free")));
    }
    #[test]
    fn a_ballpoint_only_page_reports_no_brush_approximation() {
        // Tool 2 is Ballpoint: solid ink, no simulated texture, no note.
        let solid = render_svg(&scene(&[line_points_tool(
            1,
            1,
            2,
            &[(10.0, 10.0, 8), (20.0, 20.0, 8)],
        )]))
        .unwrap();
        assert!(
            solid
                .warnings
                .iter()
                .all(|warning| !warning.contains("brush")),
            "{:?}",
            solid.warnings
        );
        // Tool 1 is Pencil, which does use the pressure/multiply simulation.
        let pencil = render_svg(&scene(&[line_points_tool(
            1,
            1,
            1,
            &[(10.0, 10.0, 8), (20.0, 20.0, 8)],
        )]))
        .unwrap();
        let brush = pencil
            .warnings
            .iter()
            .find(|warning| warning.contains("brush"))
            .expect("a pencil page notes its approximation");
        assert_eq!(severity(brush).severity, Severity::Note);
    }
    #[test]
    fn repeated_notes_collapse_to_one_entry_with_a_count() {
        let collapsed = collapse_notes(vec![
            "same".into(),
            "other".into(),
            "same".into(),
            "same".into(),
        ]);
        assert_eq!(collapsed, ["same (×3)", "other"]);
    }
    #[test]
    fn the_template_note_is_informational_and_named_by_a_constant() {
        let r = render_svg(&scene(&[line(1, 0.)])).unwrap();
        let entry = r
            .warnings
            .iter()
            .find(|warning| warning.contains(TEMPLATE_OUTSIDE_SCENE))
            .expect("the template note is present for a caller that supplies none");
        let classified = severity(entry);
        assert_eq!(classified.severity, Severity::Note);
        assert_eq!(classified.text, TEMPLATE_OUTSIDE_SCENE);
    }
    #[test]
    fn detached_group_is_not_painted() {
        let r = render_svg(&scene(&[line_in(30, 4, 0.)])).unwrap();
        assert!(!r.svg.contains("<path"));
        assert!(r.warnings.iter().any(|w| w.contains("detached")));
    }
    #[test]
    fn removed_group_suppresses_its_children() {
        let mut empty = field(1, 15, &id(1));
        empty.extend(field(2, 15, &id(20)));
        let r = render_svg(&scene(&[
            group(20, 30),
            line_in(30, 4, 0.),
            block(4, &empty),
        ]))
        .unwrap();
        assert!(!r.svg.contains("<path"));
    }
    #[test]
    fn hidden_group_suppresses_its_children() {
        let r = render_svg(&scene(&[
            group(20, 30),
            visibility(30, false),
            line_in(30, 4, 0.),
        ]))
        .unwrap();
        assert!(!r.svg.contains("<path"));
    }
    #[test]
    fn hidden_root_suppresses_everything() {
        let r = render_svg(&scene(&[visibility(1, false), line(4, 0.)])).unwrap();
        assert!(!r.svg.contains("<path"));
    }
    #[test]
    fn bounds_fields_per_block() {
        let mut body = Vec::new();
        for _ in 0..=MAX_FIELDS {
            body.extend(field(1, 1, &[0]));
        }
        assert!(render_svg(&scene(&[block(2, &body)])).is_err());
    }
    #[test]
    fn rejects_bad_version_and_truncation() {
        assert!(render_svg(b"reMarkable .lines file, version=5          ").is_err());
        let mut s = scene(&[line(1, 0.)]);
        s.pop();
        assert!(render_svg(&s).is_err());
    }
    #[test]
    fn top_level_routes_supported_legacy_versions() {
        for version in [3, 5] {
            let mut bytes =
                format!("reMarkable .lines file, version={version}          ").into_bytes();
            bytes.extend(0u32.to_le_bytes());
            assert_eq!(scene_geometry(&bytes).unwrap(), SceneGeometry::default());
            assert_eq!(render_svg(&bytes).unwrap().bounds.width, WIDTH);
        }
    }
    #[test]
    fn v1_pages_pair_redirects() {
        let a = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            pages(&json!({"formatVersion":1,"pages":[a],"redirectionPageMap":[2]})).unwrap(),
            vec![PageRef {
                id: a.into(),
                background: Some(2)
            }]
        );
    }
    #[test]
    fn stale_and_empty_v1_redirection_maps_do_not_refuse_the_document() {
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        // A notebook's pages have no PDF background.
        assert_eq!(
            pages(&json!({"formatVersion":1,"fileType":"notebook","pages":[a,b],"redirectionPageMap":[]}))
                .unwrap(),
            vec![
                PageRef { id: a.into(), background: None },
                PageRef { id: b.into(), background: None }
            ]
        );
        // A PDF's pages follow its source pages, as with no map.
        assert_eq!(
            pages(&json!({"formatVersion":1,"fileType":"pdf","originalPageCount":2,"pages":[a,b],"redirectionPageMap":[]}))
                .unwrap()
                .iter()
                .map(|page| page.background)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1)]
        );
        // A notebook's map left over from fewer pages is ignored.
        assert_eq!(
            pages(&json!({"formatVersion":1,"fileType":"notebook","pages":[a,b],"redirectionPageMap":[-1]}))
                .unwrap()
                .iter()
                .map(|page| page.background)
                .collect::<Vec<_>>(),
            vec![None, None]
        );
        // A PDF with an unknown original page count pairs pages in order,
        // but a known count that is too small is still refused, and so is a
        // PDF map of the wrong length.
        assert_eq!(
            pages(&json!({"formatVersion":1,"fileType":"pdf","originalPageCount":-1,"pages":[a],"redirectionPageMap":[]}))
                .unwrap()[0]
                .background,
            Some(0)
        );
        assert!(pages(
            &json!({"formatVersion":1,"fileType":"pdf","originalPageCount":1,"pages":[a,b]})
        )
        .is_err());
        assert!(pages(
            &json!({"formatVersion":1,"fileType":"pdf","pages":[a,b],"redirectionPageMap":[0]})
        )
        .is_err());
    }
    #[test]
    fn v2_sorts_and_hides_tombstones() {
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let c = json!({"formatVersion":2,"cPages":{"uuids":[],"pages":[{"id":a,"idx":{"timestamp":"0:1","value":"z"}},{"id":b,"idx":{"timestamp":"0:2","value":"a"},"deleted":{"timestamp":"0:3","value":true}}]}});
        assert_eq!(
            pages(&c).unwrap(),
            vec![PageRef {
                id: a.into(),
                background: None
            }]
        );
    }
    #[test]
    fn rejects_invalid_ids_and_positions() {
        let c = json!({"formatVersion":2,"cPages":{"uuids":[],"pages":[{"id":"../bad","idx":{"value":"A"}}]}});
        assert!(pages(&c).is_err());
    }

    #[test]
    fn numeric_page_tombstones_stay_deleted() {
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let mut content = json!({"formatVersion":2,"cPages":{"uuids":[],"pages":[
            {"id":a,"idx":{"timestamp":"0:1","value":"a"},"deleted":{"value":1}},
            {"id":b,"idx":{"timestamp":"0:2","value":"b"},"deleted":{"value":0}}
        ]}});
        let active = pages(&content).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, b);
        content["cPages"]["pages"][0]["deleted"]["value"] = json!("1");
        assert!(pages(&content).is_err());
    }

    #[test]
    fn null_geometry_and_empty_modern_image_table_are_valid() {
        let info = scene_info(None, None, Some((0.0, 0.0, 0.0, 0.0)));
        let mut bytes = scene(&[info]);
        bytes.extend([6, 0, 0, 0, 0, 3, 3, 14, 0x1c, 1, 0, 0, 0, 0]);
        assert_eq!(scene_geometry(&bytes).unwrap(), SceneGeometry::default());
        let rendered = render_svg(&bytes).unwrap();
        assert!(rendered.warnings.iter().all(|w| w.starts_with(NOTE_PREFIX)));
        // A table that claims an entry it does not hold is reported, and the
        // rest of the page still renders.
        *bytes.last_mut().unwrap() = 1;
        assert_eq!(scene_geometry(&bytes).unwrap(), SceneGeometry::default());
        let rendered = render_svg(&bytes).unwrap();
        assert!(rendered
            .warnings
            .iter()
            .any(|w| w.contains("image table could not be read")));
    }

    fn block_schema(tag: u8, minimum: u8, version: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_le_bytes().to_vec();
        v.extend([0, minimum, version, tag]);
        v.extend(payload);
        v
    }

    #[test]
    fn blocks_needing_a_newer_reader_are_skipped_and_reported_once() {
        // Payloads a newer writer declared unreadable here are never parsed.
        let rendered = render_svg(&scene(&[
            line(1, 0.),
            block_schema(5, 3, 3, &[0xff, 0xff, 0xff]),
            block_schema(14, 4, 4, &[0xff]),
            block_schema(5, 3, 3, &[0xff]),
        ]))
        .unwrap();
        assert!(rendered.svg.contains("<path"), "the rest still renders");
        let skipped = rendered
            .warnings
            .iter()
            .filter(|w| w.contains("newer format"))
            .collect::<Vec<_>>();
        assert_eq!(
            skipped,
            ["3 scene blocks use a newer format and were not drawn (tag 5 schema 3/3, tag 14 schema 4/4)"]
        );
        let single = render_svg(&scene(&[block_schema(14, 4, 4, &[])])).unwrap();
        assert!(single.warnings.iter().any(
            |w| w == "1 scene block uses a newer format and was not drawn (tag 14 schema 4/4)"
        ));
        assert!(skipped_blocks_warning(&[]).is_none());
        // Geometry reads the rest of the file too.
        assert!(scene_geometry(&scene(&[block_schema(13, 3, 3, &[0xff])])).is_ok());
    }

    #[test]
    fn a_newer_block_that_older_readers_may_read_is_read() {
        // Current version 3, minimum 2: a forward-compatible extension.
        let mut bytes = line(1, 0.);
        bytes[6] = 3;
        let rendered = render_svg(&scene(&[bytes])).unwrap();
        assert!(rendered.svg.contains("<path"));
        assert!(!rendered.warnings.iter().any(|w| w.contains("newer format")));
    }

    #[test]
    fn a_skipped_author_table_leaves_authors_unmapped() {
        let mut table = author_table(&[(1, [9; 16])]);
        table[5] = 3;
        let bytes = scene(&[table]);
        assert!(scene_authors(&bytes).unwrap().is_empty());
        assert!(render_svg(&bytes).is_ok());
    }

    /// Build a RootText block from a single text run and its style entries.
    fn root_text_block(text: &str, styles: &[((u16, u64), u8)]) -> Vec<u8> {
        let mut body = field(2, 15, &id(10));
        body.extend(field(3, 15, &id(0)));
        body.extend(field(4, 15, &id(0)));
        body.extend(field(5, 4, &0u32.to_le_bytes()));
        let mut value = var(text.len() as u64);
        value.push(1);
        value.extend(text.as_bytes());
        body.extend(field(6, 12, &value));
        let mut record = var(0x0c);
        record.extend((body.len() as u32).to_le_bytes());
        record.extend(body);
        let mut array = var(1);
        array.extend(record);

        let mut map = var(styles.len() as u64);
        for &((author, counter), legacy) in styles {
            let mut key = var(u64::from(author));
            key.extend(var(counter));
            map.extend(key);
            map.extend(field(1, 15, &id(7)));
            map.extend(field(2, 12, &field(1, 1, &[legacy])));
        }

        let mut container = field(1, 12, &field(1, 12, &array));
        container.extend(field(2, 12, &field(1, 12, &map)));
        let mut payload = field(1, 15, &id(0));
        payload.extend(field(2, 12, &container));
        let mut position = (-468.0f64).to_le_bytes().to_vec();
        position.extend(234.0f64.to_le_bytes());
        payload.extend(field(3, 12, &position));
        payload.extend(field(4, 4, &936.0f32.to_le_bytes()));
        let mut block = (payload.len() as u32).to_le_bytes().to_vec();
        block.extend([0, 1, 1, 7]);
        block.extend(payload);
        block
    }

    #[test]
    fn typed_text_renders_as_svg_text_runs_instead_of_warning() {
        let rendered = render_svg(&scene(&[root_text_block("hello\nworld", &[])])).unwrap();
        assert!(rendered.svg.contains(">hello</text>"), "{}", rendered.svg);
        assert!(rendered.svg.contains(">world</text>"));
        // The x origin centres the block: -468 + 1404/2 = 234, plus the
        // 4-unit document margin.
        assert!(rendered.svg.contains("<text x=\"238\""));
        // The old blanket "not rendered" warning is gone; the softer
        // approximate-layout note replaces it.
        assert!(!rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("Text, glyph, or image items are not rendered")));
        assert!(rendered
            .warnings
            .iter()
            .any(|warning| warning.contains("Typed text uses approximate layout")));
    }

    #[test]
    fn a_title_paragraph_uses_a_larger_bold_run() {
        // The style key is the identity the paragraph follows: (0, 0) here.
        let plain = render_svg(&scene(&[root_text_block("Heading", &[])])).unwrap();
        let titled = render_svg(&scene(&[root_text_block("Heading", &[((0, 0), 2)])])).unwrap();
        assert!(!plain.svg.contains("font-weight=\"bold\""));
        assert!(titled.svg.contains("font-weight=\"bold\""));
        assert!(titled.svg.contains("font-size=\"49\""));
    }

    #[test]
    fn a_bullet_paragraph_gets_an_indented_marker() {
        let rendered = render_svg(&scene(&[root_text_block("item", &[((0, 0), 4)])])).unwrap();
        assert!(rendered.svg.contains(">\u{2022}</text>"));
        // The bullet sits one indent step left of its text.
        assert!(rendered.svg.contains("<text x=\"238\""));
        assert!(rendered.svg.contains("<text x=\"278\""));
    }

    #[test]
    fn special_characters_in_typed_text_are_escaped() {
        let rendered = render_svg(&scene(&[root_text_block("a<b&c\"d", &[])])).unwrap();
        assert!(rendered.svg.contains("a&lt;b&amp;c&quot;d"));
        assert!(!rendered.svg.contains("a<b&c"));
    }

    /// A CrdtGlyphItem: subtype 1, a colour, an encoded string and a
    /// counted array of QRectF highlight rectangles.
    fn glyph_block(color: u32, rectangles: &[(f64, f64, f64, f64)]) -> Vec<u8> {
        let mut value = var(2);
        value.push(1);
        value.extend(b"hi");
        let mut rects = var(rectangles.len() as u64);
        for &(x, y, w, h) in rectangles {
            rects.extend(x.to_le_bytes());
            rects.extend(y.to_le_bytes());
            rects.extend(w.to_le_bytes());
            rects.extend(h.to_le_bytes());
        }
        let mut inner = vec![1];
        inner.extend(field(4, 4, &color.to_le_bytes()));
        inner.extend(field(5, 12, &value));
        inner.extend(field(6, 12, &rects));
        let mut env = field(1, 15, &id(1));
        env.extend(field(2, 15, &id(60)));
        env.extend(field(6, 12, &inner));
        let mut block = (env.len() as u32).to_le_bytes().to_vec();
        block.extend([0, 1, 1, 3]);
        block.extend(env);
        block
    }

    #[test]
    fn a_glyph_item_paints_multiplied_highlight_rectangles() {
        let rendered = render_svg(&scene(&[glyph_block(
            3,
            &[(59.0, 738.0, 97.0, 45.0), (10.0, 100.0, 20.0, 45.0)],
        )]))
        .unwrap();
        assert_eq!(
            rendered.svg.matches("mix-blend-mode:multiply").count(),
            2,
            "{}",
            rendered.svg
        );
        // Highlighter yellow, placed at the centred x origin.
        assert!(rendered.svg.contains("fill=\"#f5dd42\""));
        assert!(rendered.svg.contains("x=\"761\""));
        // A highlight outside the page grows the bounds like any other ink.
        let wide = render_svg(&scene(&[glyph_block(3, &[(-900.0, 10.0, 50.0, 45.0)])])).unwrap();
        assert!(wide.bounds.x < 0.0, "{:?}", wide.bounds);
    }

    /// A glyph item `item` highlighting `text` with one rectangle at height `y`,
    /// or, with `text: None`, the payload-free envelope that removes it.
    fn highlight_block(item: u64, text: Option<&str>, y: f64) -> Vec<u8> {
        let mut env = field(1, 15, &id(1));
        env.extend(field(2, 15, &id(item)));
        if let Some(text) = text {
            let mut value = var(text.len() as u64);
            value.push(1);
            value.extend(text.as_bytes());
            let mut rects = var(1);
            for value in [10.0f64, y, 100.0, 40.0] {
                rects.extend(value.to_le_bytes());
            }
            let mut inner = vec![1];
            inner.extend(field(4, 4, &3u32.to_le_bytes()));
            inner.extend(field(5, 12, &value));
            inner.extend(field(6, 12, &rects));
            env.extend(field(6, 12, &inner));
        }
        let mut block = (env.len() as u32).to_le_bytes().to_vec();
        block.extend([0, 1, 1, 3]);
        block.extend(env);
        block
    }

    #[test]
    fn scene_annotations_quote_live_highlights_in_reading_order_and_typed_text() {
        let bytes = scene(&[
            root_text_block("Première idée\n\nSecond point", &[]),
            highlight_block(70, Some("lower sentence"), 900.0),
            highlight_block(71, Some("déjà vu"), 100.0),
            highlight_block(72, Some("removed"), 50.0),
            highlight_block(72, None, 50.0),
            line(80, 10.0),
        ]);
        let found = scene_annotations(&bytes).unwrap();
        let quoted: Vec<(&str, &str)> = found
            .highlights
            .iter()
            .map(|h| (h.text.as_str(), h.color.as_str()))
            .collect();
        assert_eq!(
            quoted,
            [("déjà vu", "yellow"), ("lower sentence", "yellow")]
        );
        assert_eq!(found.typed_text, ["Première idée", "Second point"]);
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        assert_eq!(
            found.stroke_colors.into_iter().collect::<Vec<_>>(),
            [("black".to_owned(), 1)]
        );
        // Strokes only: nothing to quote, but their colours are counted, and a
        // highlighter or an eraser is told apart from a pen.
        let ink = scene_annotations(&scene(&[
            line(80, 10.0),
            line_points_tool(1, 81, 5, &[(0.0, 0.0, 8), (9.0, 0.0, 8)]),
            line_points_tool(1, 82, 6, &[(0.0, 0.0, 8), (9.0, 0.0, 8)]),
        ]))
        .unwrap();
        assert!(ink.highlights.is_empty() && ink.typed_text.is_empty());
        assert_eq!(
            ink.stroke_colors.into_iter().collect::<Vec<_>>(),
            [("black".to_owned(), 1), ("highlighter black".to_owned(), 1)]
        );
        assert_eq!(color_name(7, None), "red");
        assert_eq!(color_name(9, Some(0xff37_7eb9)), "blue (#377eb9)");
    }

    #[test]
    fn a_glyph_item_with_an_unreadable_rectangle_is_rejected() {
        assert!(render_svg(&scene(&[glyph_block(3, &[(f64::NAN, 0.0, 10.0, 10.0)])])).is_err());
        assert!(render_svg(&scene(&[glyph_block(3, &[(0.0, 0.0, 2_000_000.0, 10.0)])])).is_err());
    }

    /// A v3 `SceneImagesTable` mapping each `(key byte, file name)`.
    fn image_table(entries: &[(u8, &str)]) -> Vec<u8> {
        let mut table = var(entries.len() as u64);
        for &(key, name) in entries {
            let mut string = var(name.len() as u64);
            string.push(1);
            string.extend(name.as_bytes());
            let mut register = field(1, 15, &id(20));
            register.extend(field(2, 12, &string));
            let mut flag = field(1, 15, &id(21));
            flag.extend(field(2, 12, &field(1, 1, &[0])));
            let mut entry = vec![key; 16];
            entry.extend(field(1, 12, &register));
            entry.extend(field(2, 12, &flag));
            table.extend(var(0x0c));
            table.extend((entry.len() as u32).to_le_bytes());
            table.extend(entry);
        }
        block_schema(14, 3, 3, &field(1, 12, &table))
    }

    const QUAD: [u32; 6] = [0, 1, 2, 2, 3, 0];

    fn rect_vertices(x: f32, y: f32, w: f32, h: f32) -> Vec<(f32, f32, f32, f32)> {
        vec![
            (x, y, 0.0, 0.0),
            (x + w, y, 1.0, 0.0),
            (x + w, y + h, 1.0, 1.0),
            (x, y + h, 0.0, 1.0),
        ]
    }

    /// A `CrdtImageItem` payload: image reference register, bounds
    /// timestamp, `(x, y, u, v)` vertices and triangle indices.
    fn image_payload(key: u8, vertices: &[(f32, f32, f32, f32)], indices: &[u32]) -> Vec<u8> {
        let mut register = field(1, 15, &id(30));
        register.extend(field(2, 12, &[key; 16]));
        let mut floats = var((vertices.len() * 4) as u64);
        for &(x, y, u, v) in vertices {
            for value in [x, y, u, v] {
                floats.extend(value.to_le_bytes());
            }
        }
        let mut ints = var(indices.len() as u64);
        for &index in indices {
            ints.extend(index.to_le_bytes());
        }
        let mut payload = vec![7];
        payload.extend(field(1, 12, &register));
        payload.extend(field(2, 15, &id(31)));
        payload.extend(field(3, 12, &floats));
        payload.extend(field(4, 12, &ints));
        payload
    }

    fn image_item(parent: u64, item: u64, payload: Option<&[u8]>) -> Vec<u8> {
        image_item_ordered(parent, item, None, None, payload)
    }

    fn image_item_ordered(
        parent: u64,
        item: u64,
        left: Option<u64>,
        right: Option<u64>,
        payload: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut env = field(1, 15, &id(parent));
        env.extend(field(2, 15, &id(item)));
        if let Some(left) = left {
            env.extend(field(3, 15, &id(left)));
        }
        if let Some(right) = right {
            env.extend(field(4, 15, &id(right)));
        }
        if let Some(payload) = payload {
            env.extend(field(6, 12, payload));
        }
        block(15, &env)
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbImage::from_pixel(width, height, image::Rgb([10, 200, 30]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    fn render_with(bytes: &[u8], files: &[(&str, Vec<u8>)]) -> RenderedPage {
        render_svg_with_images(bytes, WIDTH, HEIGHT, &mut |name| {
            Ok(files
                .iter()
                .find(|(file, _)| *file == name)
                .map(|(_, data)| data.clone()))
        })
        .unwrap()
    }

    fn one_image(vertices: &[(f32, f32, f32, f32)], indices: &[u32]) -> Vec<u8> {
        scene(&[
            image_table(&[(5, "a.png")]),
            image_item(1, 70, Some(&image_payload(5, vertices, indices))),
        ])
    }

    fn warned(page: &RenderedPage, text: &str) -> bool {
        page.warnings.iter().any(|warning| warning.contains(text))
    }

    #[test]
    fn an_inserted_image_is_drawn_at_its_scene_rectangle() {
        let bytes = one_image(&rect_vertices(-10.0, -300.0, 900.0, 700.0), &QUAD);
        let rendered = render_with(&bytes, &[("a.png", png(4, 3))]);
        assert!(
            rendered.svg.contains(
                "<image x=\"692\" y=\"-300\" width=\"900\" height=\"700\" preserveAspectRatio=\"none\" xlink:href=\"data:image/png;base64,"
            ),
            "{}",
            rendered.svg
        );
        assert!(rendered
            .svg
            .contains("xmlns:xlink=\"http://www.w3.org/1999/xlink\""));
        assert!(
            rendered.warnings.iter().all(|w| w.starts_with(NOTE_PREFIX)),
            "{:?}",
            rendered.warnings
        );
        // The image grows the canvas like ink does.
        assert_eq!(rendered.bounds.y, -300.0);
        assert_eq!(rendered.bounds.x + rendered.bounds.width, 1592.0);
        assert_eq!(image_files(&bytes).unwrap(), ["a.png"]);
    }

    #[test]
    fn a_rotated_image_maps_its_pixel_grid_onto_the_stored_edges() {
        // Rotated a quarter turn: (0, 0) -> (0, 0), (1, 0) -> (0, 100),
        // (0, 1) -> (-50, 0).
        let vertices = [
            (0.0, 0.0, 0.0, 0.0),
            (0.0, 100.0, 1.0, 0.0),
            (-50.0, 100.0, 1.0, 1.0),
            (-50.0, 0.0, 0.0, 1.0),
        ];
        let rendered = render_with(&one_image(&vertices, &QUAD), &[("a.png", png(4, 2))]);
        assert!(
            rendered.svg.contains(
                "<image width=\"4\" height=\"2\" preserveAspectRatio=\"none\" transform=\"matrix(0 25 -25 0 702 0)\""
            ),
            "{}",
            rendered.svg
        );
    }

    #[test]
    fn images_and_strokes_paint_in_their_groups_link_order() {
        let payload = image_payload(5, &rect_vertices(0.0, 0.0, 50.0, 50.0), &QUAD);
        let files = [("a.png", png(2, 2))];
        // The image is linked left of the stroke, so it paints first.
        let under = render_with(
            &scene(&[
                image_table(&[(5, "a.png")]),
                image_item_ordered(1, 70, None, Some(71), Some(&payload)),
                line_ordered(1, 71, Some(70), None, 0.0),
            ]),
            &files,
        );
        assert!(under.svg.find("<image").unwrap() < under.svg.find("<path").unwrap());
        // Linked the other way round, the stroke is under the image.
        let over = render_with(
            &scene(&[
                image_table(&[(5, "a.png")]),
                line_ordered(1, 71, None, Some(70), 0.0),
                image_item_ordered(1, 70, Some(71), None, Some(&payload)),
            ]),
            &files,
        );
        assert!(over.svg.find("<path").unwrap() < over.svg.find("<image").unwrap());
    }

    #[test]
    fn a_deleted_or_replaced_image_is_not_drawn_and_reports_nothing() {
        let payload = image_payload(5, &rect_vertices(0.0, 0.0, 50.0, 50.0), &QUAD);
        let files = [("a.png", png(2, 2))];
        for deletion in [
            image_item(1, 70, None),
            block(8, &{
                let mut e = field(1, 15, &id(1));
                e.extend(field(2, 15, &id(70)));
                e
            }),
        ] {
            let bytes = scene(&[
                image_table(&[(5, "a.png")]),
                image_item(1, 70, Some(&payload)),
                deletion,
            ]);
            let rendered = render_with(&bytes, &files);
            assert!(!rendered.svg.contains("<image"));
            assert!(rendered.warnings.iter().all(|w| w.starts_with(NOTE_PREFIX)));
            assert!(image_files(&bytes).unwrap().is_empty());
        }
    }

    #[test]
    fn an_image_that_cannot_be_drawn_is_reported_and_the_page_still_renders() {
        let good = rect_vertices(0.0, 0.0, 50.0, 50.0);
        type Files<'a> = Vec<(&'a str, Vec<u8>)>;
        let cases: Vec<(Vec<u8>, Files, &str)> = vec![
            (one_image(&good, &QUAD), vec![], IMAGE_MISSING),
            (
                scene(&[
                    image_table(&[(6, "a.png")]),
                    image_item(1, 70, Some(&image_payload(5, &good, &QUAD))),
                ]),
                vec![("a.png", png(2, 2))],
                IMAGE_UNLISTED,
            ),
            (
                one_image(&good, &QUAD),
                vec![("a.png", b"GIF89a not an allowed format".to_vec())],
                IMAGE_FORMAT,
            ),
            (
                one_image(&good, &QUAD),
                vec![("a.png", png(2, 2)[..40].to_vec())],
                IMAGE_UNDECODABLE,
            ),
            (
                one_image(&good, &QUAD),
                vec![("a.png", vec![0; MAX_IMAGE_BYTES + 1])],
                IMAGE_TOO_LARGE,
            ),
            (
                scene(&[
                    image_table(&[(5, "../a.png")]),
                    image_item(1, 70, Some(&image_payload(5, &good, &QUAD))),
                ]),
                vec![("../a.png", png(2, 2))],
                IMAGE_NAME,
            ),
        ];
        for (bytes, files, reason) in cases {
            let rendered = render_with(&[bytes, line(9, 0.)].concat(), &files);
            assert!(!rendered.svg.contains("<image"), "{reason}");
            assert!(rendered.svg.contains("<path"), "{reason}");
            assert!(
                rendered.warnings.iter().any(|w| w == reason),
                "{reason}: {:?}",
                rendered.warnings
            );
        }
        let failing = render_svg_with_images(&one_image(&good, &QUAD), WIDTH, HEIGHT, &mut |_| {
            Err(anyhow!("disk error"))
        })
        .unwrap();
        assert!(warned(&failing, IMAGE_UNREADABLE_FILE));
    }

    #[test]
    fn image_placement_must_be_the_firmwares_textured_rectangle() {
        let files = [("a.png", png(2, 2))];
        let mut wrong_texture = rect_vertices(0.0, 0.0, 50.0, 50.0);
        wrong_texture[1].2 = 0.5;
        let mut skewed = rect_vertices(0.0, 0.0, 50.0, 50.0);
        skewed[2].0 = 70.0;
        // A parallelogram: opposite sides equal and parallel, no right angle.
        let mut sheared = rect_vertices(0.0, 0.0, 50.0, 50.0);
        sheared[2].0 += 20.0;
        sheared[3].0 += 20.0;
        let good = rect_vertices(0.0, 0.0, 50.0, 50.0);
        let three = good[..3].to_vec();
        let mut non_finite = good.clone();
        non_finite[0].0 = f32::NAN;
        for (vertices, indices) in [
            (wrong_texture, QUAD.to_vec()),
            (skewed, QUAD.to_vec()),
            (sheared, QUAD.to_vec()),
            (three, vec![0, 1, 2]),
            (non_finite, QUAD.to_vec()),
            (good.clone(), vec![0, 1, 2, 0, 1, 2]),
            (good.clone(), vec![0, 1, 2, 2, 3, 4]),
            (good.clone(), vec![0, 2, 1, 2, 3, 1]),
        ] {
            let rendered = render_with(&one_image(&vertices, &indices), &files);
            assert!(!rendered.svg.contains("<image"), "{vertices:?} {indices:?}");
            assert!(warned(&rendered, IMAGE_GEOMETRY), "{:?}", rendered.warnings);
        }
        // Either diagonal may be the shared triangle edge.
        let rendered = render_with(&one_image(&good, &[0, 1, 3, 1, 2, 3]), &files);
        assert!(rendered.svg.contains("<image"));
        // A payload that is not an image record is unreadable, not fatal.
        let rendered = render_with(&scene(&[image_item(1, 70, Some(&[7]))]), &[]);
        assert!(warned(&rendered, IMAGE_UNREADABLE));
    }

    #[test]
    fn a_pages_images_are_bounded_together() {
        // A valid PNG padded to 22 MiB: three placements exceed 64 MiB.
        let mut large = png(2, 2);
        large.resize(22 * 1024 * 1024, 0);
        let payload = image_payload(5, &rect_vertices(0.0, 0.0, 50.0, 50.0), &QUAD);
        let rendered = render_with(
            &scene(&[
                image_table(&[(5, "a.png")]),
                image_item(1, 70, Some(&payload)),
                image_item(1, 71, Some(&payload)),
                image_item(1, 72, Some(&payload)),
            ]),
            &[("a.png", large)],
        );
        assert_eq!(rendered.svg.matches("<image").count(), 2);
        assert!(warned(&rendered, IMAGE_PAGE_BUDGET));
    }

    #[test]
    fn only_live_safely_named_images_are_listed_for_download() {
        let item = |key| image_payload(key, &rect_vertices(0.0, 0.0, 5.0, 5.0), &QUAD);
        let bytes = scene(&[
            image_table(&[
                (1, "a.png"),
                (2, "b.jpg"),
                (3, "c.png"),
                (4, "../x.png"),
                (5, "d.JPEG"),
            ]),
            image_item(1, 70, Some(&item(1))),
            image_item(1, 71, Some(&item(2))),
            image_item(1, 71, None),
            image_item(1, 72, Some(&item(4))),
            image_item(1, 73, Some(&item(5))),
            image_item(1, 74, Some(&item(1))),
        ]);
        assert_eq!(image_files(&bytes).unwrap(), ["a.png", "d.JPEG"]);
        assert!(!valid_image_name("a.gif"));
        assert!(!valid_image_name(".png"));
        assert!(!valid_image_name("a/b.png"));
        assert!(valid_image_name("00000000-0000-4000-8000-000000000001.jpg"));
    }

    #[test]
    fn a_standalone_text_item_is_reported_rather_than_dropped_silently() {
        let mut env = field(1, 15, &id(1));
        env.extend(field(2, 15, &id(70)));
        env.extend(field(6, 12, &[6]));
        let rendered = render_svg(&scene(&[block(6, &env)])).unwrap();
        assert!(warned(&rendered, "standalone text item is not drawn"));
    }

    /// An anchored SceneTreeNode: fields 7..10 carry the anchor identity,
    /// mode byte, anchor threshold and initial origin x.
    fn anchored_node(
        node: u64,
        anchor: (u16, u64),
        mode: u8,
        threshold: f32,
        initial_x: Option<f32>,
    ) -> Vec<u8> {
        let mut body = field(1, 15, &id(node));
        body.extend(field(2, 12, &field(1, 15, &id(0))));
        let mut visible = field(1, 15, &id(0));
        visible.extend(field(2, 1, &[1]));
        body.extend(field(3, 12, &visible));
        let mut anchor_register = field(1, 15, &id(90));
        anchor_register.extend(field(2, 15, &authored_id(anchor.0, anchor.1)));
        body.extend(field(7, 12, &anchor_register));
        let mut mode_register = field(1, 15, &id(92));
        mode_register.extend(field(2, 1, &[mode]));
        body.extend(field(8, 12, &mode_register));
        let mut threshold_register = field(1, 15, &id(93));
        threshold_register.extend(field(2, 4, &threshold.to_le_bytes()));
        body.extend(field(9, 12, &threshold_register));
        if let Some(initial_x) = initial_x {
            let mut x_register = field(1, 15, &id(94));
            x_register.extend(field(2, 4, &initial_x.to_le_bytes()));
            body.extend(field(10, 12, &x_register));
        }
        block(2, &body)
    }

    /// "hello\nworld" from the text helper: characters (0, 10)..(0, 20).
    /// Its lines' baselines are 295.75 and 364.75: box top 234, document
    /// margin 4, a 24-unit block margin per paragraph and 45-unit lines with
    /// the baseline a quarter line above the bottom.
    fn anchored_scene(anchor: (u16, u64), mode: u8, initial_x: Option<f32>) -> Vec<u8> {
        scene(&[
            root_text_block("hello\nworld", &[]),
            group(20, 30),
            anchored_node(30, anchor, mode, 35.75, initial_x),
            line_in(30, 4, 0.0),
        ])
    }

    #[test]
    fn a_vertical_anchor_sits_at_its_characters_baseline_and_stored_x() {
        // 'w', the first character of the second line.
        let rendered = render_svg(&anchored_scene((0, 16), 2, Some(-464.0))).unwrap();
        assert!(
            rendered
                .svg
                .contains("<g transform=\"translate(-464 364.75)\">"),
            "{}",
            rendered.svg
        );
        assert!(warned(
            &rendered,
            "1 anchored group(s) follow their typed-text anchor"
        ));
        // Without a stored initial x the character's own x is used, which
        // here is the text box plus the document margin: -468 + 4.
        let rendered = render_svg(&anchored_scene((0, 16), 2, None)).unwrap();
        assert!(rendered.svg.contains("translate(-464 364.75)"));
    }

    #[test]
    fn other_anchor_modes_also_follow_the_character_horizontally() {
        // 'o', one approximate advance (16.24) right of the line start.
        let rendered = render_svg(&anchored_scene((0, 17), 1, Some(-464.0))).unwrap();
        assert!(
            rendered.svg.contains("translate(-447.76 364.75)"),
            "{}",
            rendered.svg
        );
    }

    #[test]
    fn reserved_anchor_identities_are_the_first_and_last_character() {
        let start = render_svg(&anchored_scene(ANCHOR_TEXT_START, 2, Some(-464.0))).unwrap();
        assert!(
            start.svg.contains("translate(-464 295.75)"),
            "{}",
            start.svg
        );
        let end = render_svg(&anchored_scene(ANCHOR_TEXT_END, 2, Some(-464.0))).unwrap();
        assert!(end.svg.contains("translate(-464 364.75)"), "{}", end.svg);
    }

    #[test]
    fn a_missing_anchor_is_reported_and_leaves_the_group_in_place() {
        let rendered = render_svg(&anchored_scene((0, 99), 2, Some(-464.0))).unwrap();
        assert!(!rendered.svg.contains("translate("));
        assert!(rendered.svg.contains("<path"));
        assert!(warned(&rendered, "typed-text anchor is missing"));
    }

    #[test]
    fn a_non_finite_anchor_value_is_rejected() {
        assert!(render_svg(&anchored_scene((0, 16), 2, Some(f32::NAN))).is_err());
        let bytes = scene(&[
            root_text_block("hello", &[]),
            group(20, 30),
            anchored_node(30, (0, 10), 2, f32::INFINITY, Some(-464.0)),
        ]);
        assert!(render_svg(&bytes).is_err());
    }

    #[test]
    fn an_anchored_image_grows_the_canvas_where_it_is_placed() {
        let payload = image_payload(5, &rect_vertices(-4.0, 2000.0, 936.0, 100.0), &QUAD);
        let rendered = render_with(
            &scene(&[
                root_text_block("hello\nworld", &[]),
                group(20, 30),
                anchored_node(30, (0, 16), 2, 35.75, Some(-464.0)),
                image_table(&[(5, "a.png")]),
                image_item(30, 70, Some(&payload)),
            ]),
            &[("a.png", png(2, 2))],
        );
        assert!(rendered.svg.contains("<image x=\"698\" y=\"2000\""));
        // 364.75 + 2000 + 100: the translated bottom edge.
        assert_eq!(rendered.bounds.y + rendered.bounds.height, 2464.75);
    }

    #[test]
    fn an_eraser_in_an_anchored_group_masks_in_the_groups_own_coordinates() {
        let rendered = render_svg(&scene(&[
            root_text_block("hello\nworld", &[]),
            group(20, 30),
            anchored_node(30, (0, 16), 2, 35.75, Some(-464.0)),
            line_in(30, 4, 0.0),
            line_points_tool(30, 5, 6, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
        ]))
        .unwrap();
        // The mask applies inside the translation, so its rectangle is the
        // page bounds shifted back by the group's offset.
        assert!(
            rendered
                .svg
                .contains("<g transform=\"translate(-464 364.75)\">\n<g mask=\"url(#eraser-0)\">"),
            "{}",
            rendered.svg
        );
        assert!(rendered.svg.contains("<rect x=\"464\" y=\"-364.75\""));
    }

    #[test]
    fn the_highlighter_multiplies_and_the_pencil_varies_with_pressure() {
        // Tool 18 (Highlighterv2) is translucent and multiplied.
        let highlighter = render_svg(&scene(&[line_points_tool(
            1,
            1,
            18,
            &[(0.0, 10.0, 8), (5.0, 15.0, 8)],
        )]))
        .unwrap();
        assert!(highlighter
            .svg
            .contains("<g opacity=\"0.35\" style=\"mix-blend-mode:multiply\">"));
        // The segments inside are solid, so their overlaps do not darken.
        assert!(!highlighter.svg.contains("stroke-opacity"));
        assert!(!highlighter.svg.contains("fill-opacity"));

        // Tool 14 (Pencilv2) varies its opacity with the recorded pressure.
        let pencil = render_svg(&scene(&[line_points_tool(
            1,
            1,
            14,
            &[(0.0, 10.0, 8), (5.0, 15.0, 8)],
        )]))
        .unwrap();
        assert!(pencil.svg.contains("mix-blend-mode:multiply"));
        // The fixture stores pressure 255, so the stroke is near full opacity.
        assert!(pencil.svg.contains("<g opacity=\"0.85\""));

        // A ballpoint stays solid, with no blend mode at all.
        let ballpoint = render_svg(&scene(&[line_points_tool(
            1,
            1,
            2,
            &[(0.0, 10.0, 8), (5.0, 15.0, 8)],
        )]))
        .unwrap();
        assert!(!ballpoint.svg.contains("mix-blend-mode"));
        assert!(!ballpoint.svg.contains("opacity"));
    }

    #[test]
    fn a_low_pressure_pencil_sample_is_fainter_than_a_full_pressure_one() {
        // line_points_tool stores pressure 255; build a zero-pressure twin.
        let mut faint = line_points_tool(1, 1, 14, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]);
        // The two 14-byte point records end the block; clear both pressures.
        let length = faint.len();
        faint[length - 1] = 0;
        faint[length - 15] = 0;
        let rendered = render_svg(&scene(&[faint])).unwrap();
        // 0.85 * 0.35 = 0.2975, rounded to three places by xml_num.
        assert!(
            rendered.svg.contains("<g opacity=\"0.297\"")
                || rendered.svg.contains("<g opacity=\"0.298\""),
            "{}",
            rendered.svg
        );
    }

    #[test]
    fn the_eraser_mask_stays_fully_opaque_whatever_the_brush() {
        let rendered = render_svg(&scene(&[
            line_points_tool(1, 1, 14, &[(0.0, 10.0, 8), (5.0, 15.0, 8)]),
            line_points_tool(1, 2, 6, &[(0.0, 10.0, 12), (5.0, 15.0, 12)]),
        ]))
        .unwrap();
        let mask = rendered
            .svg
            .split("<mask id=")
            .nth(1)
            .and_then(|part| part.split("</mask>").next())
            .expect("an eraser mask");
        // Mask geometry must be solid white/black, never blended or faded.
        assert!(!mask.contains("mix-blend-mode"), "{mask}");
        assert!(!mask.contains("opacity"), "{mask}");
    }
}
