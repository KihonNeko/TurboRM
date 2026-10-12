//! Local rendering for reMarkable paper templates.
//!
//! Asset files are discovered, in order, from `REMARKABLE_TEMPLATES_DIR`,
//! `$XDG_DATA_HOME/remarkable-desktop/templates` and
//! `~/.local/share/remarkable-desktop/templates`. No asset is bundled with this
//! program. When no asset file exists, [`builtin`] draws the standard geometric
//! papers procedurally from measurements taken from the firmware's own
//! templates; everything else reports that it is unavailable locally.
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::Value;
use std::{collections::HashMap, fs, path::PathBuf};
use turborm_core::job;

const MAX_ASSET: u64 = 2 * 1024 * 1024;
const MAX_EXPR: usize = 512;
const MAX_ITEMS: usize = 4096;
const MAX_INSTANCES: usize = 4096;
const MAX_PATH_PARTS: usize = 100_000;
const MAX_SVG: usize = 4 * 1024 * 1024;
const MAX_COORD: f64 = 1_000_000.0;

/// Render the v2 `cPages` template for `page_id` from a user-provided local
/// template directory. Empty and `Blank` templates intentionally return none.
pub fn render(
    content: &Value,
    page_id: &str,
    _index: usize,
    base_width: f32,
    base_height: f32,
) -> Result<Option<String>> {
    ensure!(
        base_width.is_finite() && base_width > 0.0 && f64::from(base_width) <= MAX_COORD,
        "invalid template width"
    );
    ensure!(
        base_height.is_finite() && base_height > 0.0 && f64::from(base_height) <= MAX_COORD,
        "invalid template height"
    );
    let Some(name) = template_for_page(content, page_id)? else {
        return Ok(None);
    };
    if name.is_empty() || name == "Blank" {
        return Ok(None);
    }
    safe_stem(&name)?;
    let (width, height) = (f64::from(base_width), f64::from(base_height));
    if let Some(bytes) = locate_asset(&name)? {
        return render_asset(&bytes, width, height).map(Some);
    }
    // No asset file: draw the standard geometric papers ourselves.
    builtin(&name, width, height)
        .map(Some)
        .ok_or_else(|| anyhow!("Template {name:?} is not available locally"))
}

/// Directories searched for `<name>.template`, most specific first.
///
/// `REMARKABLE_TEMPLATES_DIR` still wins so an existing setup keeps working.
/// The other two are the ordinary XDG data locations, so copying the device's
/// own templates into `~/.local/share/remarkable-desktop/templates` is enough.
pub fn search_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |path: PathBuf| {
        if path.is_absolute() && !roots.contains(&path) {
            roots.push(path);
        }
    };
    if let Some(value) = std::env::var_os("REMARKABLE_TEMPLATES_DIR") {
        push(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        push(PathBuf::from(value).join("remarkable-desktop/templates"));
    }
    if let Some(value) = std::env::var_os("HOME") {
        push(PathBuf::from(value).join(".local/share/remarkable-desktop/templates"));
    }
    roots
}

/// Read `<name>.template` from the first search path that holds it.
///
/// The name has already passed [`safe_stem`], so it cannot escape the
/// directory. Each candidate must still be a plain regular file within bounds.
fn locate_asset(name: &str) -> Result<Option<Vec<u8>>> {
    for root in search_paths() {
        let path = root.join(format!("{name}.template"));
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        ensure!(
            meta.file_type().is_file() && !meta.file_type().is_symlink(),
            "template asset must be a regular file"
        );
        ensure!(meta.len() <= MAX_ASSET, "template asset exceeds 2 MiB");
        return job::read_bounded(&path, MAX_ASSET as usize)
            .with_context(|| format!("Template asset {name:?} could not be read"))
            .map(Some);
    }
    Ok(None)
}

fn template_for_page(content: &Value, page_id: &str) -> Result<Option<String>> {
    let Some(pages) = content.pointer("/cPages/pages").and_then(Value::as_array) else {
        return Ok(None);
    };
    ensure!(pages.len() <= 100_000, "too many cPages records");
    let mut found = None;
    for page in pages {
        if page.get("id").and_then(Value::as_str) != Some(page_id) {
            continue;
        }
        ensure!(found.is_none(), "duplicate cPages page ID");
        found = Some(match page.pointer("/template/value") {
            None => "Blank".to_owned(),
            Some(value) => value
                .as_str()
                .ok_or_else(|| anyhow!("page template register is not a string"))?
                .to_owned(),
        });
    }
    Ok(found)
}

fn safe_stem(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 255,
        "invalid template name"
    );
    ensure!(name != "." && name != "..", "invalid template name");
    ensure!(
        !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\')),
        "unsafe template name"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Built-in geometric papers
// ---------------------------------------------------------------------------
//
// Every number below was measured from the reMarkable firmware's own
// `.template` JSON (image 3.28.0.172, `usr/share/remarkable/templates/`), by
// reading the `constants` and `boundingBox` values of each file. No firmware
// asset is copied into this repository; only the measured geometry is encoded.
//
// Measured values, portrait (`P …`) unless stated:
//   Dots small ("P Dots S"):  42.5 spacing, 1×1 square dot, x offset -22 + 1
//   Dots large ("P Dots large"): 78 spacing, 1×1 dot, x offset centred -1 - 2
//   Lines small / medium / large: row heights 52 / 78.7 / 97,
//     first baseline y 0 / 177.8 / 162, band centred on the short side
//   Grid small / medium / large: 52 / 78.3 / 104, vertical-line x offsets
//     -25 / 0 / -78 from the centred band
//   Margin small / medium / large: the matching Lines geometry plus a single
//     vertical rule at x = 478 inside the centred band (243 for landscape)
//   Landscape (`LS …`) variants use the same spacings with the band spanning
//     the full width and no centring offset.
//
// Firmware paths are stroked with the default 1px rule; the shared renderer
// paints them in #b7b7b7, which is what `render_asset` also emits.

/// Measured-from-firmware geometry for one built-in paper.
struct Geometry {
    /// Row spacing / grid pitch in paper units.
    pitch: f64,
    /// First horizontal baseline, measured from the top of the page.
    first_y: f64,
    /// Vertical-line pitch, when the paper draws them.
    column_pitch: Option<f64>,
    /// Offset applied to the vertical-line band, relative to the centred band.
    column_offset: f64,
    /// Single margin rule inside the centred band, when the paper has one.
    margin_x: Option<f64>,
    /// Square dot side, when the paper is a dot grid rather than lines.
    dot: Option<f64>,
    /// Offset applied to the dot lattice origin.
    dot_offset: f64,
    /// True when `dot_offset` is an absolute page x rather than an offset from
    /// the centred band. "P Dots S" and the landscape dot papers use a fixed
    /// page offset; "P Dots large" hangs off the centred band.
    dot_offset_absolute: bool,
    /// True when the band spans the full page width instead of a centred
    /// square the height of the short side.
    full_width: bool,
}

const DOT_STROKE: &str = "#000000";

/// Geometry for the standard geometric papers, or `None` for a paper that has
/// no generator (planners, music staves, storyboards and anything with text).
fn geometry(name: &str) -> Option<Geometry> {
    let base = Geometry {
        pitch: 0.0,
        first_y: 0.0,
        column_pitch: None,
        column_offset: 0.0,
        margin_x: None,
        dot: None,
        dot_offset: 0.0,
        dot_offset_absolute: false,
        full_width: false,
    };
    // Landscape papers span the page width and carry no centring offset.
    let landscape = name.starts_with("LS ");
    let margin_rule = if landscape { 243.0 } else { 478.0 };
    let lines = |pitch: f64, first_y: f64| Geometry {
        pitch,
        first_y,
        full_width: landscape,
        ..base
    };
    Some(match name {
        // Dot grids. The firmware square is 1×1 at each lattice point.
        // Measured: offsetX = paperOriginX - 22, box x = offsetX + 1.
        "P Dots S" | "P Dots S top" | "P Dots S bottom" => Geometry {
            pitch: 42.5,
            dot: Some(1.0),
            dot_offset: -21.0,
            dot_offset_absolute: true,
            ..base
        },
        // Measured: magicOffsetX = centred band - 1, box x = that - 2.
        "P Dots large" => Geometry {
            pitch: 78.0,
            dot: Some(1.0),
            dot_offset: -3.0,
            ..base
        },
        // Measured: magicOffsetX = -6, box x = offsetX - 0.5.
        "LS Dots top" | "LS Dots bottom" => Geometry {
            pitch: 42.5,
            dot: Some(1.0),
            dot_offset: -6.5,
            dot_offset_absolute: true,
            full_width: true,
            ..base
        },
        // Ruled papers.
        "P Lines small" => lines(52.0, 0.0),
        "P Lines medium" => lines(78.7, 177.8),
        "P Lines large" => lines(97.0, 162.0),
        "LS Lines small" | "LS Lines top" | "LS Lines bottom" => lines(52.0, 0.0),
        "LS Lines medium" => lines(78.7, 177.8),
        "LS Lines large" => lines(97.0, 162.0),
        // Ruled papers with a single margin rule.
        "P Margin small" => Geometry {
            margin_x: Some(margin_rule),
            ..lines(52.0, 154.0)
        },
        "P Margin medium" => Geometry {
            margin_x: Some(margin_rule),
            ..lines(78.7, 177.8)
        },
        "P Margin large" => Geometry {
            margin_x: Some(margin_rule),
            ..lines(97.0, 162.0)
        },
        "LS Margin small" => Geometry {
            margin_x: Some(margin_rule),
            ..lines(52.0, 154.0)
        },
        "LS Margin medium" => Geometry {
            margin_x: Some(margin_rule),
            ..lines(78.7, 177.8)
        },
        // Square grids.
        "P Grid small" => Geometry {
            column_pitch: Some(52.0),
            column_offset: -25.0,
            ..lines(52.0, 0.0)
        },
        "P Grid medium" => Geometry {
            column_pitch: Some(78.3),
            column_offset: 0.0,
            ..lines(78.3, 0.0)
        },
        "P Grid large" => Geometry {
            column_pitch: Some(104.0),
            column_offset: -78.0,
            ..lines(104.0, 0.0)
        },
        "LS Grid small" => Geometry {
            column_pitch: Some(52.0),
            ..lines(52.0, 0.0)
        },
        "LS Grid medium" | "LS Grid top" | "LS Grid bottom" => Geometry {
            column_pitch: Some(78.3),
            ..lines(78.3, 0.0)
        },
        "LS Grid large" => Geometry {
            column_pitch: Some(104.0),
            ..lines(104.0, 0.0)
        },
        _ => return None,
    })
}

/// Draw one of the standard geometric papers at `width` × `height`.
///
/// Returns `None` for any name without a generator, so the caller can say the
/// template is unavailable rather than silently drawing the wrong paper.
pub fn builtin(name: &str, width: f64, height: f64) -> Option<String> {
    let g = geometry(name)?;
    if !(g.pitch.is_finite() && g.pitch > 0.0) {
        return None;
    }
    // Portrait firmware templates position their band at
    // `templateWidth / 2 - templateHeight / 2`, i.e. a band the page's *height*
    // wide, centred horizontally. On a taller-than-wide page that origin is
    // negative and the band overhangs both edges; the viewBox clips it. That is
    // what keeps the ruling at the same physical pitch however the page turns.
    let band = if g.full_width {
        (0.0, width)
    } else {
        ((width - height) / 2.0, height)
    };
    let mut svg = String::new();
    let mut parts = 0usize;
    let bounded = |svg: &mut String, part: String, parts: &mut usize| -> bool {
        *parts += 1;
        if *parts > MAX_INSTANCES || svg.len() > MAX_SVG {
            return false;
        }
        svg.push_str(&part);
        true
    };
    if let Some(dot) = g.dot {
        // A lattice of small filled squares, exactly as the firmware paths do.
        let x0 = if g.dot_offset_absolute {
            g.dot_offset
        } else {
            band.0 + g.dot_offset
        };
        let mut y = g.first_y;
        while y < height {
            let mut x = x0;
            while x < width {
                // A lattice point just off the left edge is drawn as the
                // firmware draws it; the viewBox clips what falls outside.
                if !bounded(
                    &mut svg,
                    format!(
                        "<rect x=\"{x:.4}\" y=\"{y:.4}\" width=\"{dot}\" height=\"{dot}\" fill=\"{DOT_STROKE}\" stroke=\"none\"/>"
                    ),
                    &mut parts,
                ) {
                    return Some(wrap(svg, width, height));
                }
                x += g.pitch;
            }
            y += g.pitch;
        }
        return Some(wrap(svg, width, height));
    }
    let mut y = g.first_y;
    while y < height {
        if !bounded(
            &mut svg,
            format!(
                "<path d=\"M {:.4} {y:.4} L {:.4} {y:.4}\"/>",
                band.0,
                band.0 + band.1
            ),
            &mut parts,
        ) {
            return Some(wrap(svg, width, height));
        }
        y += g.pitch;
    }
    if let Some(pitch) = g.column_pitch {
        if pitch.is_finite() && pitch > 0.0 {
            let mut x = band.0 + g.column_offset;
            // Start at the leftmost column that is still on the page.
            while x - pitch > 0.0 {
                x -= pitch;
            }
            while x < width {
                if !bounded(
                    &mut svg,
                    format!("<path d=\"M {x:.4} 0 L {x:.4} {height:.4}\"/>"),
                    &mut parts,
                ) {
                    return Some(wrap(svg, width, height));
                }
                x += pitch;
            }
        }
    }
    if let Some(margin) = g.margin_x {
        let x = band.0 + margin;
        if (0.0..width).contains(&x) {
            svg.push_str(&format!("<path d=\"M {x:.4} 0 L {x:.4} {height:.4}\"/>"));
        }
    }
    Some(wrap(svg, width, height))
}

/// Wrap generated paths in the same document shell `render_asset` produces, so
/// a built-in paper and an asset-file paper composite identically.
fn wrap(body: String, width: f64, height: f64) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><g fill=\"none\" stroke=\"#b7b7b7\" stroke-width=\"1\">{body}</g></svg>"
    )
}

fn render_asset(bytes: &[u8], width: f64, height: f64) -> Result<String> {
    let spec: Value = serde_json::from_slice(bytes).context("invalid template JSON")?;
    allowed_keys(
        &spec,
        &[
            "name",
            "author",
            "templateVersion",
            "formatVersion",
            "categories",
            "orientation",
            "constants",
            "items",
            "supportedDevices",
            "iconData",
        ],
        "template",
    )?;
    ensure!(
        spec.get("formatVersion").and_then(Value::as_u64) == Some(1),
        "unsupported template formatVersion"
    );
    let mut vars = HashMap::from([
        ("templateWidth".to_owned(), width),
        ("templateHeight".to_owned(), height),
        ("parentWidth".to_owned(), width),
        ("parentHeight".to_owned(), height),
        // Firmware templates use the paper coordinate system's top-left origin.
        ("paperOriginX".to_owned(), 0.0),
        ("paperOriginY".to_owned(), 0.0),
    ]);
    if let Some(constants) = spec.get("constants") {
        let constants = constants
            .as_array()
            .ok_or_else(|| anyhow!("template constants must be an array"))?;
        ensure!(constants.len() <= 256, "too many template constants");
        for value in constants {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("invalid template constant"))?;
            ensure!(object.len() == 1, "template constant must have one name");
            let (name, value) = object.iter().next().unwrap();
            ensure!(
                name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "invalid constant name"
            );
            let number = number(value, &vars)?;
            vars.insert(name.clone(), number);
        }
    }
    let items = spec
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("template has no items"))?;
    let mut state = State {
        items: 0,
        instances: 0,
        path_parts: 0,
        svg: String::new(),
    };
    render_items(items, &vars, 0.0, 0.0, width, height, &mut state)?;
    ensure!(
        state.svg.len() <= MAX_SVG,
        "rendered template exceeds 4 MiB"
    );
    Ok(format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><g fill=\"none\" stroke=\"#b7b7b7\" stroke-width=\"1\">{}</g></svg>", state.svg))
}

struct State {
    items: usize,
    instances: usize,
    path_parts: usize,
    svg: String,
}

fn render_items(
    items: &[Value],
    vars: &HashMap<String, f64>,
    ox: f64,
    oy: f64,
    pw: f64,
    ph: f64,
    state: &mut State,
) -> Result<()> {
    ensure!(items.len() <= MAX_ITEMS, "too many template items");
    for item in items {
        state.items += 1;
        ensure!(state.items <= MAX_ITEMS, "too many nested template items");
        match item.get("type").and_then(Value::as_str) {
            Some("group") => render_group(item, vars, ox, oy, pw, ph, state)?,
            Some("path") => render_path(item, vars, ox, oy, state)?,
            Some("text") => bail!("template text items are not supported"),
            Some(kind) => bail!("unsupported template item type {kind:?}"),
            None => bail!("template item has no type"),
        }
    }
    Ok(())
}

fn render_group(
    item: &Value,
    vars: &HashMap<String, f64>,
    ox: f64,
    oy: f64,
    pw: f64,
    ph: f64,
    state: &mut State,
) -> Result<()> {
    allowed_keys(
        item,
        &["type", "id", "boundingBox", "repeat", "children"],
        "group",
    )?;
    let b = item
        .get("boundingBox")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("group has no boundingBox"))?;
    ensure!(
        b.keys()
            .all(|key| matches!(key.as_str(), "x" | "y" | "width" | "height")),
        "unsupported group boundingBox property"
    );
    let mut local = vars.clone();
    local.insert("parentWidth".into(), pw);
    local.insert("parentHeight".into(), ph);
    let x = number(b.get("x").unwrap_or(&Value::from(0)), &local)?;
    let y = number(b.get("y").unwrap_or(&Value::from(0)), &local)?;
    let w = number(
        b.get("width")
            .ok_or_else(|| anyhow!("group has no width"))?,
        &local,
    )?;
    let h = number(
        b.get("height")
            .ok_or_else(|| anyhow!("group has no height"))?,
        &local,
    )?;
    ensure!(w > 0.0 && h > 0.0, "group dimensions must be positive");
    let children = item
        .get("children")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("group has no children"))?;
    let (rows, cols, row_step, col_step) = repeat_counts(item.get("repeat"), &local, x, y, w, h)?;
    local.insert("parentWidth".into(), w);
    local.insert("parentHeight".into(), h);
    ensure!(
        rows.checked_mul(cols).is_some_and(|n| n <= MAX_INSTANCES),
        "template repeat exceeds limit"
    );
    for row in 0..rows {
        for col in 0..cols {
            state.instances += 1;
            ensure!(
                state.instances <= MAX_INSTANCES,
                "too many template instances"
            );
            render_items(
                children,
                &local,
                ox + x + col as f64 * col_step,
                oy + y + row as f64 * row_step,
                w,
                h,
                state,
            )?;
        }
    }
    Ok(())
}

fn repeat_counts(
    repeat: Option<&Value>,
    vars: &HashMap<String, f64>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(usize, usize, f64, f64)> {
    let Some(repeat) = repeat else {
        return Ok((1, 1, h, w));
    };
    let o = repeat
        .as_object()
        .ok_or_else(|| anyhow!("repeat must be an object"))?;
    ensure!(
        o.keys()
            .all(|key| matches!(key.as_str(), "rows" | "columns")),
        "unsupported repeat property"
    );
    let count = |v: Option<&Value>, vertical: bool| -> Result<(usize, f64)> {
        let step = if vertical { h } else { w };
        let Some(v) = v else { return Ok((1, step)) };
        if let Some(s) = v.as_str() {
            if s == "up" && vertical {
                return Ok(((y.max(0.0) / h).floor() as usize + 1, -h));
            }
            if matches!((vertical, s), (true, "down") | (true, "infinite")) {
                let end = vars["templateHeight"];
                return Ok(((((end - y).max(0.0) / h).ceil() as usize).max(1), h));
            }
            if matches!((vertical, s), (false, "right") | (false, "infinite")) {
                let end = vars["templateWidth"];
                return Ok(((((end - x).max(0.0) / w).ceil() as usize).max(1), w));
            }
        }
        let n = number(v, vars)?;
        ensure!(
            n.fract() == 0.0 && (1.0..=MAX_INSTANCES as f64).contains(&n),
            "invalid repeat count"
        );
        Ok((n as usize, step))
    };
    let (rows, row_step) = count(o.get("rows"), true)?;
    let (cols, col_step) = count(o.get("columns"), false)?;
    Ok((rows, cols, row_step, col_step))
}

fn render_path(
    item: &Value,
    vars: &HashMap<String, f64>,
    ox: f64,
    oy: f64,
    state: &mut State,
) -> Result<()> {
    allowed_keys(
        item,
        &[
            "type",
            "id",
            "data",
            "fillColor",
            "strokeColor",
            "strokeWidth",
            "antialiasing",
        ],
        "path",
    )?;
    let data = item
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("path data must be an array"))?;
    ensure!(data.len() <= MAX_PATH_PARTS, "template path exceeds limit");
    let mut d = String::new();
    let mut command = "";
    let mut coordinate = 0usize;
    for part in data {
        if let Some(s) = part
            .as_str()
            .filter(|s| matches!(*s, "M" | "L" | "C" | "Z"))
        {
            validate_path_arity(command, coordinate)?;
            command = s;
            coordinate = 0;
            d.push_str(s);
            d.push(' ');
            continue;
        }
        ensure!(
            !command.is_empty() && command != "Z",
            "path coordinate without command"
        );
        let mut v = number(part, vars)?;
        if coordinate.is_multiple_of(2) {
            v += ox
        } else {
            v += oy
        }
        d.push_str(&format!("{v:.4} "));
        coordinate += 1;
        state.path_parts += 1;
        ensure!(
            state.path_parts <= MAX_PATH_PARTS,
            "template paths exceed limit"
        );
    }
    validate_path_arity(command, coordinate)?;
    let stroke = color(item.get("strokeColor"), "#b7b7b7")?;
    let fill = color(item.get("fillColor"), "none")?;
    let sw = item
        .get("strokeWidth")
        .map(|v| number(v, vars))
        .transpose()?
        .unwrap_or(1.0);
    ensure!(sw > 0.0 && sw <= 1000.0, "invalid template stroke width");
    state.svg.push_str(&format!(
        "<path d=\"{d}\" stroke=\"{stroke}\" fill=\"{fill}\" stroke-width=\"{sw}\"/>"
    ));
    ensure!(
        state.svg.len() <= MAX_SVG,
        "rendered template exceeds 4 MiB"
    );
    Ok(())
}

fn validate_path_arity(command: &str, coordinates: usize) -> Result<()> {
    let valid = match command {
        "" => coordinates == 0,
        "M" | "L" => coordinates >= 2 && coordinates.is_multiple_of(2),
        "C" => coordinates >= 6 && coordinates.is_multiple_of(6),
        "Z" => coordinates == 0,
        _ => false,
    };
    ensure!(valid, "invalid template path command arity");
    Ok(())
}

fn color(value: Option<&Value>, default: &'static str) -> Result<String> {
    let Some(s) = value.and_then(Value::as_str) else {
        return Ok(default.into());
    };
    ensure!(
        s.len() <= 32
            && s.chars().all(
                |c| c.is_ascii_alphanumeric() || matches!(c, '#' | '(' | ')' | ',' | '.' | ' ')
            ),
        "invalid template color"
    );
    Ok(s.to_owned())
}

fn number(value: &Value, vars: &HashMap<String, f64>) -> Result<f64> {
    let n = if let Some(n) = value.as_f64() {
        n
    } else {
        let s = value
            .as_str()
            .ok_or_else(|| anyhow!("template number must be numeric or an expression"))?;
        ensure!(s.len() <= MAX_EXPR, "template expression is too long");
        Parser::new(s, vars).parse()?
    };
    ensure!(
        n.is_finite() && n.abs() <= MAX_COORD,
        "template coordinate is outside limits"
    );
    Ok(n)
}

fn allowed_keys(value: &Value, allowed: &[&str], context: &str) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{context} must be an object"))?;
    ensure!(
        object.keys().all(|key| allowed.contains(&key.as_str())),
        "unsupported {context} property"
    );
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    vars: &'a HashMap<String, f64>,
    steps: usize,
}
impl<'a> Parser<'a> {
    fn new(s: &'a str, vars: &'a HashMap<String, f64>) -> Self {
        Self {
            bytes: s.as_bytes(),
            at: 0,
            vars,
            steps: 0,
        }
    }
    fn parse(mut self) -> Result<f64> {
        let v = self.conditional()?;
        self.ws();
        ensure!(
            self.at == self.bytes.len(),
            "unsupported template expression"
        );
        Ok(v)
    }
    fn conditional(&mut self) -> Result<f64> {
        let condition = self.logical_or()?;
        self.ws();
        if self.take(b'?') {
            let yes = self.conditional()?;
            self.ws();
            ensure!(self.take(b':'), "invalid conditional");
            let no = self.conditional()?;
            Ok(if condition != 0.0 { yes } else { no })
        } else {
            Ok(condition)
        }
    }
    fn logical_or(&mut self) -> Result<f64> {
        let mut v = self.comparison()?;
        loop {
            self.ws();
            if self.take_pair(b'|', b'|') {
                let right = self.comparison()?;
                v = f64::from(v != 0.0 || right != 0.0);
            } else {
                return Ok(v);
            }
        }
    }
    fn comparison(&mut self) -> Result<f64> {
        let left = self.sum()?;
        self.ws();
        if self.take(b'>') {
            Ok(f64::from(left > self.sum()?))
        } else if self.take(b'<') {
            Ok(f64::from(left < self.sum()?))
        } else {
            Ok(left)
        }
    }
    fn sum(&mut self) -> Result<f64> {
        let mut v = self.product()?;
        loop {
            self.ws();
            if self.take(b'+') {
                v += self.product()?
            } else if self.take(b'-') {
                v -= self.product()?
            } else {
                return Ok(v);
            }
        }
    }
    fn product(&mut self) -> Result<f64> {
        let mut v = self.atom()?;
        loop {
            self.ws();
            if self.take(b'*') {
                v *= self.atom()?
            } else if self.take(b'/') {
                let d = self.atom()?;
                ensure!(d != 0.0, "division by zero");
                v /= d
            } else {
                return Ok(v);
            }
        }
    }
    fn atom(&mut self) -> Result<f64> {
        self.steps += 1;
        ensure!(self.steps <= 128, "template expression is too complex");
        self.ws();
        if self.take(b'(') {
            let v = self.conditional()?;
            self.ws();
            ensure!(self.take(b')'), "missing )");
            return Ok(v);
        };
        let start = self.at;
        if self.at < self.bytes.len()
            && (self.bytes[self.at] == b'-' || self.bytes[self.at] == b'+')
        {
            self.at += 1
        };
        while self.at < self.bytes.len()
            && (self.bytes[self.at].is_ascii_digit() || self.bytes[self.at] == b'.')
        {
            self.at += 1
        }
        if self.at > start && self.bytes[start..self.at].iter().any(u8::is_ascii_digit) {
            return std::str::from_utf8(&self.bytes[start..self.at])?
                .parse()
                .context("invalid number");
        };
        self.at = start;
        while self.at < self.bytes.len()
            && (self.bytes[self.at].is_ascii_alphanumeric() || self.bytes[self.at] == b'_')
        {
            self.at += 1
        }
        ensure!(self.at > start, "expected expression value");
        let name = std::str::from_utf8(&self.bytes[start..self.at])?;
        self.vars
            .get(name)
            .copied()
            .ok_or_else(|| anyhow!("unknown template variable {name:?}"))
    }
    fn ws(&mut self) {
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1
        }
    }
    fn take(&mut self, b: u8) -> bool {
        if self.bytes.get(self.at) == Some(&b) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn take_pair(&mut self, a: u8, b: u8) -> bool {
        if self.bytes.get(self.at..self.at + 2) == Some([a, b].as_slice()) {
            self.at += 2;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expressions_are_bounded_and_support_firmware_style_conditionals() {
        let v = HashMap::from([
            ("templateWidth".into(), 1404.0),
            ("mobileMaxWidth".into(), 1000.0),
            ("a".into(), 2.0),
        ]);
        assert_eq!(
            Parser::new("templateWidth > mobileMaxWidth ? 177.8 : 160", &v)
                .parse()
                .unwrap(),
            177.8
        );
        assert_eq!(Parser::new("(a + 3) * 4", &v).parse().unwrap(), 20.0);
        assert_eq!(
            Parser::new("templateWidth < mobileMaxWidth || a < 3 ? 7 : 9", &v)
                .parse()
                .unwrap(),
            7.0
        );
    }
    #[test]
    fn renders_group_repeat_and_path() {
        let json=br#"{"formatVersion":1,"constants":[{"gap":100}],"items":[{"type":"group","boundingBox":{"x":0,"y":0,"width":"templateWidth","height":"gap"},"repeat":{"rows":"down"},"children":[{"type":"path","data":["M",0,0,"L","parentWidth",0]}]}]}"#;
        let svg = render_asset(json, 1404.0, 300.0).unwrap();
        assert_eq!(svg.matches("<path ").count(), 3);
        assert!(svg.contains("1404.0000"));
    }
    #[test]
    fn repeat_up_covers_the_page_without_unbounded_iteration() {
        let json = br#"{"formatVersion":1,"items":[{"type":"group","boundingBox":{"x":0,"y":200,"width":300,"height":100},"repeat":{"rows":"up"},"children":[{"type":"path","data":["M",0,0,"L","parentWidth",0]}]}]}"#;
        let svg = render_asset(json, 300.0, 300.0).unwrap();
        assert_eq!(svg.matches("<path ").count(), 3);
        assert!(svg.contains("M 0.0000 0.0000 L 300.0000 0.0000"));
    }
    #[test]
    fn refuses_silent_text_loss() {
        let json=br#"{"formatVersion":1,"items":[{"type":"text","text":"Monday","fontSize":20,"position":{"x":0,"y":0}}]}"#;
        assert!(render_asset(json, 1404.0, 1872.0)
            .unwrap_err()
            .to_string()
            .contains("text items"));
    }
    #[test]
    fn builtin_papers_cover_the_standard_geometric_names() {
        // Every name the firmware's templates.json lists for a purely
        // geometric paper has a generator; planners and music staves do not.
        for name in [
            "P Dots S",
            "P Dots large",
            "P Lines small",
            "P Lines medium",
            "P Lines large",
            "P Grid small",
            "P Grid medium",
            "P Grid large",
            "P Margin small",
            "P Margin medium",
            "P Margin large",
            "LS Dots top",
            "LS Lines medium",
            "LS Grid medium",
            "LS Margin small",
        ] {
            let svg = builtin(name, 1404.0, 1872.0)
                .unwrap_or_else(|| panic!("{name} needs a built-in generator"));
            assert!(svg.starts_with("<svg"), "{name}");
            assert!(
                svg.contains("<path") || svg.contains("<rect"),
                "{name} drew nothing"
            );
        }
        assert!(builtin("P Week", 1404.0, 1872.0).is_none());
        assert!(builtin("LS Piano sheet large", 1872.0, 1404.0).is_none());
    }
    #[test]
    fn builtin_dot_and_line_pitch_match_the_measured_firmware_geometry() {
        // "P Dots S" is a 42.5 lattice of 1x1 squares starting at x = -21,
        // which on a 1404x1872 page is 34 columns by 45 rows.
        let dots = builtin("P Dots S", 1404.0, 1872.0).expect("dots");
        assert_eq!(dots.matches("<rect ").count(), 34 * 45);
        // "P Lines small" rules every 52 units from y = 0 down the page.
        let lines = builtin("P Lines small", 1404.0, 1872.0).expect("lines");
        assert_eq!(lines.matches("<path ").count(), 36);
        // "P Margin small" starts at the measured y = 154 and adds exactly one
        // vertical rule to that ruling.
        let margin = builtin("P Margin small", 1404.0, 1872.0).expect("margin");
        assert_eq!(margin.matches("<path ").count(), 34 + 1);
        assert!(margin.contains("M 244.0000 0 L 244.0000 1872.0000"));
        // "P Grid medium" rules both directions at the measured 78.3 pitch.
        let grid = builtin("P Grid medium", 1404.0, 1872.0).expect("grid");
        assert_eq!(grid.matches("<path ").count(), 24 + 21);
    }
    #[test]
    fn builtin_output_is_bounded_on_an_extreme_page() {
        let svg = builtin("P Grid small", 200_000.0, 200_000.0).expect("grid");
        assert!(svg.len() <= MAX_SVG);
        assert!(svg.matches("<path ").count() <= MAX_INSTANCES);
    }
    #[test]
    fn search_paths_prefer_the_environment_then_xdg_then_home() {
        // Reading process-wide variables, so keep this in one test.
        let guard = std::sync::Mutex::new(());
        let _held = guard.lock().unwrap();
        let previous = [
            std::env::var_os("REMARKABLE_TEMPLATES_DIR"),
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("HOME"),
        ];
        // SAFETY-free: `set_var` is safe on this toolchain's edition.
        std::env::set_var("REMARKABLE_TEMPLATES_DIR", "/opt/templates");
        std::env::set_var("XDG_DATA_HOME", "/data");
        std::env::set_var("HOME", "/home/person");
        assert_eq!(
            search_paths(),
            [
                PathBuf::from("/opt/templates"),
                PathBuf::from("/data/remarkable-desktop/templates"),
                PathBuf::from("/home/person/.local/share/remarkable-desktop/templates"),
            ]
        );
        // A relative override is refused rather than resolved against the cwd.
        std::env::set_var("REMARKABLE_TEMPLATES_DIR", "templates");
        assert!(!search_paths().contains(&PathBuf::from("templates")));
        for (name, value) in ["REMARKABLE_TEMPLATES_DIR", "XDG_DATA_HOME", "HOME"]
            .iter()
            .zip(previous)
        {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
    #[test]
    fn cpages_template_lookup_is_by_page_id() {
        let content = serde_json::json!({"cPages":{"pages":[{"id":"b","template":{"value":"P Grid"}},{"id":"a","template":{"value":"Blank"}}]}});
        assert_eq!(
            template_for_page(&content, "b").unwrap().as_deref(),
            Some("P Grid")
        );
    }
}
