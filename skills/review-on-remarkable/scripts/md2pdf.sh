#!/usr/bin/env bash
# Build a review PDF for the reMarkable Paper Pro from Markdown.
#
#   md2pdf.sh <input.md> <output.pdf> [title]
#
# Markdown -> standalone HTML with pandoc, HTML -> PDF with WeasyPrint, styled
# by review.css next to this script: one long 180 mm wide page to scroll on
# the tablet (split only past the PDF size limit), a wide right margin for
# handwriting and blank space at the end for closing notes.
# An existing output file is never overwritten.
set -euo pipefail

usage() {
  echo "Usage: $(basename "$0") <input.md> <output.pdf> [title]" >&2
  exit 2
}
fail() {
  echo "md2pdf: $*" >&2
  exit 1
}

[[ $# -ge 2 && $# -le 3 ]] || usage
input=$1
output=$2
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
css="$here/review.css"

[[ -f $input ]] || fail "input file not found: $input"
[[ -f $css ]] || fail "stylesheet not found: $css"
[[ ${output,,} == *.pdf ]] || fail "output must end in .pdf: $output"
[[ ! -e $output ]] || fail "output already exists (not overwriting): $output"

command -v pandoc >/dev/null 2>&1 ||
  fail "pandoc is not installed. Install it with: sudo apt install pandoc"

weasyprint=${WEASYPRINT:-}
if [[ -z $weasyprint ]]; then
  weasyprint=$(command -v weasyprint 2>/dev/null || true)
fi
if [[ -z $weasyprint ]]; then
  for candidate in "$HOME"/.local/bin/weasyprint "$HOME"/.venv/*/bin/weasyprint; do
    if [[ -x $candidate ]]; then
      weasyprint=$candidate
      break
    fi
  done
fi
[[ -n $weasyprint && -x $weasyprint ]] ||
  fail "weasyprint is not installed. Install it with: sudo apt install weasyprint (or pipx install weasyprint), or set WEASYPRINT=/path/to/weasyprint"

work=$(mktemp -d "${TMPDIR:-/tmp}/md2pdf.XXXXXX")
trap 'rm -rf "$work"' EXIT

# pandoc reads UTF-8 only. Convert other encodings (e.g. Latin-1 French text).
source_md="$input"
charset=$(file -b --mime-encoding "$input" 2>/dev/null || echo utf-8)
case $charset in
  utf-8 | us-ascii | binary) ;;
  *)
    iconv -f "$charset" -t UTF-8 "$input" >"$work/input.md" ||
      fail "cannot convert $input from $charset to UTF-8"
    source_md="$work/input.md"
    ;;
esac

# Title: the argument, else the first level-1 heading, else the file name.
title=${3:-}
if [[ -z $title ]]; then
  title=$(grep -m1 -E '^# +' "$source_md" | sed -E 's/^# +//; s/[[:space:]]+#*[[:space:]]*$//' || true)
fi
if [[ -z $title ]]; then
  title=$(basename "${input%.*}")
fi

# The title as a CSS string for the page header: escape backslashes and
# quotes, drop control characters.
css_title=$(printf '%s' "$title" | tr -d '\000-\037' | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
printf '@page { @top-left { content: "%s"; } }\n' \
  "$css_title" >"$work/title.css"

pandoc "$source_md" \
  --from=markdown+smart+task_lists+pipe_tables+grid_tables+footnotes \
  --to=html5 \
  --standalone \
  --highlight-style=tango \
  --metadata document-css=false \
  --metadata pagetitle="$title" \
  --output="$work/review.html" ||
  fail "pandoc could not convert $input"

# WeasyPrint draws form checkboxes on a line of their own: turn task-list
# items into plain boxes that review.css styles.
sed -i -E \
  -e 's#<li><label><input type="checkbox" checked="" ?/>#<li class="task"><label><span class="box done"></span>#g' \
  -e 's#<li><label><input type="checkbox" ?/>#<li class="task"><label><span class="box"></span>#g' \
  "$work/review.html"

# WeasyPrint's own Python runs longpage.py, which measures the content and
# renders it on a page exactly that tall.
python=$(head -1 "$weasyprint" | sed -n 's/^#![[:space:]]*\([^[:space:]]*python[^[:space:]]*\)$/\1/p')
[[ -n $python && -x $python ]] || python=python3
"$python" -c 'import weasyprint' 2>/dev/null ||
  fail "cannot import weasyprint from $python; set WEASYPRINT to a pip or pipx install"

base_url=$(cd "$(dirname "$input")" && pwd)
tmp_pdf="$work/review.pdf"
# 180 mm wide, 60 mm of blank space at the end, at most 5000 mm per page.
layout=$("$python" "$here/longpage.py" "$work/review.html" "$tmp_pdf" "$base_url/" \
  180 60 5000 "$css" "$work/title.css" 2>"$work/weasyprint.log") ||
  fail "weasyprint could not render the PDF: $(tail -3 "$work/weasyprint.log" | tr '\n' ' ')"

[[ -s $tmp_pdf ]] || fail "weasyprint produced no output"
mkdir -p "$(dirname "$output")"
# Refuse to replace a file created meanwhile.
if ! ln "$tmp_pdf" "$output" 2>/dev/null; then
  [[ ! -e $output ]] || fail "output already exists (not overwriting): $output"
  cp -n "$tmp_pdf" "$output"
fi
read -r pages height <<<"$layout"
if [[ $pages == 1 ]]; then
  echo "Wrote $output (one page, 180 x $height mm)"
else
  echo "Wrote $output ($pages pages of 180 x $height mm)"
fi
