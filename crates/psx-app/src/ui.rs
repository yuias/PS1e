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
}

impl App {
    pub fn new(sys: PsxSystem, bios: Vec<u8>) -> Self {
        Self {
            sys,
            bios,
            running: false,
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

        egui::CentralPanel::default().show(ctx, |ui| {
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
    }
}
