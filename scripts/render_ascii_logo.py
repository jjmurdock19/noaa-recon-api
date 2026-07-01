#!/usr/bin/env python3
"""Convert assets/branding/noaa_logo.bbcode into the terminal art embedded
in install.sh's print_logo().

The source is a per-character BBCode color dump (as exported by a forum
pixel-art editor) — one character cell per source pixel. This script:

  1. Parses it into a full (row, col) -> (r, g, b) | None grid (None = a
     space, i.e. no visible pixel there).
  2. Optionally box-downsamples that grid to a smaller target height,
     averaging the color of only the "content" (non-space) cells in each
     block and rendering the result as solid block characters (`█`)
     instead of trying to preserve the original varied glyphs, which is
     the standard way to keep small ANSI art legible (see e.g. neofetch's
     compact distro logos).
  3. Emits "\\033[38;5;<idx>m" runs — literal backslash-033 text, not a
     raw ESC byte, so the result is plain ASCII safe to paste into a
     shell script and expand later with `printf '%b'`.

Usage:
    python3 scripts/render_ascii_logo.py assets/branding/noaa_logo.bbcode           # full size
    python3 scripts/render_ascii_logo.py assets/branding/noaa_logo.bbcode --rows 8  # downsampled

Prints the rendered art to stdout — paste it in place of the block
between the `cat <<'NOAA_LOGO_EOF'` and `NOAA_LOGO_EOF` lines in
install.sh's print_logo().
"""
import argparse
import re
import sys

TAG_RE = re.compile(r"\[color=#([0-9a-fA-F]{6})\](.*?)\[/color\]")

# 6x6x6 color cube + grayscale ramp, standard xterm-256 layout
CUBE_STEPS = [0, 95, 135, 175, 215, 255]


def nearest_cube_index(v):
    return min(range(6), key=lambda i: abs(CUBE_STEPS[i] - v))


def to_256(r, g, b):
    if abs(r - g) < 8 and abs(g - b) < 8 and abs(r - b) < 8:
        gray = round((r + g + b) / 3)
        if gray < 8:
            return 16
        if gray > 238:
            return 231
        return 232 + max(0, min(23, round((gray - 8) / 247 * 23)))
    ri, gi, bi = nearest_cube_index(r), nearest_cube_index(g), nearest_cube_index(b)
    return 16 + 36 * ri + 6 * gi + bi


def parse_grid(path):
    """Returns a rectangular list-of-lists of (r,g,b) or None (background)."""
    grid = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            row = []
            for m in TAG_RE.finditer(line):
                hexcol, text = m.group(1), m.group(2).replace("&amp;", "&")
                r, g, b = int(hexcol[0:2], 16), int(hexcol[2:4], 16), int(hexcol[4:6], 16)
                for ch in text:
                    row.append(None if ch == " " else (r, g, b))
            grid.append(row)
    width = max(len(r) for r in grid)
    for r in grid:
        r += [None] * (width - len(r))
    return grid


def downsample(grid, target_rows):
    src_h = len(grid)
    src_w = len(grid[0])
    scale = src_h / target_rows
    target_cols = max(1, round(src_w / scale))
    out = []
    for tr in range(target_rows):
        r0, r1 = int(tr * scale), max(int(tr * scale) + 1, int((tr + 1) * scale))
        row = []
        for tc in range(target_cols):
            c0, c1 = int(tc * scale), max(int(tc * scale) + 1, int((tc + 1) * scale))
            cells = [grid[r][c] for r in range(r0, min(r1, src_h)) for c in range(c0, min(c1, src_w))]
            content = [c for c in cells if c is not None]
            if len(content) * 2 >= len(cells):  # majority-content -> keep a pixel here
                n = len(content)
                avg = (sum(c[0] for c in content) // n, sum(c[1] for c in content) // n, sum(c[2] for c in content) // n)
                row.append(avg)
            else:
                row.append(None)
        out.append(row)
    return out


def render_grid(grid, block_char="█"):
    lines = []
    for row in grid:
        out, last_idx = [], None
        for cell in row:
            if cell is None:
                out.append(" ")
                last_idx = None
            else:
                idx = to_256(*cell)
                if idx != last_idx:
                    out.append(f"\\033[38;5;{idx}m")
                    last_idx = idx
                out.append(block_char)
        out.append("\\033[0m")
        lines.append("".join(out))
    return lines


def render_original(path):
    lines = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            out, last_idx = [], None
            for m in TAG_RE.finditer(line):
                hexcol, text = m.group(1), m.group(2).replace("&amp;", "&")
                if not text:
                    continue
                r, g, b = int(hexcol[0:2], 16), int(hexcol[2:4], 16), int(hexcol[4:6], 16)
                idx = to_256(r, g, b)
                if idx != last_idx:
                    out.append(f"\\033[38;5;{idx}m")
                    last_idx = idx
                out.append(text)
            out.append("\\033[0m")
            lines.append("".join(out))
    return lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bbcode_path")
    ap.add_argument("--rows", type=int, help="downsample to this many terminal rows (omit for full size)")
    args = ap.parse_args()

    if args.rows:
        grid = downsample(parse_grid(args.bbcode_path), args.rows)
        lines = render_grid(grid)
    else:
        lines = render_original(args.bbcode_path)

    print("\n".join(lines))


if __name__ == "__main__":
    main()
