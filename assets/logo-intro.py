import math, sys
from shapely.geometry import LineString, Polygon

BAND = 5.893
P = [(28.41797, 0.0), (118.27719, 0.0), (146.69458, 49.221035),
     (101.76586, 127.03985), (44.930503, 127.04018), (0.0, 49.221465)]
hull = Polygon(P)
CX, CY = sum(p[0] for p in P) / 6, sum(p[1] for p in P) / 6
TX, TY, SCALE = 84, 138, 5.46

SIDES = [(i, (i + 1) % 6) for i in range(6)]
DIAGS = [(0, 2), (0, 3), (0, 4), (1, 3), (1, 4), (1, 5), (2, 4), (2, 5), (3, 5)]

SHORT = (2.0840, 1.2885)

def offsets(i, j):
    k = (j - i) % 6
    if k == 3:
        return 0.0, 0.0
    if k in (1, 5):
        return BAND / 2, BAND / 2
    if k == 4:
        return offsets(j, i)[::-1]
    return SHORT if i % 2 == 0 else SHORT[::-1]

def stroke(i, j):
    (x1, y1), (x2, y2) = P[i], P[j]
    L = math.dist(P[i], P[j])
    ux, uy = (x2 - x1) / L, (y2 - y1) / L
    nx, ny = -uy, ux
    if (CX - x1) * nx + (CY - y1) * ny < 0:
        nx, ny = -nx, -ny
    si, sj = offsets(i, j)
    A = (x1 + nx * si, y1 + ny * si)
    B = (x2 + nx * sj, y2 + ny * sj)
    l = math.dist(A, B)
    vx, vy = (B[0] - A[0]) / l, (B[1] - A[1]) / l
    seg = LineString([(A[0] - 20 * vx, A[1] - 20 * vy), (B[0] + 20 * vx, B[1] + 20 * vy)])
    clipped = seg.buffer(BAND / 2, cap_style="flat").intersection(hull)
    ts = [(x - A[0]) * vx + (y - A[1]) * vy for x, y in clipped.exterior.coords]
    a, b = min(ts), max(ts)
    return (A[0] + vx * a, A[1] + vy * a), (A[0] + vx * b, A[1] + vy * b)

T = 6.0
kfs = []

DRAW, RETRACT = (0.15, 1.4), (3.7, 1.1)
GROW = 0.6

def bez(x, x1, y1, x2, y2):

    c = lambda s, p1, p2: 3 * (1 - s) ** 2 * s * p1 + 3 * (1 - s) * s * s * p2 + s ** 3
    lo, hi = 0.0, 1.0
    for _ in range(40):
        m = (lo + hi) / 2
        lo, hi = (m, hi) if c(m, x1, x2) < x else (lo, m)
    return c(lo, y1, y2)

clamp = lambda v: min(1.0, max(0.0, v))
ends = [x + y for e in SIDES + DIAGS for x, y in stroke(*e)]
lo, hi = min(ends), max(ends)
pr = lambda p: (p[0] + p[1] - lo) / (hi - lo)
front = lambda t, ph: bez(clamp((t - ph[0]) / ph[1]), 0.65, 0, 0.35, 1)

def reached(x, ph):
    a, b = ph[0], ph[0] + ph[1]
    for _ in range(40):
        m = (a + b) / 2
        a, b = (m, b) if front(m, ph) < x else (a, m)
    return a

def shown(t, ph, run):

    if run[0] == "one":
        p, q = run[1], run[2]
        return clamp((front(t, ph) - pr(p)) / (pr(q) - pr(p)))
    return bez(clamp((t - reached(pr(run[1]), ph)) / GROW), 0.22, 1, 0.36, 1)

toward = lambda p, q, e: (p[0] + (q[0] - p[0]) * e / math.dist(p, q), p[1] + (q[1] - p[1]) * e / math.dist(p, q))
runs = []
for e in SIDES + DIAGS:
    a, b = sorted(stroke(*e), key=pr)
    if abs(pr(b) - pr(a)) * (hi - lo) / math.sqrt(2) < 0.5 * math.dist(a, b):
        m = ((a[0] + b[0]) / 2, (a[1] + b[1]) / 2)
        runs += [("both", m, a, toward(m, b, 0.2)), ("both", m, b, toward(m, a, 0.2))]
    else:
        runs.append(("one", a, b, a))
N = 180
lines = []
for n, run in enumerate(runs):
    v = [1 - shown(t, DRAW, run) if t < RETRACT[0] else -1.01 * shown(t, RETRACT, run) for t in (T * i / N for i in range(N + 1))]
    keep = [i for i in range(N + 1) if i in (0, N) or abs(v[i - 1] - 2 * v[i] + v[i + 1]) > 1e-3]
    kfs.append(f"    @keyframes g{n} {{ " + " ".join(f"{100 * i / N:.3f}% {{ stroke-dashoffset: {v[i]:.4f}; }}" for i in keep) + " }")
    s0, end = run[3], run[2]
    lines.append(f'        <path class="b" style="animation-name:g{n}" pathLength="1" '
                 f'd="M {s0[0]:.4f} {s0[1]:.4f} L {end[0]:.4f} {end[1]:.4f}" />')
hull_d = "M " + " L ".join(f"{x} {y}" for x, y in P) + " Z"

def svg(extra=""):
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="970" height="970" viewBox="0 0 970 970">
  <title>gray mark intro</title>
  <defs><clipPath id="hull"><path d="{hull_d}" /></clipPath></defs>
    <g transform="translate({TX} {TY}) scale({SCALE})">
      <g clip-path="url(#hull)" fill="none" stroke="#ffffff" stroke-width="{BAND}" stroke-linecap="butt">
{chr(10).join(lines)}
      </g>
    </g>
  <style>
    .b {{ stroke-dasharray: 1 2; }}
    .b {{ animation: {T}s linear 0s infinite both; }}
{chr(10).join(kfs)}
    @media (prefers-reduced-motion: reduce) {{
      .b {{ animation: none; }}
    }}{extra}
  </style>
</svg>
"""

open(__file__.replace(".py", ".svg"), "w").write(svg())

if len(sys.argv) > 1:
    moments = {"1-start": 0.5, "2-wave": 0.9, "3-drawn": 2.0,
               "4-full": 3.2, "5-retract": 4.3, "6-empty": 5.8}
    for name, at in moments.items():
        open(f"{sys.argv[1]}/snap-{name}.svg", "w").write(svg(
            f"\n    .b {{ animation-delay: -{at}s !important;"
            f" animation-play-state: paused !important; }}"))
    open(f"{sys.argv[1]}/snap-static.svg", "w").write(svg(
        "\n    .b { animation: none !important; }"))
