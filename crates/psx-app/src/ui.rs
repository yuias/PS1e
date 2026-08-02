//! egui debug shell: run control, CPU registers, TTY console.

use eframe::egui;
use psx_core::{CPU_CLOCK_HZ, PsxSystem};

const REG_NAMES: [&str; 32] = [
    "zero", "at", "v0", "v1", "a0", "a1", "a2", "a3", //
    "t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", //
    "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", //
    "t8", "t9", "k0", "k1", "gp", "sp", "fp", "ra",
];

pub struct App {
    sys: PsxSystem,
    /// Kept for Reset.
    bios: Vec<u8>,
    running: bool,
    show_vram: bool,
    display_tex: Option<egui::TextureHandle>,
    vram_tex: Option<egui::TextureHandle>,
}

impl App {
    pub fn new(sys: PsxSystem, bios: Vec<u8>) -> Self {
        Self {
            sys,
            bios,
            running: false,
            show_vram: false,
            display_tex: None,
            vram_tex: None,
        }
    }

    /// Convert a VRAM rectangle (15-bit pixels) into an egui image.
    fn vram_image(&self, x0: u32, y0: u32, w: u32, h: u32) -> egui::ColorImage {
        let vram = &self.sys.bus.gpu.vram;
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            let row = (((y0 + y) & 0x1ff) as usize) * 1024;
            for x in 0..w {
                let px = vram[row + (((x0 + x) & 0x3ff) as usize)];
                // Expand 5-bit channels, replicating the top bits
                let e = |c: u16| ((c << 3) | (c >> 2)) as u8;
                pixels.push(egui::Color32::from_rgb(
                    e(px & 0x1f),
                    e((px >> 5) & 0x1f),
                    e((px >> 10) & 0x1f),
                ));
            }
        }
        egui::ColorImage {
            size: [w as usize, h as usize],
            source_size: egui::Vec2::new(w as f32, h as f32),
            pixels,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.running {
            // One video frame worth of CPU time per UI frame (assumes ~60 fps
            // UI; will be replaced by proper pacing once the GPU exists).
            self.sys.run_cycles(CPU_CLOCK_HZ / 60);
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let label = if self.running { "⏸ Pause" } else { "▶ Run" };
                if ui.button(label).clicked() {
                    self.running = !self.running;
                }
                if ui.button("Step").clicked() {
                    self.running = false;
                    self.sys.step();
                }
                if ui.button("Reset").clicked() {
                    self.running = false;
                    self.sys = PsxSystem::new(self.bios.clone()).expect("reset failed");
                }
                ui.separator();
                ui.checkbox(&mut self.show_vram, "VRAM viewer");
                ui.separator();
                ui.monospace(format!(
                    "pc {:#010x}   cycles {}",
                    self.sys.cpu.pc,
                    self.sys.cycles()
                ));
            });
        });

        egui::SidePanel::right("registers")
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("CPU");
                egui::Grid::new("regs").striped(true).show(ui, |ui| {
                    for (i, name) in REG_NAMES.iter().enumerate() {
                        ui.monospace(format!("{name:>4}"));
                        ui.monospace(format!("{:08x}", self.sys.cpu.regs[i]));
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                    ui.monospace("  hi");
                    ui.monospace(format!("{:08x}", self.sys.cpu.hi));
                    ui.monospace("  lo");
                    ui.monospace(format!("{:08x}", self.sys.cpu.lo));
                    ui.end_row();
                });
            });

        egui::TopBottomPanel::bottom("tty")
            .resizable(true)
            .default_height(160.0)
            .show(ctx, |ui| {
                ui.heading("TTY");
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.sys.tty_output().to_string())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let gpu = &self.sys.bus.gpu;
            let (w, h) = gpu.display_resolution();
            let (sx, sy) = gpu.display_vram_start();
            let enabled = gpu.display_enabled();
            let image = self.vram_image(sx, sy, w, h);
            let tex = match &mut self.display_tex {
                Some(t) => {
                    t.set(image, egui::TextureOptions::NEAREST);
                    t.clone()
                }
                None => {
                    let t = ui.ctx().load_texture(
                        "display",
                        image,
                        egui::TextureOptions::NEAREST,
                    );
                    self.display_tex = Some(t.clone());
                    t
                }
            };
            if enabled {
                // Fit the panel while keeping a 4:3 presentation aspect
                let avail = ui.available_size();
                let scale = (avail.x / 4.0).min(avail.y / 3.0);
                let size = egui::Vec2::new(scale * 4.0, scale * 3.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::new(&tex).fit_to_exact_size(size));
                });
            } else {
                ui.centered_and_justified(|ui| ui.label("display disabled"));
            }
        });

        if self.show_vram {
            let image = self.vram_image(0, 0, 1024, 512);
            let tex = match &mut self.vram_tex {
                Some(t) => {
                    t.set(image, egui::TextureOptions::NEAREST);
                    t.clone()
                }
                None => {
                    let t =
                        ctx.load_texture("vram", image, egui::TextureOptions::NEAREST);
                    self.vram_tex = Some(t.clone());
                    t
                }
            };
            egui::Window::new("VRAM (1024x512)")
                .default_width(1024.0)
                .show(ctx, |ui| {
                    ui.add(egui::Image::new(&tex));
                });
        }
    }
}
