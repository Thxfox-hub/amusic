//! struIDEl native — нативный UI на egui.
//! Мост к Strudel (JS) и звуку пока заглушка; см. docs/NATIVE_REWRITE.md.

fn main() -> eframe::Result<()> {
    // Icône fenêtre (logo Strudel officiel, RGBA 64x64 embarqué).
    let icon = egui::IconData {
        rgba: include_bytes!("icon.rgba").to_vec(),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("struIDEl (native)")
            .with_icon(std::sync::Arc::new(icon)),
        renderer: eframe::Renderer::Glow, // Glow вместо Wgpu — меньше мерцания на Linux
        ..Default::default()
    };
    eframe::run_native(
        "struIDEl",
        options,
        Box::new(|cc| Ok(Box::new(ide_native::app::StruidelApp::new(cc)))),
    )
}
