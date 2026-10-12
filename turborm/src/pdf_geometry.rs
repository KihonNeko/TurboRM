//! Placement of a source PDF page inside a reMarkable scene.
//!
//! A scene coordinate unit is a device pixel at **227 dpi**, and the firmware
//! composites the background image at `(-imageWidth / 2, 0)` in centered scene
//! coordinates. A PDF page is therefore laid down at its **true physical
//! size**, `points * 227 / 72`, horizontally centred on the scene origin with
//! its top edge at scene `y = 0`.
//!
//! This module performs no I/O and holds no credentials.
use anyhow::{bail, ensure, Result};
use turborm_core::notebook::CanvasBounds;

/// Scene units (device pixels) per PDF point.
///
/// The value is the reciprocal of the `72 / 227` double the firmware keeps in
/// its unit-conversion table next to `227 / 72` and `360 / 25.4`.
pub const SCENE_UNITS_PER_POINT: f32 = 227.0 / 72.0;

/// Nominal reMarkable paper, in scene units.
pub const PAPER_WIDTH: f32 = 1404.0;
/// Nominal reMarkable paper, in scene units.
pub const PAPER_HEIGHT: f32 = 1872.0;

/// Reject page boxes that would produce a canvas the renderer cannot bound.
const MAX_POINTS: f32 = 200_000.0;

/// The geometry of one source PDF page, in PDF points, after `/Rotate` and the
/// `/CropBox`-within-`/MediaBox` intersection have been applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PdfPage {
    /// Visible width in points, after rotation.
    pub width: f32,
    /// Visible height in points, after rotation.
    pub height: f32,
}

impl PdfPage {
    /// Build a page box from raw `/MediaBox`, optional `/CropBox`, `/Rotate`
    /// and `/UserUnit`, all as stored in the PDF.
    ///
    /// `media` and `crop` are `[x0, y0, x1, y1]` and need not be normalised;
    /// PDF permits either corner order. A `/CropBox` is clipped to the
    /// `/MediaBox`, as required by the specification, and a degenerate or
    /// absent intersection falls back to the `/MediaBox`.
    pub fn new(
        media: [f32; 4],
        crop: Option<[f32; 4]>,
        rotate: i32,
        user_unit: f32,
    ) -> Result<Self> {
        ensure!(
            user_unit.is_finite() && user_unit > 0.0 && user_unit <= 1000.0,
            "PDF /UserUnit is out of range"
        );
        let media = normalise(media)?;
        // A /CropBox outside the /MediaBox is clipped rather than honoured.
        let visible = match crop.map(normalise).transpose()? {
            Some(crop) => intersect(media, crop).unwrap_or(media),
            None => media,
        };
        let (width, height) = (
            (visible[2] - visible[0]) * user_unit,
            (visible[3] - visible[1]) * user_unit,
        );
        ensure!(
            width > 0.0 && height > 0.0 && width <= MAX_POINTS && height <= MAX_POINTS,
            "PDF page box is empty or exceeds the supported size"
        );
        // /Rotate is a clockwise display rotation in multiples of 90 degrees.
        // The quarter turns swap the visible extent; nothing else here depends
        // on the rotation, because the rasterizer already applies it.
        let quarter = rotate.rem_euclid(360);
        ensure!(quarter % 90 == 0, "PDF /Rotate is not a multiple of 90");
        Ok(if quarter == 90 || quarter == 270 {
            Self {
                width: height,
                height: width,
            }
        } else {
            Self { width, height }
        })
    }

    /// The page as the firmware lays it out, in centered scene coordinates:
    /// physical size, horizontally centred, top edge on `y = 0`.
    pub fn scene_rect(&self) -> CanvasBounds {
        let width = self.width * SCENE_UNITS_PER_POINT;
        let height = self.height * SCENE_UNITS_PER_POINT;
        CanvasBounds {
            x: -width / 2.0,
            y: 0.0,
            width,
            height,
        }
    }
}

/// How a scene's nominal paper and a PDF background sit on the render canvas.
///
/// The canvas keeps this renderer's existing convention: `x = 0` is the left
/// edge of the nominal paper, so canvas `x` is scene `x + paper_width / 2`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Nominal paper size in scene units, used for the ink coordinate origin.
    pub paper: (f32, f32),
    /// Where to draw the rasterized PDF page on the canvas.
    pub background: CanvasBounds,
}

/// Place a PDF page for a page whose scene declares `paper`.
///
/// `paper` is the scene's own nominal paper size when the scene records one,
/// otherwise the caller's document default. It only fixes the canvas origin;
/// it never scales the background.
pub fn place(page: PdfPage, paper: (f32, f32)) -> Result<Placement> {
    let (paper_width, paper_height) = paper;
    ensure!(
        paper_width.is_finite()
            && paper_height.is_finite()
            && paper_width > 0.0
            && paper_height > 0.0,
        "invalid nominal paper size"
    );
    let rect = page.scene_rect();
    Ok(Placement {
        paper,
        background: CanvasBounds {
            // Scene x is centred on the paper; the canvas starts at its left edge.
            x: rect.x + paper_width / 2.0,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        },
    })
}

/// Read one page's box from an already-parsed PDF.
///
/// `/MediaBox`, `/CropBox`, `/Rotate` and `/UserUnit` are inheritable page-tree
/// attributes, so each is resolved through the page's `/Parent` chain.
pub fn page_box(pdf: &lopdf::Document, page_id: lopdf::ObjectId) -> Result<PdfPage> {
    let media = inherited_rect(pdf, page_id, b"MediaBox")?
        // Letter is the specification's fallback for a missing /MediaBox.
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let crop = inherited_rect(pdf, page_id, b"CropBox")?;
    let rotate = inherited_number(pdf, page_id, b"Rotate")?.unwrap_or(0.0);
    ensure!(
        rotate.is_finite() && rotate.abs() <= 100_000.0,
        "PDF /Rotate is out of range"
    );
    let user_unit = inherited_number(pdf, page_id, b"UserUnit")?.unwrap_or(1.0);
    PdfPage::new(media, crop, rotate as i32, user_unit)
}

/// Every page's box from the output of Poppler's `pdfinfo -f 1 -l N -box`.
///
/// Poppler has already resolved inheritance through the page tree and
/// clipped the `/CropBox` to the `/MediaBox`. It does not report
/// `/UserUnit`, so the caller only uses this for PDFs that declare none.
/// Pages must be numbered 1, 2, 3... with a box and a rotation each.
pub fn parse_pdfinfo_boxes(output: &str) -> Result<Vec<PdfPage>> {
    #[derive(Default)]
    struct Raw {
        media: Option<[f32; 4]>,
        crop: Option<[f32; 4]>,
        rotate: Option<i32>,
    }
    let mut pages: Vec<Raw> = Vec::new();
    for line in output.lines() {
        // "Page    1 MediaBox:     0.00     0.00   612.00   792.00"
        let Some(rest) = line.strip_prefix("Page ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let (Some(number), Some(field)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(number) = number.parse::<usize>() else {
            continue;
        };
        ensure!(
            number >= 1 && number <= pages.len() + 1,
            "pdfinfo listed the PDF's pages out of order"
        );
        if number > pages.len() {
            pages.push(Raw::default());
        }
        let page = &mut pages[number - 1];
        let values = words.collect::<Vec<_>>();
        let rect = || -> Result<[f32; 4]> {
            ensure!(values.len() == 4, "pdfinfo printed a malformed page box");
            let mut rect = [0.0; 4];
            for (slot, value) in rect.iter_mut().zip(&values) {
                *slot = value.parse()?;
            }
            Ok(rect)
        };
        match field {
            "MediaBox:" => page.media = Some(rect()?),
            "CropBox:" => page.crop = Some(rect()?),
            "rot:" => {
                ensure!(values.len() == 1, "pdfinfo printed a malformed rotation");
                page.rotate = Some(values[0].parse()?);
            }
            _ => {}
        }
    }
    ensure!(!pages.is_empty(), "pdfinfo listed no pages");
    pages
        .into_iter()
        .map(|page| match (page.media, page.rotate) {
            (Some(media), Some(rotate)) => PdfPage::new(media, page.crop, rotate, 1.0),
            _ => bail!("pdfinfo left out a page's box or rotation"),
        })
        .collect()
}

fn normalise(rect: [f32; 4]) -> Result<[f32; 4]> {
    ensure!(
        rect.iter().all(|value| value.is_finite()),
        "PDF page box is not a finite rectangle"
    );
    Ok([
        rect[0].min(rect[2]),
        rect[1].min(rect[3]),
        rect[0].max(rect[2]),
        rect[1].max(rect[3]),
    ])
}

fn intersect(a: [f32; 4], b: [f32; 4]) -> Option<[f32; 4]> {
    let out = [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ];
    (out[2] > out[0] && out[3] > out[1]).then_some(out)
}

/// Walk the `/Parent` chain for an inheritable attribute, with a bounded depth.
fn inherited<'a>(
    pdf: &'a lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Result<Option<&'a lopdf::Object>> {
    let mut id = page_id;
    for _ in 0..64 {
        let dictionary = pdf
            .get_object(id)
            .ok()
            .and_then(|object| object.as_dict().ok());
        let Some(dictionary) = dictionary else {
            return Ok(None);
        };
        if let Ok(value) = dictionary.get(key) {
            // Resolve one level of indirection, which lopdf does not do here.
            return Ok(Some(match value {
                lopdf::Object::Reference(reference) => pdf
                    .get_object(*reference)
                    .map_err(|_| anyhow::anyhow!("PDF page attribute is unresolvable"))?,
                other => other,
            }));
        }
        match dictionary.get(b"Parent") {
            Ok(lopdf::Object::Reference(parent)) => id = *parent,
            _ => return Ok(None),
        }
    }
    bail!("PDF page tree is too deeply nested")
}

fn inherited_rect(
    pdf: &lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Result<Option<[f32; 4]>> {
    let Some(object) = inherited(pdf, page_id, key)? else {
        return Ok(None);
    };
    let Ok(array) = object.as_array() else {
        return Ok(None);
    };
    if array.len() != 4 {
        return Ok(None);
    }
    let mut out = [0.0f32; 4];
    for (slot, value) in out.iter_mut().zip(array) {
        let Some(number) = number(pdf, value)? else {
            return Ok(None);
        };
        *slot = number;
    }
    Ok(Some(out))
}

fn inherited_number(
    pdf: &lopdf::Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Result<Option<f32>> {
    match inherited(pdf, page_id, key)? {
        Some(object) => number(pdf, object),
        None => Ok(None),
    }
}

fn number(pdf: &lopdf::Document, object: &lopdf::Object) -> Result<Option<f32>> {
    Ok(match object {
        lopdf::Object::Integer(value) => Some(*value as f32),
        lopdf::Object::Real(value) => Some(*value),
        lopdf::Object::Reference(reference) => match pdf.get_object(*reference) {
            Ok(lopdf::Object::Integer(value)) => Some(*value as f32),
            Ok(lopdf::Object::Real(value)) => Some(*value),
            _ => None,
        },
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values in this module's tests come from pages measured on a tablet.
    fn approx(left: f32, right: f32) {
        assert!(
            (left - right).abs() < 0.01,
            "expected {right}, found {left}"
        );
    }

    fn approx_rect(left: CanvasBounds, right: CanvasBounds) {
        approx(left.x, right.x);
        approx(left.y, right.y);
        approx(left.width, right.width);
        approx(left.height, right.height);
    }

    #[test]
    fn the_reference_document_reproduces_its_measured_scene_rectangle() {
        // a reference PDF: a 1053x1404 pt page on 1404x1872
        // paper. Registration against the tablet-aligned ink recovered a
        // 3320x4427 scene rectangle centred on x = 0 with its top at y = 0.
        let page = PdfPage::new([0.0, 0.0, 1053.0, 1404.0], None, 0, 1.0).unwrap();
        let rect = page.scene_rect();
        approx_rect(
            rect,
            CanvasBounds {
                x: -1659.9375,
                y: 0.0,
                width: 3319.875,
                height: 4426.5,
            },
        );
        // On the canvas the same rectangle starts 702 units further right.
        let placed = place(page, (PAPER_WIDTH, PAPER_HEIGHT)).unwrap();
        approx_rect(
            placed.background,
            CanvasBounds {
                x: -957.9375,
                y: 0.0,
                width: 3319.875,
                height: 4426.5,
            },
        );
    }

    #[test]
    fn letter_and_a4_keep_their_physical_size_rather_than_the_nominal_paper() {
        let letter = PdfPage::new([0.0, 0.0, 612.0, 792.0], None, 0, 1.0)
            .unwrap()
            .scene_rect();
        approx_rect(
            letter,
            CanvasBounds {
                x: -964.75,
                y: 0.0,
                width: 1929.5,
                height: 2497.0,
            },
        );
        let a4 = PdfPage::new([0.0, 0.0, 595.276, 841.89], None, 0, 1.0)
            .unwrap()
            .scene_rect();
        approx(a4.width, 1876.773);
        approx(a4.height, 2654.292);
        // Both stay centred on the scene origin.
        approx(a4.x + a4.width / 2.0, 0.0);
        approx(a4.y, 0.0);
    }

    #[test]
    fn a_landscape_page_stays_wider_than_it_is_tall() {
        let page = PdfPage::new([0.0, 0.0, 792.0, 612.0], None, 0, 1.0).unwrap();
        assert_eq!((page.width, page.height), (792.0, 612.0));
        let rect = page.scene_rect();
        approx(rect.width, 2497.0);
        approx(rect.height, 1929.5);
        assert!(rect.width > rect.height);
        // Landscape ink still uses the document's own nominal paper.
        let placed = place(page, (PAPER_HEIGHT, PAPER_WIDTH)).unwrap();
        approx(placed.background.x, -1248.5 + 936.0);
    }

    #[test]
    fn quarter_turns_swap_the_visible_extent_and_half_turns_do_not() {
        let portrait = [0.0, 0.0, 612.0, 792.0];
        for rotate in [90, 270, -90, 450] {
            let page = PdfPage::new(portrait, None, rotate, 1.0).unwrap();
            assert_eq!(
                (page.width, page.height),
                (792.0, 612.0),
                "rotate {rotate} must swap the extent"
            );
        }
        for rotate in [0, 180, 360, -180] {
            let page = PdfPage::new(portrait, None, rotate, 1.0).unwrap();
            assert_eq!(
                (page.width, page.height),
                (612.0, 792.0),
                "rotate {rotate} must keep the extent"
            );
        }
        assert!(PdfPage::new(portrait, None, 45, 1.0).is_err());
    }

    #[test]
    fn a_cropbox_replaces_the_mediabox_and_is_clipped_to_it() {
        // A crop inside the media box is used as-is.
        let page = PdfPage::new(
            [0.0, 0.0, 612.0, 792.0],
            Some([72.0, 72.0, 540.0, 720.0]),
            0,
            1.0,
        )
        .unwrap();
        assert_eq!((page.width, page.height), (468.0, 648.0));
        approx(page.scene_rect().width, 468.0 * 227.0 / 72.0);
        // A crop that overhangs the media box is clipped to the intersection.
        let clipped = PdfPage::new(
            [0.0, 0.0, 612.0, 792.0],
            Some([-100.0, -100.0, 400.0, 400.0]),
            0,
            1.0,
        )
        .unwrap();
        assert_eq!((clipped.width, clipped.height), (400.0, 400.0));
        // A disjoint crop is ignored rather than producing an empty page.
        let disjoint = PdfPage::new(
            [0.0, 0.0, 612.0, 792.0],
            Some([1000.0, 1000.0, 1200.0, 1200.0]),
            0,
            1.0,
        )
        .unwrap();
        assert_eq!((disjoint.width, disjoint.height), (612.0, 792.0));
    }

    /// The boxes Poppler reports give the same pages as reading the PDF.
    #[test]
    fn pdfinfo_boxes_are_read_page_by_page() {
        let output = "\
Page    1 size:  612 x 792 pts (letter)
Page    1 rot:   0
Page    1 MediaBox:      0.00     0.00   612.00   792.00
Page    1 CropBox:       0.00     0.00   612.00   792.00
Page    1 BleedBox:      0.00     0.00   612.00   792.00
Page    2 size:  200 x 80 pts
Page    2 rot:   90
Page    2 MediaBox:      0.00     0.00   200.00   100.00
Page    2 CropBox:       0.00    10.00   200.00    90.00
File size:       376 bytes
";
        let pages = parse_pdfinfo_boxes(output).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[0].height), (612.0, 792.0));
        // Rotated a quarter turn: the cropped 200 x 80 page stands 80 x 200.
        assert_eq!((pages[1].width, pages[1].height), (80.0, 200.0));

        // Anything incomplete or out of order is refused, never guessed.
        assert!(parse_pdfinfo_boxes("File size: 1 bytes\n").is_err());
        assert!(parse_pdfinfo_boxes("Page    2 rot:   0\n").is_err());
        assert!(parse_pdfinfo_boxes("Page    1 rot:   0\n").is_err());
        assert!(
            parse_pdfinfo_boxes("Page    1 rot:   45\nPage    1 MediaBox: 0 0 612 792\n").is_err()
        );
        assert!(parse_pdfinfo_boxes("Page    1 rot:   0\nPage    1 MediaBox: 0 0 612\n").is_err());
    }

    #[test]
    fn reversed_corners_and_user_unit_are_honoured() {
        let reversed = PdfPage::new([612.0, 792.0, 0.0, 0.0], None, 0, 1.0).unwrap();
        assert_eq!((reversed.width, reversed.height), (612.0, 792.0));
        let scaled = PdfPage::new([0.0, 0.0, 612.0, 792.0], None, 0, 2.0).unwrap();
        assert_eq!((scaled.width, scaled.height), (1224.0, 1584.0));
        assert!(PdfPage::new([0.0, 0.0, 612.0, 792.0], None, 0, 0.0).is_err());
        assert!(PdfPage::new([0.0, 0.0, 612.0, 792.0], None, 0, f32::NAN).is_err());
    }

    #[test]
    fn empty_enormous_and_non_finite_boxes_are_refused() {
        assert!(PdfPage::new([10.0, 10.0, 10.0, 200.0], None, 0, 1.0).is_err());
        assert!(PdfPage::new([0.0, 0.0, 1e9, 1e9], None, 0, 1.0).is_err());
        assert!(PdfPage::new([0.0, 0.0, f32::INFINITY, 792.0], None, 0, 1.0).is_err());
        let page = PdfPage::new([0.0, 0.0, 612.0, 792.0], None, 0, 1.0).unwrap();
        assert!(place(page, (0.0, 1872.0)).is_err());
        assert!(place(page, (1404.0, f32::NAN)).is_err());
    }

    #[test]
    fn a_custom_scene_paper_only_moves_the_canvas_origin() {
        let page = PdfPage::new([0.0, 0.0, 1053.0, 1404.0], None, 0, 1.0).unwrap();
        let standard = place(page, (PAPER_WIDTH, PAPER_HEIGHT)).unwrap();
        let wide = place(page, (1620.0, 2160.0)).unwrap();
        // The background keeps its size; only the origin shift changes.
        approx(wide.background.width, standard.background.width);
        approx(wide.background.height, standard.background.height);
        approx(wide.background.x - standard.background.x, 108.0);
        approx(wide.background.y, standard.background.y);
    }

    #[test]
    fn page_boxes_are_read_from_a_pdf_including_inherited_attributes() {
        use lopdf::{dictionary, Object};
        let mut pdf = lopdf::Document::with_version("1.5");
        let pages = pdf.new_object_id();
        // The first page inherits /MediaBox and /Rotate from /Pages.
        let inherit = pdf.add_object(dictionary! {"Type" => "Page", "Parent" => pages});
        let own = pdf.add_object(dictionary! {"Type" => "Page", "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 1053.into(), 1404.into()],
        "Rotate" => 0});
        pdf.objects.insert(
            pages,
            dictionary! {"Type" => "Pages",
            "Kids" => vec![Object::Reference(inherit), Object::Reference(own)],
            "Count" => 2,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Rotate" => 90}
            .into(),
        );
        assert_eq!(
            page_box(&pdf, inherit).unwrap(),
            PdfPage {
                width: 792.0,
                height: 612.0
            }
        );
        assert_eq!(
            page_box(&pdf, own).unwrap(),
            PdfPage {
                width: 1053.0,
                height: 1404.0
            }
        );
    }

    #[test]
    fn a_page_without_a_mediabox_falls_back_to_letter() {
        use lopdf::dictionary;
        let mut pdf = lopdf::Document::with_version("1.5");
        let page = pdf.add_object(dictionary! {"Type" => "Page"});
        assert_eq!(
            page_box(&pdf, page).unwrap(),
            PdfPage {
                width: 612.0,
                height: 792.0
            }
        );
    }
}
