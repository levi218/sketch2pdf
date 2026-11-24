# sketch2pdf

[![CI](https://github.com/levi218/sketch2pdf/actions/workflows/ci.yml/badge.svg)](https://github.com/levi218/sketch2pdf/actions/workflows/ci.yml)
[![Demo](https://img.shields.io/badge/demo-GitHub%20Pages-2563eb)](https://levi218.github.io/sketch2pdf/)
[![License: GPL v3](https://img.shields.io/badge/license-GPLv3-blue.svg)](LICENSE)

Convert Sketch (`.sketch`, v43+ JSON format) documents to PDF, without Sketch or macOS.

Each artboard becomes one PDF page, in document order. Drawing goes through Skia's PDF
backend, so shapes and text stay vector. Blurs and shadows are rasterised by Skia.

## Usage

```bash
cargo build --release
```

```bash
target/release/sketch2pdf design.sketch -o design.pdf --fonts ./fonts
```

- The output defaults to the input path with a `.pdf` extension.
- `--fonts DIR` can be repeated. It loads every `.ttf`, `.otf` and `.ttc` file under `DIR`, searching subdirectories too. Fonts embedded in the document are used first, then these, then system fonts.
- Anything that can't be rendered faithfully, such as a missing font or an unsupported image, is printed as a `warning:` line on stderr.

As a library:

```rust
let out = sketch2pdf::convert_with_fonts(&std::fs::read("design.sketch")?, &[])?;
std::fs::write("design.pdf", out.pdf)?;
// out.warnings: Vec<String>
```

### In the browser

`docs/` is a static converter page that runs `sketch2pdf` as WebAssembly, entirely client-side. Each release deploys it to [GitHub Pages](https://levi218.github.io/sketch2pdf/).

To build `docs/sketch2pdf.js` and `docs/sketch2pdf_wasm.wasm`, activate [emsdk](https://emscripten.org/docs/getting_started/downloads.html), then:

```bash
rustup target add wasm32-unknown-emscripten
```

```bash
./build-wasm.sh
```

To try the page locally, build it, then serve `docs/` over HTTP (for example `python3 -m http.server -d docs`). The browser has no system fonts, so text needs fonts embedded in the document or added on the page.

## Releasing

Push a version tag:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

The release workflow builds the CLI for Linux (x86_64), macOS (Apple silicon) and Windows (x86_64), and the web demo as a zip. It attaches them all to a GitHub release for the tag, then deploys the demo to GitHub Pages.

The Pages deploy is skipped while the repository is private. Once it's public, turn on Pages and allow deploys from version tags (by default, only the default branch can deploy):

```bash
gh api -X POST repos/levi218/sketch2pdf/pages -f build_type=workflow
```

```bash
gh api -X POST repos/levi218/sketch2pdf/environments/github-pages/deployment-branch-policies -f name='v*' -f type=tag
```

## Project layout

- `sketch2pdf/` is the library. The whole renderer is in `src/lib.rs`.
- `sketch2pdf-cli/` is the `sketch2pdf` command-line tool.
- `sketch2pdf-wasm/` exposes the library to JavaScript as a C ABI, for the emscripten build. `docs/` holds the page that uses it.
- `tools/report.py` compares the output against two references: the preview Sketch embeds in each file, and Photopea PSD exports of the same file. It writes an HTML report to `report/`. Setup and run instructions are in the script's docstring.
- `samples/` holds test files and is not tracked. `cargo test` converts every `samples/sketch/*.sketch` file when the directory is present.

## Sketch file structure

A `.sketch` file is a zip archive of JSON files and assets:

```
document.json          document root: page list, shared styles, foreign symbols
meta.json              app version, plus a map of pages → artboards by ID and name
user.json              per-user UI state (zoom, scroll position); not used here
pages/<UUID>.json      one file per page, holding its full layer tree
images/<sha1>[.ext]    bitmaps (PNG, JPEG, …) and vector images (PDF)
fonts/<sha1>           embedded font files
previews/preview.png   thumbnail of the first page
```

### document.json

- `pages`: an ordered list of `{"_class": "MSJSONFileReference", "_ref": "pages/<UUID>"}`. Each `_ref` resolves to `<_ref>.json` inside the archive.
- `layerStyles.objects` and `layerTextStyles.objects`: shared styles, looked up by `do_objectID` through a layer's `sharedStyleID`.
- `foreignSymbols`, `foreignLayerStyles` and `foreignTextStyles`: symbols and styles imported from libraries, copied into the document.

### Layers

Every object has a `_class` and a `do_objectID`. A page is a tree of layers:

| `_class` | Meaning |
| --- | --- |
| `artboard` | A screen. Each artboard is one PDF page. |
| `symbolMaster` | A reusable component, identified by `symbolID`. |
| `symbolInstance` | A placed copy of a master, with `overrideValues`. |
| `group` | A container for child `layers`. |
| `shapeGroup` | Child shapes combined with `booleanOperation` (union, subtract, intersect, difference). |
| `rectangle`, `oval`, `triangle`, `star`, `polygon`, `shapePath` | Vector paths made of `points` (curve points with `curveFrom`/`curveTo`). |
| `text` | Text stored as an `attributedString` (a string plus attribute runs for font, size, colour and so on). |
| `bitmap` | An image whose `image._ref` points into `images/`. |
| `slice`, `MSImmutableHotspotLayer` | Export and prototyping helpers. These aren't drawn. |

Fields that most layers share:

- `frame` (`x`, `y`, `width`, `height`): the layer's position, relative to its parent.
- `rotation`, `isFlippedHorizontal`, `isFlippedVertical` and `isVisible`.
- `style`: `fills`, `borders`, `shadows`, `innerShadows`, `blur` and `contextSettings` (opacity and blend mode). Each entry has its own `isEnabled` flag.
- `hasClippingMask` makes the layer a mask that clips the siblings above it. A sibling with `shouldBreakMaskChain` ends the mask.
- `resizingConstraint` is a bitmask that controls how a child resizes when a symbol instance is a different size from its master.

Points such as `"{0.5, 1}"` are stored as strings. Their coordinates are normalised to the layer's frame (0 to 1).

### Symbol overrides

Instance overrides are keyed by the path of layer IDs from the instance down to the target layer, joined by `/` and followed by a property name. For example, `<nestedInstanceID>/<textLayerID>_stringValue` replaces that layer's text. `<layerID>_symbolID` swaps a nested symbol, and `<layerID>_layerStyle` swaps a shared style. Overrides set on an outer instance take priority over those set on an inner one.

## Limitations

- Vector (PDF) images in `images/` are skipped and reported as a warning.
- If a symbol swap override brings in a background blur, the blur isn't drawn.
