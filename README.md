# SwiftPDF

A fast, lightweight PDF reader written in Rust. Pages load lazily on a background thread,
so long documents scroll instantly.

## Features

- Instant scrolling through long PDFs; smooth zoom that stays anchored on your cursor
- Tabs, resizable thumbnail sidebar, recent files, restore tabs on startup
- Fast full-text search, text selection and copy
- Right-click menu with highlighting (4 colours); Save / Save As writes highlights into the PDF
- Merge, split, extract pages; export to PNG images, plain text, or Word (text only)
- Password-protected PDFs, drag and drop, "Open with" support, single window (a second launch adds a tab)
- Dark / light theme, accent colours, remembered page and zoom per file

## Shortcuts

| Keys | Action |
|---|---|
| Ctrl+O | Open files |
| Ctrl+W | Close tab |
| Ctrl+Tab / Ctrl+Shift+Tab | Next / previous tab |
| Ctrl+F | Find (Enter / Shift+Enter: next / previous) |
| Ctrl+G | Go to page |
| Ctrl+C | Copy selected text |
| Ctrl+H | Highlight selection (yellow); right-click for more colours |
| Ctrl+S / Ctrl+Shift+S | Save / Save as |
| Ctrl+Scroll, Ctrl+= , Ctrl+- , Ctrl+0 | Zoom in / out / reset |
| PageUp, PageDown, Space, Home, End, arrows | Navigate |

## Build

1. Install Rust (https://rustup.rs). On Windows use the **MSVC** toolchain and the
   "Desktop development with C++" workload of the Visual Studio Build Tools.
2. Download the prebuilt PDFium for your platform (64-bit Windows: `pdfium-win-x64`)
   from <https://github.com/bblanchon/pdfium-binaries/releases> and copy `pdfium.dll`
   into the project folder (next to `Cargo.toml`).
3. Run `cargo run --release -- file.pdf`, or build a distributable with `.\package.ps1`.

If `pdfium.dll` is in the project folder while building, it is **embedded in
`swiftpdf.exe`**, so the executable works on its own. Without it, place `pdfium.dll` next to
the executable.

`cargo test` runs the unit tests.

## Where data is stored

`%APPDATA%\swiftpdf\` holds settings, the recent list, the last session, reading
positions, unsaved highlights and `crash.log`. Settings > About has an "Open data folder"
button. Delete the folder to reset everything.

## Troubleshooting

- **"PDF engine not found"**: `pdfium.dll` is missing or is the wrong architecture
  (use the 64-bit build for 64-bit Windows).
- **A PDF will not open**: it may be damaged. Password-protected files ask for the password.
- **Crashes**: please send `crash.log` from the data folder.

## Credits and licences

See `LICENSE-THIRD-PARTY.md`.
