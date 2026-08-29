#!/usr/bin/env bash
# Builds a single PDF from docs/manual/*.md (README.md as front matter,
# then each numbered chapter in order) -- for anyone who'd rather have one
# offline/printable document than browsing the manual page by page on
# GitHub. Also run by .github/workflows/manual-pdf.yml on every push that
# touches docs/manual/, uploaded there as a build artifact.
#
# Requires pandoc, plus a LaTeX toolchain for PDF output (Debian/Ubuntu:
# texlive-latex-base texlive-fonts-recommended texlive-latex-extra lmodern)
# -- pandoc's own documented recommendation for basic PDF generation, and
# far more reliably available than wkhtmltopdf (removed from current
# Debian/Ubuntu repos). lmodern specifically: pandoc's default template
# loads it unconditionally, but it's its own separate package, not pulled
# in by the texlive-* packages above -- easy to miss on a fresh install
# (a real CI run without it failed at this step; likely "File
# `lmodern.sty' not found", the standard symptom of a missing lmodern
# package, though the exact log wasn't accessible to confirm).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANUAL_DIR="$REPO_ROOT/docs/manual"
OUT_DIR="${1:-$REPO_ROOT/dist}"
OUT_FILE="$OUT_DIR/hpsdr-rs-manual.pdf"

command -v pandoc >/dev/null 2>&1 || {
    echo "error: pandoc not found -- install it (e.g. apt install pandoc)" >&2
    exit 1
}

mkdir -p "$OUT_DIR"

COMBINED="$(mktemp)"
trap 'rm -f "$COMBINED"' EXIT

cat "$MANUAL_DIR/README.md" >>"$COMBINED"
echo >>"$COMBINED"

for chapter in "$MANUAL_DIR"/[0-9][0-9]-*.md; do
    echo >>"$COMBINED"
    # Drop each chapter's own prev/index/next navigation line -- it only
    # makes sense when browsing the pages individually on GitHub, not in
    # one linear PDF. Also rewrite cross-chapter links to a heading
    # anchor (e.g. "02-main-window.md#zoom-and-pan") into a same-document
    # fragment link ("#zoom-and-pan"), so they resolve as real internal
    # PDF links now that every chapter lives in one document instead of
    # pointing at another page's now-nonexistent filename.
    # GitHub's own heading-slug algorithm turns "/" into "--" (e.g. "VFO
    # A / VFO B / Split" -> vfo-a--vfo-b--split, as used by every #anchor
    # link in these docs, GitHub-rendered pages included); pandoc's
    # slugger instead collapses that to a single "-". Rewritten here
    # (PDF-only) rather than in the source docs, which are authored
    # against GitHub's renderer.
    grep -v '\[Index\](README\.md)' "$chapter" \
        | sed -E 's/\]\([0-9]{2}-[a-z0-9-]+\.md#/](#/g; s/vfo-a--vfo-b--split/vfo-a-vfo-b-split/g' \
            >>"$COMBINED"
    echo >>"$COMBINED"
done

# --resource-path so image references (e.g. images/02-main-window-overview.png)
# resolve against docs/manual/ rather than $COMBINED's own location in /tmp.
#
# header-includes forces every image to render exactly where it appears
# in the source instead of LaTeX's own default: pandoc wraps a standalone
# image paragraph in a floating `figure` environment, and LaTeX's float
# algorithm is free to relocate a figure to wherever there's room --
# confirmed by a real report of the PureSignal "correcting" screenshot
# drifting forward past its own section into the START of the next
# chapter (Diversity), ahead of that chapter's own image, because there
# wasn't space left on PureSignal's own page. `\floatplacement{figure}{H}`
# (from the `float` package) pins every figure to right where it's
# written ("here", not just "here if it fits") -- the standard fix for
# this well-known pandoc/LaTeX behavior.
pandoc "$COMBINED" \
    --resource-path="$MANUAL_DIR" \
    --metadata title="hpsdr-rs User Manual" \
    --metadata author="John Melton G0ORX <john.d.melton@googlemail.com>" \
    --toc --toc-depth=2 \
    -V geometry:margin=1in \
    -V colorlinks=true \
    -V header-includes='\usepackage{float}\floatplacement{figure}{H}' \
    -o "$OUT_FILE"

echo "Built $OUT_FILE"
