from __future__ import annotations

import math
import os
import re
import sys

SRC = "assets/logo-dark.svg"
DST = "assets/logo-animated.svg"
DST_LINES = "assets/logo-animated-lines.svg"

GEM_FROM, GEM_DUR, GEM_EASE = 0.55, 0.50, (0.16, 1.0, 0.30, 1.0)
LINE_DELAY, LINE_SPREAD, LINE_DUR = 0.10, 0.85, 0.42
LINE_EASE = (0.34, 1.25, 0.50, 1.0)

W, H = 970, 970

def parse_path(d: str, tx: float, ty: float, s: float):

    toks = re.findall(r"[MmLlZz]|-?\d*\.?\d+(?:[eE][-+]?\d+)?", d)
    subs, cur, x, y, cmd, i = [], [], 0.0, 0.0, None, 0

    def num(k: int) -> float:
        return float(toks[i + k])

    while i < len(toks):
        t = toks[i]
        if t in "MmLlZz":
            if t in "Zz" and cur:

                x, y = cur[0]
            cmd, i = t, i + 1
            continue
        if cmd == "M":
            x, y, i = num(0), num(1), i + 2
            if cur:
                subs.append(cur)
            cur, cmd = [(x, y)], "L"
        elif cmd == "m":
            x, y, i = x + num(0), y + num(1), i + 2
            if cur:
                subs.append(cur)
            cur, cmd = [(x, y)], "l"
        elif cmd == "L":
            x, y, i = num(0), num(1), i + 2
            cur.append((x, y))
        else:
            x, y, i = x + num(0), y + num(1), i + 2
            cur.append((x, y))
    if cur:
        subs.append(cur)

    out = []
    for sp in subs:
        pts = [(tx + px * s, ty + py * s) for px, py in sp]
        if len(pts) > 2 and pts[0] == pts[-1]:
            pts = pts[:-1]
        if len(pts) > 2:
            out.append(pts)
    return out

def signed_area(p):
    a = 0.0
    for k in range(len(p)):
        x1, y1 = p[k]
        x2, y2 = p[(k + 1) % len(p)]
        a += x1 * y2 - x2 * y1
    return a / 2

def centroid(p):
    n = len(p)
    return (sum(q[0] for q in p) / n, sum(q[1] for q in p) / n)

def bbox(p):
    xs = [q[0] for q in p]
    ys = [q[1] for q in p]
    return min(xs), min(ys), max(xs), max(ys)

def overlaps(a, b):
    return a[0] <= b[2] and b[0] <= a[2] and a[1] <= b[3] and b[1] <= a[3]

def dist(a, b):
    return math.hypot(a[0] - b[0], a[1] - b[1])

def origin(p, center):

    return min(p, key=lambda q: dist(q, center))

def path_d(p, ox, oy):
    body = " ".join(
        "%s%.2f %.2f" % ("M " if k == 0 else "L ", x - ox, y - oy)
        for k, (x, y) in enumerate(p)
    )
    return body + " Z"

def cubic_bezier_ease(x1, y1, x2, y2):

    cx = 3 * x1
    bx = 3 * (x2 - x1) - cx
    ax = 1 - cx - bx
    cy = 3 * y1
    by = 3 * (y2 - y1) - cy
    ay = 1 - cy - by

    def sample(t, a, b, c):
        return ((a * t + b) * t + c) * t

    def slope(t, a, b, c):
        return (3 * a * t + 2 * b) * t + c

    def solve(x):
        lo, hi = 0.0, 1.0
        t = x
        for _ in range(24):
            f = sample(t, ax, bx, cx) - x
            if abs(f) < 1e-7:
                return t
            d = slope(t, ax, bx, cx)
            if abs(d) < 1e-7:
                break
            t -= f / d
        while abs(sample(t, ax, bx, cx) - x) > 1e-6 and hi - lo > 1e-9:
            if sample(t, ax, bx, cx) < x:
                lo = t
            else:
                hi = t
            t = (lo + hi) / 2
        return t

    def ease(x):
        if x <= 0:
            return 0.0
        if x >= 1:
            return 1.0
        return sample(solve(x), ay, by, cy)

    return ease

def clamp(x, lo, hi):
    return lo if x < lo else hi if x > hi else x

def load():
    svg = open(SRC).read()
    d = re.search(r'\bd="([^"]+)"', svg).group(1)
    m = re.search(r'transform="translate\(([-.\d.]+) ([.\d-]+)\) scale\(([\d.]+)\)"', svg)
    tx, ty, s = (float(g) for g in m.groups())

    mask_d = d
    mask_tf = m.group(0).split('"')[1]
    subs = parse_path(d, tx, ty, s)
    gem = max(subs, key=lambda p: abs(signed_area(p)))
    center = centroid(gem)
    holes = [p for p in subs if p is not gem]
    gb = bbox(gem)

    holes = [p for p in holes if overlaps(bbox(p), gb)]
    holes.sort(key=lambda p: dist(origin(p, center), center))

    line_art_d = " ".join(path_d(p, *center) for p in subs)
    return gem, holes, center, line_art_d, mask_d, mask_tf

def scale_at(t, delay, dur, ease):
    return ease(clamp((t - delay) / dur, 0.0, 1.0))

def gem_scale(t):
    return GEM_FROM + (1.0 - GEM_FROM) * scale_at(t, 0.0, GEM_DUR, cubic_bezier_ease(*GEM_EASE))

def delay_of(rank, n):
    return LINE_DELAY + (LINE_SPREAD * rank / max(1, n - 1))

def line_scale(t, delay):
    return scale_at(t, delay, LINE_DUR, cubic_bezier_ease(*LINE_EASE))

def svg(gem, holes, center, line_art_d, mask_d, mask_tf, lines_only=False, t=None):

    def emit_holes(pad):
        out = []
        for rank, p in enumerate(holes):
            o = origin(p, center)
            delay = delay_of(rank, len(holes))
            if t is None:
                out.append(
                    pad + '<g transform="translate(%.2f %.2f)">'
                    '<path class="line" fill="#000000" style="animation-delay:%.3fs" d="%s" /></g>'
                    % (o[0], o[1], delay, path_d(p, *o))
                )
            else:
                out.append(
                    pad + '<g transform="translate(%.2f %.2f) scale(%.4f)">'
                    '<path fill="#000000" d="%s" /></g>'
                    % (o[0], o[1], line_scale(t, delay), path_d(p, *o))
                )
        return out

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="%d" height="%d" viewBox="0 0 %d %d">'
        % (W, H, W, H),
        "  <title>%s</title>"
        % ("gray mark lines, growing out from the centre" if lines_only
           else "gray mark, growing out from the centre"),
    ]
    if not lines_only:
        parts.append('  <rect width="%d" height="%d" rx="160" fill="#000000" />' % (W, H))

    parts += [
        "  <defs>",
        '    <mask id="lines" maskUnits="userSpaceOnUse" x="0" y="0" width="%d" height="%d">' % (W, H),
        '      <rect width="%d" height="%d" fill="#ffffff" />' % (W, H),
    ]
    if lines_only:

        parts.append("      <g>")
        parts += emit_holes("        ")
        parts.append("      </g>")
        parts.append('      <path fill="#ffffff" transform="%s" d="%s" />' % (mask_tf, mask_d))
        parts += ["    </mask>", "  </defs>"]

        parts.append(
            '  <path fill="#ffffff" mask="url(#lines)" d="%s" />' % path_d(gem, 0, 0)
        )
    else:
        parts.append('      <path fill="#ffffff" transform="%s" d="%s" />' % (mask_tf, mask_d))
        parts += ["    </mask>", "  </defs>"]
        if t is None:
            parts.append(
                '  <g transform="translate(%.2f %.2f)">'
                '<path class="gem" fill="#ffffff" d="%s" /></g>'
                % (center[0], center[1], path_d(gem, *center))
            )
        else:
            parts.append(
                '  <g transform="translate(%.2f %.2f) scale(%.4f)">'
                '<path fill="#ffffff" d="%s" /></g>'
                % (center[0], center[1], gem_scale(t), path_d(gem, *center))
            )
        parts.append('  <g mask="url(#lines)">')
        parts += emit_holes("    ")
        parts.append("  </g>")

    if t is None:
        gem_rule = "" if lines_only else (
            "    .gem { transform-origin: 0 0; animation: gem-grow %.2fs cubic-bezier(%s) both; }\n"
            % (GEM_DUR, ", ".join(str(v) for v in GEM_EASE))
        )
        gem_kf = "" if lines_only else (
            "    @keyframes gem-grow { from { transform: scale(%.2f); } }\n" % GEM_FROM
        )
        gem_rule += (
            "    .line { transform-origin: 0 0; animation: line-grow %.2fs cubic-bezier(%s) both; }\n"
            % (LINE_DUR, ", ".join(str(v) for v in LINE_EASE))
        )
        gem_kf += "    @keyframes line-grow { from { transform: scale(0); } }\n"
        parts.append(
            "  <style>\n" + gem_rule + gem_kf
            + "    @media (prefers-reduced-motion: reduce) {\n"
            "      .gem, .line { animation: none; }\n"
            "    }\n"
            "  </style>"
        )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"

def main():
    argv = sys.argv[1:]
    lines_only = "--lines" in argv
    argv = [a for a in argv if a != "--lines"]
    gem, holes, center, line_art_d, mask_d, mask_tf = load()
    if len(argv) == 3 and argv[0] == "--frames":
        n, out = int(argv[1]), argv[2]
        os.makedirs(out, exist_ok=True)
        total = LINE_DELAY + LINE_SPREAD + LINE_DUR
        for k in range(n):
            t = total * k / max(1, n - 1)
            with open(os.path.join(out, "f%03d.svg" % k), "w") as fh:
                fh.write(svg(gem, holes, center, line_art_d, mask_d, mask_tf, lines_only, t=t))
        print("wrote %d frames (%.2fs) to %s" % (n, total, out))
        return
    dst = DST_LINES if lines_only else DST
    with open(dst, "w") as fh:
        fh.write(svg(gem, holes, center, line_art_d, mask_d, mask_tf, lines_only))
    print("wrote %s (%d facet lines%s)" % (dst, len(holes), ", transparent" if lines_only else ""))

if __name__ == "__main__":
    main()
