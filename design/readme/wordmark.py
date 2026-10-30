"""Build a README lockup: a product glyph next to its name, set as outlines, in light and dark.

    python wordmark.py --glyph design/icon/glyph/x-glyph.svg --font app/assets/fonts/Font[wght].ttf \
        --text Scratch --out design/readme --name scratch

Writes <out>/<name>-light.svg and <name>-dark.svg. Outlines need no font at view time, so GitHub
renders them as drawn. Needs fontTools (pip install fonttools); run without -I if it lives in the user site.
"""
import argparse, re
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen

ap = argparse.ArgumentParser()
ap.add_argument("--glyph", help="product glyph SVG (with a viewBox); omit for a name-only wordmark")
ap.add_argument("--font", required=True, help="the product's own display font, static or variable")
ap.add_argument("--text", required=True)
ap.add_argument("--axes", default="wght=700", help="variable-font instance, e.g. wght=700,opsz=96,wdth=88")
ap.add_argument("--tracking", type=float, default=-0.01, help="letter spacing in em")
ap.add_argument("--light", default="#2C2C2B", help="text colour on light backgrounds")
ap.add_argument("--dark", default="#F0EFED", help="text colour on dark backgrounds")
ap.add_argument("--glyph-scale", type=float, default=1.55, help="glyph height in cap heights")
ap.add_argument("--px", type=float, default=96, help="rendered px per em (sets width/height)")
ap.add_argument("--out", required=True)
ap.add_argument("--name", required=True)
a = ap.parse_args()

font = TTFont(a.font)
if "fvar" in font:
    axes = {k: float(v) for k, v in (p.split("=") for p in a.axes.split(",") if p)}
    font = instantiateVariableFont(font, axes)
gs = font.getGlyphSet()
cmap = font.getBestCmap()
upm = font["head"].unitsPerEm
asc = font["hhea"].ascent
cap = getattr(font["OS/2"], "sCapHeight", 0) or asc * 0.7


def kerning():
    """Pair kerning from the kern feature's GPOS PairPos lookups (format 1 and 2)."""
    pairs = {}
    if "GPOS" not in font:
        return pairs
    gpos = font["GPOS"].table
    idx = [i for fr in gpos.FeatureList.FeatureRecord if fr.FeatureTag == "kern" for i in fr.Feature.LookupListIndex]
    glyphs = set(cmap.values())
    for li in idx:
        lk = gpos.LookupList.Lookup[li]
        if lk.LookupType not in (2, 9):
            continue
        for st in lk.SubTable:
            if lk.LookupType == 9:
                if st.ExtensionLookupType != 2:
                    continue
                st = st.ExtSubTable
            cov = st.Coverage.glyphs
            if st.Format == 1:
                for i, first in enumerate(cov):
                    for pvr in st.PairSet[i].PairValueRecord:
                        x = getattr(pvr.Value1, "XAdvance", 0) if pvr.Value1 else 0
                        pairs.setdefault((first, pvr.SecondGlyph), x or 0)
            elif st.Format == 2:
                c1, c2 = st.ClassDef1.classDefs, st.ClassDef2.classDefs
                for first in cov:
                    r = st.Class1Record[c1.get(first, 0)]
                    for second in glyphs:
                        v = r.Class2Record[c2.get(second, 0)].Value1
                        x = getattr(v, "XAdvance", 0) if v else 0
                        if x:
                            pairs.setdefault((first, second), x)
    return pairs


kern = kerning()
names = [cmap[ord(c)] for c in a.text]
tracking = a.tracking * upm
x = 0.0
paths = []
for i, n in enumerate(names):
    pen = SVGPathPen(gs)
    gs[n].draw(TransformPen(pen, (1, 0, 0, -1, x, asc)))
    paths.append(pen.getCommands())
    x += gs[n].width + tracking
    if i + 1 < len(names):
        x += kern.get((n, names[i + 1]), 0)
text_w = x - tracking

pad = upm * 0.06
baseline = asc
glyph_w = glyph_h = gap = 0.0
glyph_y = baseline
glyph_el = ""
if a.glyph:
    glyph_svg = Path(a.glyph).read_text(encoding="utf-8")
    vb = [float(v) for v in re.search(r'viewBox="([^"]+)"', glyph_svg).group(1).replace(",", " ").split()]
    inner = re.search(r"<svg[^>]*>(.*)</svg>", glyph_svg, re.S).group(1)
    glyph_h = cap * a.glyph_scale
    glyph_w = glyph_h * vb[2] / vb[3]
    gap = upm * 0.22
    glyph_y = baseline - cap / 2 - glyph_h / 2  # centred on the cap height
    glyph_el = (f'  <svg x="{pad:.1f}" y="{glyph_y:.1f}" width="{glyph_w:.1f}" height="{glyph_h:.1f}" '
                f'viewBox="{" ".join(f"{v:g}" for v in vb)}">{inner}</svg>\n')

width = glyph_w + gap + text_w + 2 * pad
top = min(glyph_y, baseline - asc * 0.98) - pad
height = max(glyph_y + glyph_h, baseline + upm * 0.06) + pad - top

out = Path(a.out)
out.mkdir(parents=True, exist_ok=True)
for theme, color in (("light", a.light), ("dark", a.dark)):
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 {top:.1f} {width:.1f} {height:.1f}" '
           f'width="{width / upm * a.px:.0f}" height="{height / upm * a.px:.0f}" role="img" aria-label="{a.text}">\n'
           f'  <title>{a.text}</title>\n{glyph_el}'
           f'  <g transform="translate({pad + glyph_w + gap:.1f} 0)" fill="{color}">\n'
           f'    {"".join(f"<path d=\"{p}\"/>" for p in paths)}\n  </g>\n</svg>\n')
    (out / f"{a.name}-{theme}.svg").write_text(svg, encoding="utf-8", newline="\n")
    print(f"{out / f'{a.name}-{theme}.svg'}  {width / upm * a.px:.0f}x{height / upm * a.px:.0f}px")
