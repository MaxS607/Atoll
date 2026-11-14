# Atoll
A smooth, tiny "interactive pill notch" for Windows. Atoll sits at the top centre of your screen as a small pill showing the time. Hover over it and it expands into a dashboard with media controls, CPU and RAM meters, and the date.

<img width="316" height="103" alt="image1" src="https://github.com/user-attachments/assets/f2ec0763-94ab-4649-bef7-64b09fbaa21f" />
<br>
<img width="642" height="229" alt="image2" src="https://github.com/user-attachments/assets/0e384afc-6d2c-4346-84e7-a47774f93178" />

## Features

- **Collapsed:** a compact pill with the current time.
- **Expanded (on hover):**
  - Clock and date (weekday abbreviations, e.g. `Sa. 14.11.2026`)
  - CPU and RAM usage as colour-coded rings (they shift colour as load rises)
  - Now-playing info: album cover, title, seek slider, previous / play-pause / next
- **While music is playing:** the collapsed pill widens slightly to show the album cover on the left and an animated audio visualizer on the right.
- **Seeking:** click or drag the slider to jump to any point in the track.
- **Pinning:** left-click the island (outside the buttons) to keep it expanded; click again to release.
- **Quit:** right-click the island.

Media info comes from the Windows media session (the same source as the system volume flyout), so it works with Spotify, browsers, and most players.

## How it works

Atoll is a single Rust file with no UI framework.

- **Fixed-size layered window.** The window never moves or resizes. Only the pill drawn inside it changes size, using per-pixel alpha.
- **Software rendering.** The pill, rings, slider and icons are drawn with signed-distance functions using float geometry, so edges are anti-aliased and animate in sub-pixel steps.
- **Smooth text.** Text is rendered once into 4x supersampled masks and box-filtered down each frame, so it glides instead of jumping.
- **SVG icons.** The media icons are SVG path data, parsed and rasterised once at your DPI scale.
- **Spring animation.** A critically damped spring drives the expand/collapse. It can be interrupted at any moment without snapping.
- **Vsync pacing.** Frames are paced by `DwmFlush`, and Atoll sleeps when nothing is animating.

## Requirements

- Windows 10 or 11
- [Rust](https://rustup.rs/) (stable, 2021 edition) with the MSVC toolchain

## Build and run

```
cargo build --release
```

The executable is at `target/release/atoll.exe`. Run it to start the island. It has no console window and no tray icon, so quit it by right-clicking the island.

### Cargo dependencies

Atoll uses the `windows` crate. Your `Cargo.toml` needs these features:

```toml
[dependencies.windows]
version = "0.58"
features = [
    "Foundation",
    "Foundation_Collections",
    "Graphics_Imaging",
    "Media_Control",
    "Storage_Streams",
    "Win32_Foundation",
    "Win32_Graphics_Dwm",
    "Win32_Graphics_Gdi",
    "Win32_System_Com",
    "Win32_System_LibraryLoader",
    "Win32_System_Performance",
    "Win32_System_SystemInformation",
    "Win32_System_Threading",
    "Win32_UI_HiDpi",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_WindowsAndMessaging",
]
```

Adjust the version to match the one your project already uses.

## Controls

| Action | Result |
| --- | --- |
| Hover the pill | Expand |
| Move the cursor away | Collapse (unless pinned) |
| Left-click the pill | Pin or unpin the expanded state |
| Click prev / play-pause / next | Control the current media session |
| Click or drag the slider | Seek within the track |
| Right-click | Quit |

## Customising

All layout values are constants at the top of `src/main.rs`, in logical pixels at 96 DPI:

| Constants | What they control |
| --- | --- |
| `COLLAPSED_*`, `EXPANDED_*`, `MAX_RADIUS` | Pill sizes and corner radius |
| `TOP_MARGIN` | Distance from the top of the screen |
| `RING_*`, `LABEL_CX` | CPU and RAM ring position, size and thickness |
| `ALBUM_ART_*`, `COLL_ART_*` | Album cover in the expanded and collapsed states |
| `SLIDER_*`, `CX_*`, `CY_BTN`, `ICON_BOX` | Slider, button positions and icon size |
| `OMEGA` | Animation speed (higher is snappier, about 20 settles in 0.3 s) |

Colours for the CPU and RAM rings are set in `get_cpu_color` and `get_ram_color`. The date format and the German weekday names are in `Renderer::update_text`.

## Notes and limitations

- The window is placed on the primary monitor, centred at the top.
- The media controls only appear while a media session exists.
- The weekday names are currently in German.
