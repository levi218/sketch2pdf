"""Compare sketch2pdf output against two references and write report/index.html.

1. The preview Sketch embeds in every file (previews/preview.png) vs our first page.
2. Photopea's PSD of the same file (samples/psd), per artboard: each root artboard of the PSD
   is matched to our page of the same name and cropped out of the PSD's merged image.

Setup:   python3 -m venv .venv && .venv/bin/pip install psd-tools pillow numpy
Run:     cargo build --release && .venv/bin/python tools/report.py
Needs pdftoppm (poppler). Font files in fonts/ (if present) are passed to the converter with --fonts.
"""

from __future__ import annotations

import concurrent.futures as cf
import html
import io
import json
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

import numpy as np
from PIL import Image
from psd_tools import PSDImage

ROOT = Path(__file__).resolve().parent.parent
SAMPLES = ROOT / "samples"
OUT = ROOT / "out"
REPORT = ROOT / "report"
IMG = REPORT / "img"
CLI = ROOT / "target/release/sketch2pdf"
THUMB = 320


def convert(sketch: Path) -> tuple[Path, list[str]]:
    pdf = OUT / (sketch.stem + ".pdf")
    fonts = ["--fonts", ROOT / "fonts"] if (ROOT / "fonts").is_dir() else []
    r = subprocess.run([CLI, sketch, pdf, *fonts], capture_output=True, text=True)
    return pdf, [l for l in r.stderr.splitlines() if l]


def load(sketch: Path) -> tuple[dict, list[dict]]:
    z = zipfile.ZipFile(sketch)
    doc = json.loads(z.read("document.json"))
    return doc, [json.loads(z.read(p["_ref"] + ".json")) for p in doc["pages"]]


def boards(pages: list[dict]) -> list[dict]:
    """Mirror of Doc::boards() in the library: artboards plus the symbol masters on their pages;
    else, for a pure symbol library, its masters; else each page's whole canvas.
    Each entry: name, index of its Sketch page, frame on that page's canvas (None for a page)."""
    top = [(i, l) for i, p in enumerate(pages) for l in p["layers"] if l.get("isVisible", True)]
    with_ab = {i for i, l in top if l["_class"] == "artboard"}
    found = [{"name": l["name"], "page": i, "frame": l["frame"]} for i, l in top
             if i in with_ab and l["_class"] in ("artboard", "symbolMaster")]
    if found:
        return found
    if top and all(l["_class"] == "symbolMaster" for _, l in top):
        return [{"name": l["name"], "page": i, "frame": l["frame"]} for i, l in top]
    return [{"name": p["name"], "page": i, "frame": None} for i, p in enumerate(pages)
            if any(l.get("isVisible", True) for l in p["layers"])]


def page_canvas(pdf: Path, boards_: list[dict], page: dict, size: tuple[int, int]):
    """Our artboards placed where they sit on a Sketch page's canvas, like Sketch's page preview.
    Loose layers outside artboards are not exported, so they show up in the diff."""
    frames = [l["frame"] for l in page["layers"] if l.get("isVisible", True)]
    on_page = [(i, b) for i, b in enumerate(boards_) if b["frame"] and b["page"] == page["_idx"]]
    if not frames or not on_page:
        return None
    x0, y0 = min(f["x"] for f in frames), min(f["y"] for f in frames)
    x1, y1 = max(f["x"] + f["width"] for f in frames), max(f["y"] + f["height"] for f in frames)
    s = size[0] / (x1 - x0)
    canvas = Image.new("RGB", size, "white")
    for i, b in on_page:
        f = b["frame"]
        w, h = round(f["width"] * s), round(f["height"] * s)
        if w > 0 and h > 0:
            canvas.paste(render(pdf, i + 1, (w, h)), (round((f["x"] - x0) * s), round((f["y"] - y0) * s)))
    return canvas


def aspect(w, h):
    return w / h if h else 0


def render(pdf: Path, page: int, size: tuple[int, int]) -> Image.Image:
    with tempfile.TemporaryDirectory() as d:
        base = Path(d) / "p"
        subprocess.run(
            ["pdftoppm", "-png", "-singlefile", "-f", str(page), "-l", str(page),
             "-scale-to-x", str(size[0]), "-scale-to-y", str(size[1]), pdf, base],
            check=True, capture_output=True,
        )
        return Image.open(base.with_suffix(".png")).convert("RGB")


def flatten(im: Image.Image) -> Image.Image:
    im = im.convert("RGBA")
    bg = Image.new("RGBA", im.size, "white")
    return Image.alpha_composite(bg, im).convert("RGB")


def compare(ref: Image.Image, ours: Image.Image) -> tuple[float, float, Image.Image]:
    """(similarity %, % of pixels clearly off, diff heatmap)."""
    a = np.asarray(ref, dtype=np.int16)
    b = np.asarray(ours, dtype=np.int16)
    d = np.abs(a - b).max(axis=2)
    similarity = 100 * (1 - d.mean() / 255)
    off = 100 * (d > 32).mean()
    heat = np.asarray(ref.convert("L"), dtype=np.float32) * 0.25 + 191  # faded reference
    rgb = np.stack([heat, heat, heat], axis=2)
    rgb[d > 32] = [220, 30, 30]
    return similarity, off, Image.fromarray(rgb.astype(np.uint8))


def thumb(im: Image.Image, name: str) -> str:
    im = im.copy()
    im.thumbnail((THUMB, THUMB * 3))
    im.save(IMG / name, quality=80)
    return f"img/{name}"


def row(key: str, label: str, ref: Image.Image, ours: Image.Image) -> dict:
    sim, off, diff = compare(ref, ours)
    return {
        "label": label, "sim": sim, "off": off, "size": f"{ref.width}×{ref.height}",
        "imgs": [thumb(ref, f"{key}-ref.jpg"), thumb(ours, f"{key}-ours.jpg"), thumb(diff, f"{key}-diff.jpg")],
    }


def process(sketch: Path) -> dict:
    stem = sketch.stem
    pdf, warnings = convert(sketch)
    r = {"name": stem, "warnings": warnings, "preview": None, "psd": [], "notes": []}
    if not pdf.exists():
        r["notes"].append("conversion failed")
        return r
    doc, pages = load(sketch)
    bs = boards(pages)
    names = [b["name"] for b in bs]
    r["pages"] = len(names)

    try:
        preview = flatten(Image.open(io.BytesIO(zipfile.ZipFile(sketch).read("previews/preview.png"))))
    except KeyError:
        preview = None
        r["notes"].append("file has no embedded preview")
    if preview:
        # Depending on the Sketch version the preview is the open page's whole canvas or one of
        # its artboards. Try every candidate with the preview's shape; keep the best match.
        want = aspect(*preview.size)
        cands = []
        cur = pages[min(doc.get("currentPageIndex", 0), len(pages) - 1)]
        cur["_idx"] = pages.index(cur)
        canvas = page_canvas(pdf, bs, cur, preview.size)
        if canvas is not None and abs(aspect(*canvas.size) / want - 1) < 0.02:
            frames = [l["frame"] for l in cur["layers"] if l.get("isVisible", True)]
            w = max(f["x"] + f["width"] for f in frames) - min(f["x"] for f in frames)
            h = max(f["y"] + f["height"] for f in frames) - min(f["y"] for f in frames)
            if abs(aspect(w, h) / want - 1) < 0.02:
                cands.append((f"whole page “{cur['name']}”", canvas))
        for i, b in enumerate(bs):
            f = b["frame"]
            if f and abs(aspect(f["width"], f["height"]) / want - 1) < 0.02 and len(cands) < 40:
                cands.append((f"{i + 1}. {b['name']}", render(pdf, i + 1, preview.size)))
        for i, b in enumerate(bs):
            if b["frame"] is None and (len(bs) == 1 or b["page"] == cur["_idx"]):
                cands.append((f"{i + 1}. {b['name']}", render(pdf, i + 1, preview.size)))
        if cands:
            label, im = max(cands, key=lambda c: compare(preview, c[1])[0])
            r["preview"] = row(f"{stem}-preview", "preview ≈ " + label, preview, im)
            if len(cands) > 1:
                r["notes"].append(f"preview matched best of {len(cands)} same-shaped candidates")
        else:
            r["notes"].append(f"no page or canvas has the preview's shape ({preview.width}×{preview.height})")

    psd_path = SAMPLES / "psd" / (stem + ".psd")
    if not psd_path.exists():
        return r
    try:
        psd = PSDImage.open(psd_path)
        merged = flatten(psd.topil())
    except Exception as e:
        r["notes"].append(f"could not read PSD: {e}")
        return r
    artboards = [l for l in psd if l.kind == "artboard"]
    if len(names) == 1 and len(artboards) != 1:
        # We export loose layers as one sheet; Photopea made each top-level group an artboard.
        r["notes"].append(f"PSD has {len(artboards)} root artboards; compared our single page with the whole PSD")
        r["psd"].append(row(f"{stem}-psd-0", names[0], merged, render(pdf, 1, merged.size)))
        return r
    unused = list(artboards)
    for i, n in enumerate(names):
        b = next((b for b in unused if b.name == n), None)
        if b is None:
            r["notes"].append(f"page {i + 1} “{n}”: no PSD artboard with that name")
            continue
        unused.remove(b)
        ref = merged.crop(b.bbox)
        r["psd"].append(row(f"{stem}-psd-{i}", f"{i + 1}. {n}", ref, render(pdf, i + 1, ref.size)))
    if unused:
        r["notes"].append(f"{len(unused)} PSD artboard(s) without a matching page: "
                          + ", ".join(b.name for b in unused[:5]))
    return r


def score_cell(x):
    if x is None:
        return "<td>–</td>"
    cls = "good" if x >= 97 else "ok" if x >= 92 else "bad"
    return f'<td class="{cls}">{x:.1f}%</td>'


def write_html(results: list[dict]):
    def avg(rows):
        return sum(r["sim"] for r in rows) / len(rows) if rows else None

    results.sort(key=lambda r: min([x["sim"] for x in r["psd"]] + ([r["preview"]["sim"]] if r["preview"] else [0])))
    summary = []
    for r in results:
        psd = r["psd"]
        summary.append(
            f'<tr><td><a href="#{r["name"]}">{html.escape(r["name"].split("__")[0])}</a></td>'
            f'<td>{r.get("pages", 0)}</td>{score_cell(r["preview"] and r["preview"]["sim"])}'
            f'<td>{len(psd) or "–"}</td>{score_cell(avg(psd))}{score_cell(min((x["sim"] for x in psd), default=None))}'
            f'<td>{len(r["warnings"])}</td></tr>'
        )

    def rows_html(rows):
        out = []
        for x in rows:
            imgs = "".join(f'<td><img loading="lazy" src="{src}"></td>' for src in x["imgs"])
            out.append(f'<tr><th>{html.escape(x["label"])}<br><small>{x["size"]}px</small></th>'
                       f'{score_cell(x["sim"])}<td>{x["off"]:.1f}%</td>{imgs}</tr>')
        return "".join(out)

    head = "<tr><th></th><th>similarity</th><th>pixels off</th><th>reference</th><th>sketch2pdf</th><th>diff</th></tr>"
    sections = []
    for r in results:
        notes = "".join(f"<li>{html.escape(n)}</li>" for n in r["notes"])
        warns = "".join(f"<li>{html.escape(w)}</li>" for w in r["warnings"])
        warns = f"<details><summary>{len(r['warnings'])} converter warnings</summary><ul>{warns}</ul></details>" if warns else ""
        prev = f'<h3>Embedded preview vs our page 1</h3><div class=wrap><table>{head}{rows_html([r["preview"]])}</table></div>' if r["preview"] else ""
        psd = f'<h3>Photopea PSD, per artboard</h3><div class=wrap><table>{head}{rows_html(r["psd"])}</table></div>' if r["psd"] else "<p class=muted>No PSD yet.</p>"
        sections.append(f'<section id="{r["name"]}"><h2>{html.escape(r["name"])}</h2><ul class=notes>{notes}</ul>{warns}{prev}{psd}</section>')

    all_psd = [x["sim"] for r in results for x in r["psd"]]
    all_prev = [r["preview"]["sim"] for r in results if r["preview"]]
    (REPORT / "index.html").write_text(f"""<meta charset=utf-8>
<title>sketch2pdf comparison report</title>
<meta name="claude:viewport-fit-sides" content="cover">
<style>
:root{{--bg:#fbfbfa;--fg:#1f2328;--muted:#6b7280;--line:#dde1e6;--rule:#1f2328;--good:#dff3dc;--ok:#fbf0cf;--bad:#f9dcd9;--note:#8a5a00}}
@media (prefers-color-scheme: dark){{:root:not([data-theme="light"]){{--bg:#16181c;--fg:#e6e8eb;--muted:#9aa1ab;--line:#30353c;--rule:#e6e8eb;--good:#1f3a22;--ok:#3d3315;--bad:#4a2220;--note:#e0b45c;color-scheme:dark}}}}
:root[data-theme="dark"]{{--bg:#16181c;--fg:#e6e8eb;--muted:#9aa1ab;--line:#30353c;--rule:#e6e8eb;--good:#1f3a22;--ok:#3d3315;--bad:#4a2220;--note:#e0b45c;color-scheme:dark}}
body{{font:14px/1.4 -apple-system,system-ui,sans-serif;background:var(--bg);color:var(--fg);padding-block:24px;padding-left:max(16px,env(safe-area-inset-left,0px));padding-right:max(16px,env(safe-area-inset-right,0px))}}
a{{color:inherit}}
.wrap{{overflow-x:auto}}
table{{border-collapse:collapse;margin:8px 0 16px;font-variant-numeric:tabular-nums}} td,th{{border:1px solid var(--line);padding:4px 8px;vertical-align:top;text-align:left}}
img{{max-width:{THUMB}px;background:#fff;border:1px solid var(--line)}}
.good{{background:var(--good)}}.ok{{background:var(--ok)}}.bad{{background:var(--bad)}}
.muted,small{{color:var(--muted)}} section{{border-top:2px solid var(--rule);margin-top:32px}} .notes{{color:var(--note)}}
</style>
<h1>sketch2pdf comparison report</h1>
<p>{len(results)} samples. Similarity = 100% − mean per-pixel max-channel difference; “pixels off” = share of pixels
differing by more than 32/255 (red in the diff). Transparent areas are flattened onto white.
Mean similarity: preview {sum(all_prev)/max(len(all_prev),1):.1f}% over {len(all_prev)} files,
PSD {sum(all_psd)/max(len(all_psd),1):.1f}% over {len(all_psd)} artboards. Sorted worst first.</p>
<div class=wrap><table><tr><th>sample</th><th>pages</th><th>preview</th><th>PSD artboards</th><th>PSD mean</th><th>PSD worst</th><th>warnings</th></tr>
{''.join(summary)}</table></div>
{''.join(sections)}
""")


def main():
    if not CLI.exists():
        sys.exit(f"build the CLI first: cargo build --release ({CLI} missing)")
    OUT.mkdir(exist_ok=True)
    IMG.mkdir(parents=True, exist_ok=True)
    for f in IMG.glob("*"):
        f.unlink()
    sketches = sorted((SAMPLES / "sketch").glob("*.sketch"))
    results = []
    with cf.ProcessPoolExecutor() as ex:
        futures = {ex.submit(process, s): s for s in sketches}
        for fut in cf.as_completed(futures):
            s = futures[fut]
            try:
                results.append(fut.result())
            except Exception as e:  # one bad sample should not sink the report
                results.append({"name": s.stem, "warnings": [], "preview": None, "psd": [], "notes": [f"report error: {e!r}"]})
            print(f"[{len(results)}/{len(sketches)}] {s.stem}", file=sys.stderr)
    write_html(results)
    print(REPORT / "index.html")


if __name__ == "__main__":
    main()
