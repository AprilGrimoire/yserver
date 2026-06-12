# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — full run #5, yserver/KMS bare-metal (bee, 2026-06-07)

`just xts-yserver-hw all 3600` — all 1078 test cases in 56 minutes,
zero crashes, zero hangs, **zero ABORTs**. Journal + summary:
`xts/results/2026-06-07-22:03:01/`.

**3961 / 5987 test purposes PASS (66.2%)** — up from 3747 (62.6%) in
run #4, driven by:
- Xlib13 (+90, 115 → 205) — silence's input/grab/focus rewrite
  (`7459a81…e7a085c`): core focus model, keyboard grab focus
  events, unified core pointer freeze, pointer confinement +
  grab-mode crossings, AllowSome port.
- Xlib4 (+62, 105 → 167) — Xlib4 BadX / BadMatch / BadValue
  validation series (`b5040a1`, `bdef15e`, `0d453d0`): window
  handlers gate on lookup failure, attribute-value
  BadPixmap/BadColor/BadCursor, InputOnly value-mask + sibling
  semantics + width/height=0 BadValue, CopyFromParent class
  resolution at creation.
- Xlib11 (+24, 50 → 74) — grab semantics from silence's series.
- Xlib9 (+23, 806 → 829) — drawing/copy continued.
- XI (+10) — XTest-through-XI1 cluster.

Total UNRES collapsed from 116 (run #4) to **80**.

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | Δ PASS |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|-------:|
| Xproto    |   122 |   389 |  358 |    7 |     3 |    19 |     2 |     0 |      0 |
| Xlib3     |   109 |   162 |  107 |   18 |     3 |    21 |     6 |     1 |     +3 |
| Xlib4     |    29 |   324 |  167 |  117 |     7 |    17 |    11 |     5 |    +62 |
| Xlib5     |    15 |    84 |   59 |   18 |     0 |     5 |     2 |     0 |      0 |
| Xlib6     |     8 |    50 |    6 |   15 |     0 |    29 |     0 |     0 |      0 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |      0 |
| Xlib8     |    29 |   165 |   88 |   39 |     6 |    22 |    10 |     0 |      0 |
| Xlib9     |    46 |  1472 |  829 |  380 |     0 |    36 |    23 |   201 |    +23 |
| Xlib10    |    23 |    95 |   25 |   29 |     5 |    35 |     1 |     0 |      0 |
| Xlib11    |    33 |   195 |   74 |   49 |     3 |     4 |    22 |    43 |    +24 |
| Xlib12    |    27 |   138 |   94 |   12 |     3 |    15 |     2 |    12 |     +3 |
| Xlib13    |    32 |   269 |  205 |   24 |    24 |    10 |     3 |     3 |    +90 |
| Xlib14    |    45 |    58 |   46 |    7 |     0 |     5 |     0 |     0 |      0 |
| Xlib15    |    45 |   159 |  125 |    1 |     0 |    33 |     0 |     0 |     +3 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |      0 |
| Xlib17    |    55 |   131 |  102 |    8 |     0 |    21 |     0 |     0 |      0 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |      0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |      0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |      0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |      0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |      0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |      0 |
| Xt9       |    33 |   189 |  122 |    2 |     8 |    55 |     2 |     0 |     −4 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |      0 |
| Xt11      |    58 |   285 |  247 |    2 |     0 |    34 |     0 |     0 |      0 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |      0 |
| Xt13      |    39 |   178 |  124 |    5 |     2 |    47 |     0 |     0 |      0 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |      0 |
| XtC       |    29 |   147 |   88 |    0 |     2 |    56 |     1 |     0 |      0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |      0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |      0 |
| XI        |    36 |   316 |  226 |   37 |    13 |    33 |     2 |     5 |    +10 |
| XIproto   |    35 |   107 |   95 |    0 |     0 |    12 |     0 |     0 |      0 |
| **total** | **1078** | **5987** | **3966** | **818** | **80** | **705** | **137** | **268** | **+219** |

ShapeExt, Xlib16, XIproto and Xt3/4/5/10/14 are fully clean (zero
FAIL/UNRES). yserver survived the whole sweep with zero panics in
the server log.

Notes:

- The Xlib9 UNRES collapse (23 → 0) closes the long tail from
  earlier runs — the remaining 380 FAILs are all real assertions,
  not infrastructure noise.
- Xlib13 (+90) is the single biggest section move ever — silence's
  input/grab/focus port lands cleanly against MATE's WM-style
  tests.
- Xt9 −4 (126 → 122, 0 → 2 FAIL, 6 → 8 UNRES) is the only minor
  regression; small enough to chase only if a pattern develops.
- 2 NORESULTs: `Xt5/XtUnmanageChildren`, `Xt5/XtUnmanageChild`
  (unchanged).

Largest FAIL buckets / next targets:
1. **Xlib9 (380)** — remaining drawing/GetImage content semantics
   (now reachable cleanly — UNRES gone).
2. **Xlib4 (117)** — depth-mismatch BadMatch (CWBorderPixmap parser
   needed), colormap visual-type checks, bit-gravity pixel cluster,
   stacking-order pixel checks, BadAccess event-mask conflicts.
3. **Xlib11 (49)** — residual grab semantics.
4. **XI (37)** — XTest-input-through-XI1-freeze gap.
5. **Xlib7 (31)** — colormap section (mostly UNSUPPORTED on
   non-PseudoColor).

Previous full runs:
- #4 — 2026-06-07 17:14:17 (bee, HW): 3747/5987 PASS (62.6%) —
  `xts/results/2026-06-07-17:14:17/`. Last run before the Xlib4
  BadX work and the desktop-input-fixes branch.
- #3 — 2026-06-06 (air, M1): 3419/5987 PASS (57.1%) —
  `xts/results/2026-06-06-20:26:54/`.
- #2 — 2026-06-05 (M2) + 2026-06-06 air XI row: 3370/5987 PASS (56.3%)
  — `xts/results/2026-06-05-13:20:07/` (+ `2026-06-06-00:58:03` for XI).
- #1 — 2026-06-04 (first ever to complete): 2784/5987 PASS (46.5%) —
  `xts/results/2026-06-04-15:48:44/`.

Aborted run between #3 and #4 (`xts/results/2026-06-07-14:01:34/`):
2999/5987 PASS, 1290 UNRES — the GetImage BadMatch cascade caused
by an unguarded `XConfigureWindow` on the root window, fixed by
`77f785b` before run #4.

## rendercheck — bare-metal 2026-06-04, rendercheck 1.6, 900 s/test

| category    |  PASS | TOTAL |
|-------------|------:|------:|
| fill        |    64 |    64 |
| dcoords     |     2 |     2 |
| scoords     |     1 |     1 |
| mcoords     |     1 |     1 |
| tscoords    |     2 |     2 |
| tmcoords    |     2 |     2 |
| blend       |     5 |     5 |
| composite   |     5 |     5 |
| cacomposite |     5 |     5 |
| gradients   |  6081 |  6081 |
| repeat      |   380 |   380 |
| triangles   |   570 |   570 |
| bug7366     |     1 |     1 |
| **total**   | **7119** | **7119** |

**100% pass.**

> Use rendercheck ≥ 1.6. Version 1.5 has a bug in
> `gradients::render_to_gradient_test` that trips even against the
> host X server.
