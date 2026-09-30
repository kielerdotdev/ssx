# ssx-editor

The image-editor **engine** of ssx: document model, tools, renderer, interactive session and
undo. It has no UI-toolkit dependency. A GUI forwards pointer/keyboard events in, and displays
the pixels and overlay geometry that come out. Image effects live in the sibling crate
[`ssx-imgfx`](../ssx-imgfx).

## Architecture

```
            pointer / key / text events                    Frames (RGBA8, straight alpha)
   GUI  ───────────────────────────────▶  EditorSession ───────────────────────────▶  GUI
        ◀───────────────────────────────    │   │   │      render(viewport, scale)
          SessionEvent (dirty rects, ...)   │   │   └── Renderer (tiny-skia + ssx-imgfx)
          Overlay (handles, guides, caret)  │   └────── History (Command, coalescing)
          CursorHint                        └────────── Document (base + canvas + objects)
```

* **`Document`** — `Arc<Frame>` base image, `Canvas` (padding + background fill), ordered
  `Vec<Object>` with stable ids, optional groups, step counter start. Because the base is an
  `Arc`, cloning a document is cheap; global operations (crop, cut-out, rotate, resize,
  effects, flatten) are recorded as before/after document snapshots.
* **`Object` = `Style` + `ObjectKind`.** Kinds: Rectangle, Ellipse, Line, Arrow (head style
  and size per end), Freehand / Freehand-arrow (Catmull-Rom smoothing), Text, Balloon (draggable
  tail tip), Step (auto-numbered, derived from z-order so deletions renumber), Magnify (lens
  with movable source), Spotlight (several combine), Blur, Pixelate, Highlight (multiply,
  rectangle or pen), Image, Sticker (built-in vectors, glyph, bitmap), Cursor stamp, Grid /
  hatch. Style: stroke colour/width/dash, fill (solid or gradient), opacity, shadow, corner
  radius, blend mode. Unknown kinds from newer files are preserved verbatim.
* **`Renderer`** — `render(&Document, RenderOptions { scale, viewport, include_guides, .. })`.
  Paints into a premultiplied `tiny-skia` pixmap; blur/pixelate/magnifier read the pixels
  below them, so the work area is grown to include everything they need. At scale 1 the base
  image is copied, so an export is pixel-exact where nothing is drawn.
* **`TextEngine`** — `cosmic-text` shaping over the **bundled Liberation Sans** (OFL 1.1, see
  `THIRD_PARTY.md`) and *no system fonts*, so text renders identically on every OS. Glyph
  outlines become vector paths, giving crisp text at any zoom/rotation with outline and shadow.
* **`History`** — every mutation is a `Command` with absolute before/after state. Drags and
  typing coalesce into one step via a `CoalesceKey`; the redo stack is cleared by new edits;
  the history is bounded by entry count and approximate bytes (a snapshot only charges for
  *new* base images).
* **`EditorSession`** — the state machine (`set_tool`, `pointer_down/move/up`, `key_down`,
  `text_insert`, selection, handles, snapping, clipboard, global ops).

### Coordinate spaces

* **Image space** (all model coordinates and all session input/output): `(0,0)` is the top-left
  pixel of the *base image*; the canvas may extend into negative coordinates (padding).
* **Output pixels** (render viewports): `canvas pixels × scale`, origin at the top-left of the
  padded canvas. Convert with `Document::image_rect_to_output(rect, scale)` /
  `Document::canvas_offset()`.

## Integrating in a GUI

```rust
let mut session = EditorSession::from_frame(captured_frame)?;
let mut renderer = ...; // the session owns one: session.render(&opts)

// Event loop (pseudo-code)
on_pointer_down(p)  => session.pointer_down(to_image(p), mods, pressure)
on_pointer_move(p)  => session.pointer_move(to_image(p), mods, pressure)   // also without buttons
on_pointer_up(p)    => session.pointer_up(to_image(p), mods)
on_double_click(p)  => session.double_click(to_image(p), mods)
on_key(k)           => if !session.key_down(k, mods) { app_shortcuts(k) }
on_text_input(s)    => session.text_insert(s)            // IME commit or typed characters
on_ime_preedit(..)  => session.ime_preedit(Some((s, cursor)))
on_zoom(z)          => session.set_view_scale(z)         // keeps hit slack/handles constant on screen

after each call:
  for ev in session.take_events() {
      Dirty(r)       => repaint_region(session.document().image_rect_to_output(r.into(), zoom))
      CanvasChanged  => re-fit view, re-upload everything
      ToolChanged / SelectionChanged / HistoryChanged / TextEditing => update toolbar, IME
      CursorChanged(c) => set_cursor(c)
      SetClipboardText(t) / PasteTextRequested => system clipboard
  }
  draw(session.overlay())        // selection outlines, handles, guides, marquee, caret, crop
```

**Textures and dirty rects.** Keep one GPU texture of the document at the current zoom (or tile
it). On `Dirty(r)`, call `session.render(&RenderOptions { scale: zoom, viewport: Some(rect), .. })`
with the rectangle converted by `image_rect_to_output`, and upload only that patch. On pan/zoom
render just the newly exposed viewport. Rendering the visible part of a 4K/50-object document
takes about 50 ms cold and single-digit milliseconds for a small dirty rectangle (see
`tests/perf.rs`). Overlay geometry (`Overlay`) is in image space, unrotated + `rotation`, so
draw it with your own toolkit's vector primitives on top of the texture.

**Keyboard.** Shortcuts handled inside `key_down` (with `ctrl`): Z / Shift+Z / Y (undo, redo),
A (select all), C / X / V (object clipboard, or text while editing), D (duplicate), G / Shift+G
(group, ungroup). Also: Delete/Backspace, arrows (nudge 1 px, Shift 10 px), Enter (commit
text / apply crop / edit selected text), Escape (cancel drag / crop / deselect / leave text
edit). Modifiers while drawing: **Shift** = constrain (45° lines, squares, circles, axis-locked
moves, aspect-preserving resize, 15° rotation), **Alt** = from centre, **Ctrl** = snap to
canvas/object edges and centres (guides appear in the overlay). Press Shift *after* the drag has
started when moving objects (Shift+press toggles selection).

**Text editing.** Enter commits (Shift+Enter inserts a newline), Escape also leaves edit mode
keeping the text; an empty text object is discarded and leaves no undo step. Creating a text
object and typing into it is one undo step. `text_edit_state()` and `overlay().caret` expose the
caret, selection rectangles and IME pre-edit for the GUI to draw.

## Deviations from ShareX / known gaps

* Arbitrary-angle rotation of the *whole document* is not offered (only 90°/180°/270° and
  flips); `ssx_imgfx::rotate` can rotate a flattened frame.
* Colour-emoji fonts are not rendered; stickers are built-in vector shapes, monochrome glyphs
  from the bundled font, or bitmaps.
* Non-Latin scripts render only if the bundled font has the glyphs (Liberation Sans covers
  Latin, Greek, Cyrillic). Register extra fonts with `TextEngine::register_font`, accepting that
  output then depends on those bytes.
* Bidirectional text is shaped correctly but caret/selection across mixed-direction runs is
  approximated.
* Zoomed-out viewport rendering resamples the base image bilinearly (no mip-chain); a GUI that
  wants perfect thumbnails should downscale the base once with `ssx_imgfx::resize`.
* Freeform/elliptical crops flatten annotations into the image (a non-rectangular result cannot
  be canvas + objects); rectangular crops stay non-destructive.
* Effect objects (blur, pixelate, magnifier, spotlight) ignore per-object opacity and blend
  mode; the magnifier honours a shadow.
* Viewport renders can differ from the same region of a full render by a few levels on
  anti-aliased edges (float rounding of the translation), never elsewhere.

## Tests

`cargo test -p ssx-editor` runs unit tests plus:

* `tests/golden.rs` — every object kind, blend/effect and global op rendered against
  `tests/golden/*.png` (tolerance-based; regenerate with `UPDATE_GOLDEN=1`).
* `tests/interaction.rs` — scripted pointer/key sequences asserting on the document (and JSON).
* `tests/undo_props.rs` — `proptest` random operation sequences: undo-all restores the initial
  document byte-for-byte, redo-all the final one.
* `tests/fuzz.rs` — thousands of random and hostile events must not panic.
* `tests/serialization.rs` — round trips, the committed v1 fixture, forward/back compatibility.
* `tests/hittest.rs`, and `src/text.rs` unit tests for text layout.
* `tests/perf.rs` — `#[ignore]`d timing reports:
  `cargo test -p ssx-editor --release --test perf -- --ignored --nocapture`.
