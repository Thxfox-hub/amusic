//! struIDEl native — нативный UI на egui.
//! Мост к Strudel (JS) и звуку пока заглушка; см. docs/NATIVE_REWRITE.md.

fn main() -> eframe::Result<()> {
    // Panic hook : un crash ne doit plus mourir en silence — détail dans le
    // panneau notifs Quickshell (notify-send) + trace écrite dans /tmp.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let msg = info.to_string();
        let _ = std::fs::write("/tmp/struidel-panic.log", format!("{msg}\n"));
        let _ = std::process::Command::new("notify-send")
            .args(["-u", "critical", "-a", "struIDEl",
                   "struidel a crashé", &msg])
            .spawn();
    }));

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
        // Pas de vsync : avec eglSwapInterval(1), un swap_buffers bloque quand
        // Wayland cesse les frame callbacks (fenêtre sur un autre workspace) —
        // c'est ce qui figeait toute l'UI/API quand l'app était occluse.
        vsync: false,
        ..Default::default()
    };
    eframe::run_native(
        "struIDEl",
        options,
        Box::new(|cc| Ok(Box::new(ide_native::app::StruidelApp::new(cc)))),
    )
}
