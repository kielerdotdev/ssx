# Wayland coordinates in `ssx-capture-wayland`

The workspace contract (`docs/engineering-standards.md`) says every coordinate is a
**physical pixel on the virtual desktop** and that `Monitor.rect` is the rectangle the
captured `Frame` covers. Wayland does not hand us such a space, so this crate defines one.

## The mismatch

* Wayland lays outputs out in **logical** pixels (`wl_output.geometry` x/y,
  `zxdg_output_v1.logical_position/size`). A 3840x2160 panel at scale 2 occupies 1920x1080
  logical pixels.
* Screen-copy protocols return **physical** pixels: the full 3840x2160 for that panel.
* Scales can be fractional (1.25, 1.5). `wl_output.scale` is then only the ceiling; the true
  factor is `mode size / xdg-output logical size`.
* Outputs can be rotated/flipped (`wl_output.transform`). Buffers arrive in *scan-out*
  orientation, not as the user sees them.
* Different monitors can have different scales.

## The model

For each output:

| quantity | source |
|---|---|
| logical rect `L` | xdg-output logical position/size (fallback: `wl_output` x/y and `mode / integer scale`) |
| native size `N` | current mode size, width/height swapped for 90/270 transforms ("as displayed") |
| own scale `s` | `N.width / L.width`, snapped to a multiple of 1/120 when that reproduces `L` exactly (2560 px at 1.5x is 1707 logical px: raw 1.4997, reported 1.5) |

Then:

> **`S = max(s)` over all outputs, and virtual desktop = logical layout x `S`.**

* `Monitor.rect` edges are `round(L.edge * S)` (edges are rounded, not sizes, so neighbours
  stay adjacent). The highest-scale monitor gets exactly its native size.
* `Monitor.scale_factor` is the monitor's **own** `s`.
* `Frame.scale_factor` is `S`: frame pixels per logical pixel, so `frame size / S` is the
  logical size regardless of which monitor it came from.
* `Frame.origin` is the top-left of the covered `Monitor.rect`/region.

Consequences:

* **Uniform-scale layouts** (all monitors 1x, or all 2x, or all 1.5x): `S == s`, every
  `rect.size == native size`, desktop pixels *are* physical pixels. Nothing is resampled.
* **Mixed-scale layouts**: the highest-DPI monitor is pixel exact; lower-DPI monitors cover
  `S / s` desktop pixels per native pixel and are **resampled up** so that
  `frame.size == Monitor.rect.size` always holds, which lets `ssx_capture::composite` and
  `blit` stitch frames without special cases. Integer ratios (1x next to 2x) replicate pixels
  (crisp, identical to how the compositor shows them); other ratios are bilinear. Nothing
  is lost from the high-DPI monitor and nothing is invented beyond what a 2x desktop would
  show for the 1x monitor.
* `Config::desktop_scale = Some(1.0)` pins `S` to 1: the desktop is the raw logical layout
  and high-DPI monitors are downsampled instead. Use it if a consumer wants logical pixels.
* Fractional scales can leave a 1 px seam (never an overlap) between neighbours because
  logical edges do not always map to integers.

Worked examples (verified in `src/coords.rs` unit tests and, for the first three, on live
sway in `tests/sway_live.rs`):

| layout | S | rects |
|---|---|---|
| 800x600 @1x at (0,0) + 640x480 @2x (logical 320x240) at (800,0) | 2 | (0,0,1600,1200) resampled 2x, (1600,0,640,480) exact |
| 960x720 @1.5x (logical 640x480) | 1.5 | (0,0,960,720) |
| 640x480 with `transform 90` | 1 | (0,0,480,640) (as displayed) |
| two 3840x2160 @2x side by side | 2 | (0,0,3840,2160), (3840,0,3840,2160) |

## Regions

`capture_region(r)` intersects `r` with each `Monitor.rect`:

* Parts of `r` outside every monitor (gaps, beyond the edge) stay **transparent black**; a
  region overlapping nothing is `InvalidRegion`.
* One monitor per part. **Protocol-level region capture** (`wlr-screencopy`
  `capture_output_region`, cheap for small regions) is used only when it is exact: the
  monitor is pixel exact, untransformed and at an **integer** scale. The protocol takes
  *logical* coordinates, so the request is the enclosing logical box (floor/ceil) and the
  result is cropped to the exact pixels. Anything else (fractional scale, rotation,
  resampled monitor, `ext-image-copy-capture` which has no region request) captures the
  whole output and crops. This is an optimisation only; results are identical.
* Verified on live sway (`mixed_scales_use_max_scale_desktop`): unaligned and aligned
  regions on a 2x output match the painted pattern pixel for pixel.

## Transforms

Buffers are in scan-out orientation: `buffer = rotate_ccw(k * 90 deg, flip_x?(upright))`
(`wl_output.transform` numbering). `Transform::undo` inverts it exactly, so returned frames
show what the user sees and `Monitor.rect` uses the displayed size.

* sway names rotations **clockwise** (`transform 90`), `wl_output` counter-clockwise, so sway's
  `90` is reported as `Transform::Rot270`, and sway's `flipped-90` as `Flipped270`.
* Verified live against sway for normal/90/180/270 by painting an asymmetric pixel-unique
  pattern and requiring an exact match (and equality with `grim`).
* The `flipped*` transforms could **not** be verified live: sway 1.9's pixman renderer draws
  every flipped output as uniform grey (`grim` sees the same grey), so only their geometry
  and the pure-maths unit tests cover them. The maths matches wlroots' matrices
  (`flipped-90` is the transpose, `flipped-270` the anti-transpose).
* `ext-image-copy-capture-v1` reports the transform per frame; it is used as sent.

## Windows

Wayland has no window-geometry protocol; sway and Hyprland IPC provide it.

* **sway**: every `rect` in `GET_TREE` is in the global logical layout space (verified on a
  window on the second output: `rect.x == 820`). For windows with a title bar sway's `rect`
  *excludes* the bar (it sits above, `deco_rect.height` tall; `deco_rect` itself is global for
  floating windows but workspace-relative for tiled ones, so only its height is used). The
  reported `WindowInfo.rect` includes the title bar and borders (what the user sees), except for
  windows inside a tabbed/stacked strip, whose bar is shared. It is then mapped with
  `Layout::logical_to_desktop`.
* **Hyprland**: `at`/`size` are global logical layout coordinates and go through the same
  mapping. **Not verified against a live Hyprland** (not installable in the dev environment);
  parsers are covered by fixtures modelled on documented `hyprctl -j clients/monitors`
  output. `j/monitors` supplies which workspaces are visible.
* `WindowInfo.minimized` means "not on screen right now" (inactive workspace, hidden
  scratchpad, hidden Hyprland client). Such windows sort last and `capture_window` refuses them.
* Ordering is front-to-back by heuristic: fullscreen, floating (topmost first), tiled.

`capture_window` **crops the screen** to the window rectangle. Anything on top of the window
(other windows, notifications, the pointer if requested) appears in the shot, and windows
partly off screen are clipped.

### Extension point: occlusion-free window capture

If the compositor advertises `ext_foreign_toplevel_image_capture_source_manager_v1` and
`ext_foreign_toplevel_list_v1`, `capture_window` first tries the toplevel capture source
(`Config::toplevel_capture`, default on): the IPC window is matched to a toplevel by exact
title and app id, and only an **unambiguous** match is used. The frame then contains just the
window's own pixels (no decorations, no occlusion) at the buffer's size, placed at the IPC
rectangle's origin. Any failure falls back to the screen crop. This path is covered by the
mock compositor in `tests/ext_mock.rs` only, since no compositor available here implements
it. A `hyprland-toplevel-export` implementation would slot in next to it in
`WaylandCapture::capture_toplevel`.

## Known limits

* wlr-screencopy/ext capture without a `wl_shm` buffer (dma-buf only compositors) returns a
  clear error; there is no dma-buf path.
* HDR / float formats: 16-bit float shm buffers are converted assuming sRGB encoding; the
  backend never reports `hdr_float`.
* If a compositor lacks xdg-output, the logical size is derived from the mode and the
  integer `wl_output.scale`, so fractional scales cannot be recovered.
* Cursor: `include_cursor` maps to `overlay_cursor` / `paint_cursors`; a headless compositor
  has no pointer, so the flag is only checked as accepted, not for pixels.
