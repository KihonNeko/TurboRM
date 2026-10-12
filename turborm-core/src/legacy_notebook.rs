//! Bounded reader for the flat layer/stroke `.rm` formats used before scenes.

use crate::notebook::{CanvasBounds, RenderedPage, SceneGeometry};
use anyhow::{anyhow, ensure, Result};

const PREFIX: &[u8] = b"reMarkable .lines file, version=";
const HEADER_LEN: usize = 43;
const MAX_FILE: usize = 100 * 1024 * 1024;
const MAX_LAYERS: usize = 100_000;
const MAX_STROKES: usize = 1_000_000;
const MAX_POINTS: usize = 250_000;
const MAX_SVG: usize = 32 * 1024 * 1024;
const MAX_COORD: f32 = 1_000_000.0;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: HEADER_LEN,
        }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| anyhow!("legacy notebook offset overflow"))?;
        ensure!(end <= self.bytes.len(), "truncated legacy notebook");
        let value = &self.bytes[self.at..end];
        self.at = end;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

pub(super) fn version(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < HEADER_LEN || !bytes.starts_with(PREFIX) {
        return None;
    }
    match bytes.get(PREFIX.len()).copied() {
        Some(b'3') => Some(3),
        Some(b'5') => Some(5),
        _ => None,
    }
}

pub(super) fn geometry(bytes: &[u8]) -> Result<SceneGeometry> {
    validate_header(bytes)?;
    Ok(SceneGeometry::default())
}

fn validate_header(bytes: &[u8]) -> Result<u8> {
    ensure!(
        bytes.len() <= MAX_FILE,
        "legacy notebook exceeds 100 MiB limit"
    );
    ensure!(
        bytes.len() >= HEADER_LEN,
        "truncated legacy notebook header"
    );
    let version = version(bytes).ok_or_else(|| anyhow!("unsupported legacy notebook version"))?;
    let expected = format!("reMarkable .lines file, version={version}          ");
    ensure!(
        &bytes[..HEADER_LEN] == expected.as_bytes(),
        "invalid legacy notebook header"
    );
    Ok(version)
}

fn num(v: f32) -> String {
    let mut s = format!("{v:.3}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

fn color(index: u32) -> &'static str {
    match index {
        0 => "#111111",
        1 => "#777777",
        2 => "#ffffff",
        _ => "#111111",
    }
}

pub(super) fn render(bytes: &[u8], width: f32, height: f32) -> Result<RenderedPage> {
    ensure!(
        width.is_finite()
            && height.is_finite()
            && width > 0.0
            && height > 0.0
            && width <= MAX_COORD
            && height <= MAX_COORD,
        "invalid page dimensions"
    );
    let version = validate_header(bytes)?;
    let mut reader = Reader::new(bytes);
    let layers = reader.u32()? as usize;
    ensure!(layers <= MAX_LAYERS, "too many legacy notebook layers");
    let mut strokes = 0usize;
    let mut points = 0usize;
    let mut body = String::new();
    let mut warnings = Vec::new();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0.0f32, 0.0f32, width, height);
    for _ in 0..layers {
        let layer_strokes = reader.u32()? as usize;
        strokes = strokes
            .checked_add(layer_strokes)
            .ok_or_else(|| anyhow!("too many legacy strokes"))?;
        ensure!(strokes <= MAX_STROKES, "too many legacy notebook strokes");
        for _ in 0..layer_strokes {
            let pen = reader.u32()?;
            let ink = reader.u32()?;
            let _unknown = reader.u32()?;
            let base_width = reader.f32()?;
            ensure!(
                base_width.is_finite() && (0.0..=16384.0).contains(&base_width),
                "invalid legacy stroke width"
            );
            if version == 5 {
                let _ = reader.u32()?;
            }
            let stroke_points = reader.u32()? as usize;
            points = points
                .checked_add(stroke_points)
                .ok_or_else(|| anyhow!("too many legacy points"))?;
            ensure!(points <= MAX_POINTS, "too many legacy notebook points");
            let erased = matches!(pen, 6 | 8 | 18);
            if erased {
                warnings.push(format!("Legacy eraser tool {pen} was omitted because its destructive geometry cannot be reconstructed safely"));
            }
            let opacity = if matches!(pen, 5 | 17) { "0.35" } else { "1" };
            let mut previous: Option<(f32, f32, f32)> = None;
            for _ in 0..stroke_points {
                let x = reader.f32()?;
                let y = reader.f32()?;
                let speed = reader.f32()?;
                let direction = reader.f32()?;
                let point_width = reader.f32()?;
                let pressure = reader.f32()?;
                ensure!(
                    [x, y, speed, direction, point_width, pressure]
                        .iter()
                        .all(|v| v.is_finite()),
                    "non-finite legacy point"
                );
                ensure!(
                    x.abs() <= MAX_COORD && y.abs() <= MAX_COORD,
                    "legacy coordinates exceed preview bounds"
                );
                let stroke_width = if version == 5 && matches!(pen, 12 | 14) {
                    point_width * base_width
                } else {
                    point_width
                };
                ensure!(
                    stroke_width.is_finite() && stroke_width.abs() <= 16384.0,
                    "invalid legacy point width"
                );
                let stroke_width = stroke_width.max(0.25);
                if erased {
                    continue;
                }
                if let Some((px, py, pw)) = previous {
                    let radius = (pw + stroke_width) / 4.0;
                    min_x = min_x.min(px.min(x) - radius);
                    min_y = min_y.min(py.min(y) - radius);
                    max_x = max_x.max(px.max(x) + radius);
                    max_y = max_y.max(py.max(y) + radius);
                    body.push_str(&format!("<path d=\"M {} {} L {} {}\" fill=\"none\" stroke=\"{}\" stroke-opacity=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n", num(px), num(py), num(x), num(y), color(ink), opacity, num((pw + stroke_width) / 2.0)));
                } else {
                    let radius = stroke_width / 2.0;
                    min_x = min_x.min(x - radius);
                    min_y = min_y.min(y - radius);
                    max_x = max_x.max(x + radius);
                    max_y = max_y.max(y + radius);
                    body.push_str(&format!(
                        "<circle cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{}\" fill-opacity=\"{}\"/>\n",
                        num(x),
                        num(y),
                        num(radius),
                        color(ink),
                        opacity
                    ));
                }
                previous = Some((x, y, stroke_width));
                ensure!(body.len() <= MAX_SVG, "rendered SVG exceeds 32 MiB limit");
            }
        }
    }
    ensure!(reader.at == bytes.len(), "trailing data in legacy notebook");
    // Both are standing facts about this reader, not content problems.
    warnings.push(format!(
        "{}Legacy pen texture and blending are approximated with variable-width round strokes",
        crate::notebook::NOTE_PREFIX
    ));
    warnings.push(format!(
        "{}{}",
        crate::notebook::NOTE_PREFIX,
        crate::notebook::TEMPLATE_OUTSIDE_SCENE
    ));
    let bounds = CanvasBounds {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    };
    Ok(RenderedPage { svg: format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"{} {} {} {}\">\n{body}</svg>\n", num(bounds.width), num(bounds.height), num(bounds.x), num(bounds.y), num(bounds.width), num(bounds.height)), warnings, bounds })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(version: u8, pen: u32, point_width: f32, base_width: f32) -> Vec<u8> {
        let mut b = format!("reMarkable .lines file, version={version}          ").into_bytes();
        b.extend(1u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(pen.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(base_width.to_le_bytes());
        if version == 5 {
            b.extend(0u32.to_le_bytes());
        }
        b.extend(2u32.to_le_bytes());
        for (x, y) in [(10f32, 20f32), (30f32, 40f32)] {
            for v in [x, y, 0.0, 0.0, point_width, 1.0] {
                b.extend(v.to_le_bytes());
            }
        }
        b
    }
    #[test]
    fn renders_real_flat_v3_and_left_origin() {
        let r = render(&file(3, 2, 2.0, 4.0), 1404.0, 1872.0).unwrap();
        assert!(r.svg.contains("M 10 20 L 30 40"));
    }
    #[test]
    fn v5_new_pen_multiplies_legacy_width() {
        for pen in [12, 14] {
            let r = render(&file(5, pen, 2.0, 3.0), 1404.0, 1872.0).unwrap();
            assert!(r.svg.contains("stroke-width=\"6\""));
        }
        let pencil = render(&file(5, 13, 2.0, 3.0), 1404.0, 1872.0).unwrap();
        assert!(pencil.svg.contains("stroke-width=\"2\""));
    }
    #[test]
    fn rejects_truncation_counts_and_non_finite_points() {
        let mut truncated = file(5, 12, 2.0, 3.0);
        truncated.pop();
        assert!(render(&truncated, 1404.0, 1872.0).is_err());
        let mut count = b"reMarkable .lines file, version=3          ".to_vec();
        count.extend(((MAX_LAYERS + 1) as u32).to_le_bytes());
        assert!(render(&count, 1404.0, 1872.0).is_err());
        let bad = file(3, 2, f32::NAN, 1.0);
        assert!(render(&bad, 1404.0, 1872.0).is_err());
    }
}
