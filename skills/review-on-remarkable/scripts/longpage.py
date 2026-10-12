"""Render HTML to a PDF made of one long page, for scrolling on the tablet.

    python3 longpage.py <in.html> <out.pdf> <base_url> <width_mm> <tail_mm> <max_mm> <stylesheet>...

The document is laid out once on a very tall page to measure the height of
its content, then rendered again on a page exactly that tall plus <tail_mm>
of blank space at the end for closing notes. A document taller than <max_mm>
(the PDF page-size limit is about 5080 mm) is split into as few pages of
equal height as needed. Prints the number of pages and their height in mm.
"""

import math
import sys

from weasyprint import CSS, HTML

PX_PER_MM = 96 / 25.4
PROBE_MM = 50000


def content_bottom(page_box):
    """Bottom margin edge of the document's root box, in CSS px.

    Only the root box is measured: its height already includes everything in
    normal flow, and the page's margin boxes (the header) are skipped.
    """
    bottom = 0.0
    for child in page_box.children:
        if type(child).__name__ == "MarginBox":
            continue
        bottom = max(bottom, child.position_y + child.margin_height())
    return bottom


def main():
    if len(sys.argv) < 8:
        sys.exit(__doc__)
    source, target, base_url = sys.argv[1:4]
    width_mm, tail_mm, max_mm = (float(value) for value in sys.argv[4:7])
    stylesheets = [CSS(filename=path) for path in sys.argv[7:]]

    html = HTML(filename=source, base_url=base_url, encoding="utf-8")
    probe = CSS(string=f"@page {{ size: {width_mm}mm {PROBE_MM}mm; }}")
    measured = html.render(stylesheets=stylesheets + [probe])
    page = measured.pages[0]._page_box
    bottom_margin = page.margin_bottom or 0
    height_mm = (content_bottom(page) + bottom_margin) / PX_PER_MM + tail_mm
    if len(measured.pages) > 1:
        height_mm = PROBE_MM

    pages = max(1, math.ceil(height_mm / max_mm))
    if pages > 1:
        # Page breaks never fall exactly at the split, so leave some room.
        height_mm = min(max_mm, height_mm / pages * 1.08)
    size = CSS(string=f"@page {{ size: {width_mm}mm {height_mm:.1f}mm; }}")
    document = html.render(stylesheets=stylesheets + [size])
    document.write_pdf(target)
    print(f"{len(document.pages)} {height_mm:.0f}")


if __name__ == "__main__":
    main()
