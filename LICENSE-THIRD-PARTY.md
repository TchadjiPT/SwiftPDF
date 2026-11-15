# Third-party software used by SwiftPDF

SwiftPDF is built on open-source components. Each keeps its own licence; the summary
below is for convenience and is not legal advice. Check the linked sources for the full text.

| Component | Used for | Licence |
|---|---|---|
| PDFium (prebuilt binaries by bblanchon) | Parsing and rendering PDFs | Apache-2.0 / BSD-style (see the `LICENSE` files in the pdfium-binaries download) |
| pdfium-render | Rust bindings to PDFium | MIT or Apache-2.0 |
| egui / eframe | User interface | MIT or Apache-2.0 |
| rfd | Native file dialogs | MIT |
| docx-rs | Writing .docx files | MIT |
| png | Writing PNG images | MIT or Apache-2.0 |
| winresource (build time only) | Embedding the icon and version info | MIT |

When you redistribute SwiftPDF (including the embedded or bundled `pdfium.dll`), include the
licence texts of the components above. `cargo install cargo-about` can generate a complete
list of all transitive Rust dependencies and their licence texts:

    cargo about init
    cargo about generate about.hbs > THIRD-PARTY-RUST.html

PDFium: <https://pdfium.googlesource.com/pdfium/> and <https://github.com/bblanchon/pdfium-binaries>
