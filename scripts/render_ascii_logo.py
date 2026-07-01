#!/usr/bin/env python3
"""Convert assets/branding/noaa_logo.bbcode into the terminal art embedded
in install.sh's print_logo().

The source is a per-character BBCode color dump (as exported by a forum
pixel-art editor). This maps each color to the nearest xterm 256-color
index and emits "\\033[38;5;<idx>m<char>" runs — literal backslash-033
text, not a raw ESC byte, so the result is plain ASCII that's safe to
paste into a shell script and expand later with `printf '%b'`.

Usage:
    python3 scripts/render_ascii_logo.py assets/branding/noaa_logo.bbcode

Prints the rendered art to stdout — paste it in place of the block
between the `cat <<'NOAA_LOGO_EOF'` and `NOAA_LOGO_EOF` lines in
install.sh's print_logo().
"""
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


def parse_line(line):
    chunks = []
    for m in TAG_RE.finditer(line):
        hexcol, text = m.group(1), m.group(2).replace("&amp;", "&")
        if not text:
            continue
        r, g, b = int(hexcol[0:2], 16), int(hexcol[2:4], 16), int(hexcol[4:6], 16)
        chunks.append((to_256(r, g, b), text))
    return chunks


def render(chunks):
    out, last_idx = [], None
    for idx, text in chunks:
        if idx != last_idx:
            out.append(f"\\033[38;5;{idx}m")
            last_idx = idx
        out.append(text)
    out.append("\\033[0m")
    return "".join(out)


def main():
    with open(sys.argv[1], encoding="utf-8") as f:
        lines = [l.rstrip("\n") for l in f if l.strip()]
    for line in lines:
        print(render(parse_line(line)))


if __name__ == "__main__":
    main()
