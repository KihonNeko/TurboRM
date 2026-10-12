#!/usr/bin/env bash
# Print an HTML page (for example a Claude artifact) to a PDF for the
# reMarkable Paper Pro, with headless Chromium so JavaScript charts render.
# The PDF is one long 180 mm wide page, scrolled on the tablet.
#
#   html2pdf.sh <input.html | file:///path/page.html> <output.pdf> [--margin-notes]
#
# A temporary copy of the page gets artifact-print.css (next to this script)
# and a small script injected before </head>: light theme forced, colours
# kept, fixed/sticky elements put back in the flow, and content wider than
# the page scaled down to fit. --margin-notes leaves a 45 mm right margin for
# handwriting. The page's own directory is served read-only on 127.0.0.1 for
# the few seconds of the conversion, so relative files and fetch() of local
# data work as they would on a web server. The original is never modified.
# An existing output file is never overwritten.
set -euo pipefail

usage() {
  echo "Usage: $(basename "$0") <input.html|file:///path.html> <output.pdf> [--margin-notes]" >&2
  exit 2
}
fail() {
  echo "html2pdf: $*" >&2
  exit 1
}

[[ $# -ge 2 && $# -le 3 ]] || usage
input=$1
output=$2
notes=false
if [[ $# -eq 3 ]]; then
  [[ $3 == --margin-notes ]] || usage
  notes=true
fi

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
css="$here/artifact-print.css"
[[ -f $css ]] || fail "stylesheet not found: $css"

if [[ $input == file://* ]]; then
  input=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.unquote(urllib.parse.urlparse(sys.argv[1]).path))' "$input")
fi
[[ -f $input ]] || fail "input file not found: $input"
[[ ${output,,} == *.pdf ]] || fail "output must end in .pdf: $output"
[[ ! -e $output ]] || fail "output already exists (not overwriting): $output"

chrome=${CHROMIUM:-}
if [[ -z $chrome ]]; then
  for candidate in chromium chromium-browser google-chrome google-chrome-stable; do
    if command -v "$candidate" >/dev/null 2>&1; then
      chrome=$(command -v "$candidate")
      break
    fi
  done
fi
[[ -n $chrome ]] || fail "Chromium is not installed. Install it with: sudo apt install chromium (or set CHROMIUM=/path/to/chrome)"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"

work=$(mktemp -d "${TMPDIR:-/tmp}/html2pdf.XXXXXX")
server_pid=
cleanup() {
  [[ -n $server_pid ]] && kill "$server_pid" 2>/dev/null
  rm -rf "$work"
}
trap cleanup EXIT

# Printable width in CSS pixels (96 px per inch): 180 mm minus the margins.
if $notes; then
  right_mm=45
else
  right_mm=12
fi
width_px=$(python3 -c "print(round((180 - 12 - $right_mm) / 25.4 * 96))")

python3 - "$input" "$css" "$work/page.html" "$notes" <<'PY'
import pathlib, re, sys

source, css_path, target, notes = sys.argv[1:5]
path = pathlib.Path(source).resolve()
raw = path.read_bytes()
# Honour a declared charset; artifacts are UTF-8.
match = re.search(rb'<meta[^>]+charset=["\']?([A-Za-z0-9_-]+)', raw[:4096], re.I)
encoding = match.group(1).decode() if match else "utf-8"
try:
    text = raw.decode(encoding)
except (LookupError, UnicodeDecodeError):
    text = raw.decode("utf-8", errors="replace")

css = pathlib.Path(css_path).read_text(encoding="utf-8")
if notes == "true":
    css += "\n@page { margin-right: 45mm; }\n"
# Vertical page margins and the blank space left after the content, in mm.
margins_mm = 12 + 14
tail_mm = 60 if notes == "true" else 15
script = r"""
var REMARKABLE_PAGE = { margins: %MARGINS%, tail: %TAIL%, max: 5000 };
(function () {
  var root = document.documentElement;
  function light() {
    root.setAttribute('data-theme', 'light');
    root.classList.remove('dark');
    root.style.colorScheme = 'light';
  }
  try { localStorage.setItem('theme', 'light'); } catch (e) {}
  light();
  // Charts are printed as drawn: turn off Chart.js animations, whether the
  // library is already loaded or is assigned to window.Chart later, so a
  // resize for the page width never prints a half-drawn chart.
  function still(chart) {
    try {
      chart.defaults.animation = false;
      chart.defaults.animations = false;
      chart.defaults.transitions = { active: { animation: { duration: 0 } }, resize: { animation: { duration: 0 } } };
    } catch (e) {}
    return chart;
  }
  if (window.Chart) {
    still(window.Chart);
  } else {
    try {
      var loaded;
      Object.defineProperty(window, 'Chart', {
        configurable: true,
        get: function () { return loaded; },
        set: function (value) { loaded = still(value); }
      });
    } catch (e) {}
  }
  // A side column (navigation, table of contents, sidebar) next to the
  // content takes a large share of a narrow page: stack it above instead.
  function stackSidebars() {
    var available = root.clientWidth;
    var sides = document.querySelectorAll('nav, aside, [class*="sidebar" i], [id*="sidebar" i]');
    for (var i = 0; i < sides.length; i++) {
      var side = sides[i];
      var parent = side.parentElement;
      if (!parent || parent === root) continue;
      var box = side.getBoundingClientRect();
      var outer = parent.getBoundingClientRect();
      if (box.width < available * 0.15 || box.width > outer.width * 0.6) continue;
      var display = getComputedStyle(parent).display;
      if (display.indexOf('grid') < 0 && display.indexOf('flex') < 0) continue;
      parent.style.setProperty('display', 'block', 'important');
      var children = parent.children;
      for (var j = 0; j < children.length; j++) {
        var child = children[j];
        child.style.setProperty('width', 'auto', 'important');
        child.style.setProperty('max-width', 'none', 'important');
        child.style.setProperty('margin-left', '0', 'important');
        child.style.setProperty('margin-right', '0', 'important');
        child.style.setProperty('margin-bottom', '12px', 'important');
      }
    }
  }
  function unfix() {
    var all = document.querySelectorAll('body *');
    for (var i = 0; i < all.length; i++) {
      var position = getComputedStyle(all[i]).position;
      if (position === 'fixed' || position === 'sticky') {
        all[i].style.setProperty('position', 'static', 'important');
      }
    }
  }
  // A table wider than the page first loses its forced widths and nowrap
  // cells; if it is still too wide, only that table is scaled down.
  function fitTables() {
    var available = root.clientWidth;
    var tables = document.querySelectorAll('table');
    for (var i = 0; i < tables.length; i++) {
      var table = tables[i];
      if (table.getBoundingClientRect().right <= available + 2) continue;
      table.style.setProperty('min-width', '0', 'important');
      table.style.setProperty('width', 'auto', 'important');
      table.style.setProperty('max-width', '100%', 'important');
      var cells = table.querySelectorAll('th, td');
      for (var j = 0; j < cells.length; j++) {
        // Numbers such as "12 800" stay on one line; text may wrap.
        var numeric = /^[\s\d.,:%+\-\u2212\u202f\u00a0€$£¥]*$/.test(cells[j].textContent);
        cells[j].style.setProperty('white-space', numeric ? 'nowrap' : 'normal', 'important');
      }
      var box = table.getBoundingClientRect();
      if (box.right > available + 2 && box.width > 0) {
        var room = Math.max(available - Math.max(box.left, 0), available * 0.5);
        table.style.zoom = Math.max(room / box.width, 0.5).toFixed(4);
      }
    }
  }
  // Whatever is still wider than the page scales the whole page down.
  function fit() {
    var body = document.body;
    if (!body) return;
    body.style.zoom = '';
    var available = root.clientWidth;
    var needed = Math.max(root.scrollWidth, body.scrollWidth);
    if (needed > available + 2) {
      body.style.zoom = (available / needed).toFixed(4);
    }
  }
  // One long page: as tall as the laid-out content, plus blank space at the
  // end for notes. Print layout can differ a little from the screen, so a
  // few percent are added. Past the PDF size limit, Chromium paginates.
  function longPage() {
    var body = document.body;
    var px = Math.max(root.scrollHeight, body ? body.scrollHeight : 0,
      root.getBoundingClientRect().height);
    var mm = px * 25.4 / 96 * 1.03 + REMARKABLE_PAGE.margins + REMARKABLE_PAGE.tail;
    mm = Math.min(Math.ceil(mm), REMARKABLE_PAGE.max);
    var style = document.createElement('style');
    style.id = 'remarkable-page-size';
    style.textContent = '@page { size: 180mm ' + mm + 'mm; }';
    document.head.appendChild(style);
    root.setAttribute('data-remarkable-height-mm', String(mm));
  }
  document.addEventListener('DOMContentLoaded', light);
  window.addEventListener('load', function () {
    light();
    var ready = document.fonts && document.fonts.ready ? document.fonts.ready : Promise.resolve();
    ready.then(function () {
      setTimeout(function () {
        light(); unfix(); stackSidebars(); fitTables(); fit(); longPage();
      }, 600);
    });
  });
})();
"""
script = script.replace("%MARGINS%", str(margins_mm)).replace("%TAIL%", str(tail_mm))
injection = (
    '\n<style id="remarkable-print">\n' + css + "\n</style>\n"
    + '<script id="remarkable-print-script">' + script + "</script>\n"
)
close = re.search(r"</head\s*>", text, re.I)
if close:
    text = text[: close.start()] + injection + text[close.start():]
else:
    body = re.search(r"<body(\s[^>]*)?>", text, re.I)
    at = body.start() if body else 0
    text = text[:at] + injection + text[at:]
# The theme attribute on <html> itself, before any script runs.
text = re.sub(
    r"<html(\s[^>]*)?>",
    lambda m: m.group(0) if "data-theme" in m.group(0) else m.group(0)[:-1] + ' data-theme="light">',
    text, count=1, flags=re.I,
)
pathlib.Path(target).write_text(text, encoding="utf-8")
PY

# A GET/HEAD-only server for the page's directory, with the prepared copy
# at a reserved path beside the original so relative URLs resolve the same.
cat >"$work/serve.py" <<'PY'
import functools, http.server, sys

root, page, port_file = sys.argv[1:4]
RESERVED = "/__remarkable_print__.html"


class Handler(http.server.SimpleHTTPRequestHandler):
    def translate_path(self, path):
        if path.split("?", 1)[0].split("#", 1)[0] == RESERVED:
            return page
        return super().translate_path(path)

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(
    ("127.0.0.1", 0), functools.partial(Handler, directory=root)
)
with open(port_file, "w") as out:
    out.write(str(server.server_address[1]))
server.serve_forever()
PY
python3 "$work/serve.py" "$(dirname "$(realpath "$input")")" "$work/page.html" "$work/port" &
server_pid=$!
for _ in $(seq 50); do
  [[ -s $work/port ]] && break
  sleep 0.1
done
[[ -s $work/port ]] || fail "could not start the local file server"
url="http://127.0.0.1:$(cat "$work/port")/__remarkable_print__.html"

tmp_pdf="$work/out.pdf"
"$chrome" \
  --headless \
  --disable-gpu \
  --no-first-run \
  --no-default-browser-check \
  --disable-extensions \
  --user-data-dir="$work/profile" \
  --window-size="$width_px,1200" \
  --blink-settings=preferredColorScheme=1 \
  --force-color-profile=srgb \
  --no-pdf-header-footer \
  --run-all-compositor-stages-before-draw \
  --virtual-time-budget=8000 \
  --print-to-pdf="$tmp_pdf" \
  "$url" >"$work/chrome.log" 2>&1 ||
  fail "Chromium could not print the page (log: $(tail -3 "$work/chrome.log" | tr '\n' ' '))"

[[ -s $tmp_pdf ]] || fail "Chromium produced no PDF (log: $(tail -3 "$work/chrome.log" | tr '\n' ' '))"
mkdir -p "$(dirname "$output")"
if ! ln "$tmp_pdf" "$output" 2>/dev/null; then
  [[ ! -e $output ]] || fail "output already exists (not overwriting): $output"
  cp -n "$tmp_pdf" "$output"
fi
info=$(pdfinfo "$output" 2>/dev/null || true)
pages=$(awk '/^Pages:/ {print $2}' <<<"$info")
height=$(awk '/^Page size:/ {printf "%d", $5 * 25.4 / 72 + 0.5}' <<<"$info")
if [[ $pages == 1 ]]; then
  echo "Wrote $output (one page, 180 x $height mm)"
else
  echo "Wrote $output (${pages:-?} pages)"
fi
