//! The keyboard binding dialog: a DualShock diagram with one label box per
//! digital button, armed by a click and bound by the next keypress.
//!
//! The controller is painted rather than loaded from an image. It costs no
//! decoder and no megabyte in the binary, it scales to whatever size the
//! window is dragged to, it takes its colours from the egui theme so it is
//! readable in light and dark alike, and the button being bound can be lit
//! up — none of which a bitmap gives for free.
//!
//! The outlines below were traced off a DualShock line drawing and are in
//! its pixel coordinates, so a shape can be checked against the original by
//! its numbers. Everything left of [`AXIS`] is mirrored to make the right
//! half, which is why only one shoulder pad and one d-pad arm are stored.
//!
//! The analog sticks are drawn but not bindable: the emulated controller is
//! a digital pad, so it has no L3/R3 for a key to reach.

use crate::config::KeyBindings;
use eframe::egui;

/// Button names in [`KeyBindings::pairs`] order, which is the order every
/// table in this module indexes by.
pub const BUTTON_NAMES: [&str; 14] = [
    "up", "down", "left", "right", "cross", "circle", "square", "triangle", "L1", "R1", "L2", "R2",
    "start", "select",
];

/// The controller's centre line, in drawing coordinates.
const AXIS: f32 = 724.0;

/// Left half of the body, from the notch between the grips round to the
/// top edge. The shoulder pads were lifted out before tracing, so the
/// outline runs along their edge and disappears under them.
#[rustfmt::skip]
const BODY_HALF: [(f32, f32); 38] = [
    (724.0, 712.0), (664.0, 712.0), (641.0, 751.0), (615.0, 777.0),
    (584.0, 795.0), (544.0, 806.0), (500.0, 806.0), (462.0, 796.0),
    (427.0, 776.0), (396.0, 744.0), (288.0, 949.0), (265.0, 976.0),
    (226.0, 1001.0), (183.0, 1014.0), (144.0, 1015.0), (122.0, 1011.0),
    (97.0, 1002.0), (62.0, 980.0), (36.0, 950.0), (20.0, 914.0),
    (12.0, 874.0), (11.0, 825.0), (65.0, 415.0), (74.0, 371.0),
    (83.0, 345.0), (113.0, 291.0), (151.0, 252.0), (155.0, 249.0),
    (164.0, 258.0), (192.0, 243.0), (229.0, 231.0), (276.0, 225.0),
    (327.0, 230.0), (364.0, 242.0), (407.0, 267.0), (422.0, 252.0),
    (448.0, 245.0), (724.0, 245.0),
];

#[rustfmt::skip]
const L2_PAD: [(f32, f32); 15] = [
    (299.0, 60.0), (358.0, 67.0), (366.0, 71.0), (383.0, 88.0),
    (389.0, 119.0), (389.0, 137.0), (374.0, 153.0), (334.0, 147.0),
    (279.0, 146.0), (232.0, 151.0), (202.0, 159.0), (188.0, 145.0),
    (202.0, 93.0), (219.0, 75.0), (249.0, 65.0),
];

#[rustfmt::skip]
const L1_PAD: [(f32, f32); 20] = [
    (276.0, 134.0), (344.0, 136.0), (367.0, 140.0), (384.0, 148.0),
    (406.0, 173.0), (421.0, 252.0), (406.0, 266.0), (384.0, 251.0),
    (342.0, 233.0), (310.0, 226.0), (276.0, 224.0), (251.0, 226.0),
    (214.0, 234.0), (185.0, 245.0), (164.0, 257.0), (150.0, 240.0),
    (169.0, 173.0), (190.0, 152.0), (200.0, 147.0), (231.0, 139.0),
];

/// The "up" d-pad arm, relative to [`DPAD`]. The other three are this one
/// turned a quarter, a half and three quarters.
#[rustfmt::skip]
const DPAD_ARM: [(f32, f32); 9] = [
    (-28.0, -110.0), (29.0, -110.0), (37.0, -102.0), (37.0, -57.0),
    (6.0, -27.0), (-2.0, -26.0), (-8.0, -28.0), (-37.0, -58.0),
    (-37.0, -101.0),
];

/// Centres of the d-pad and of the button ring around it. The face side
/// mirrors both.
const DPAD: (f32, f32) = (274.0, 432.0);
/// Ring radii: the raised collar and the recess inside it.
const RING: (f32, f32) = (206.0, 185.0);
/// Analog stick centre, and the radii of its three circles.
const STICK: (f32, f32) = (521.0, 640.0);
const STICK_R: [f32; 3] = [141.0, 102.0, 87.0];
/// The two inner circles were traced a few units above the outer collar, so
/// they keep their own centre line rather than sharing [`STICK`]'s.
const STICK_INNER_Y: f32 = 637.0;

/// The whole diagram is laid out in this fixed space and scaled to fit the
/// dialog. The drawing occupies 1448x1086 of it at [`ORIGIN`]; the margin
/// left over is what the label boxes and their leader lines live in.
const CANVAS: egui::Vec2 = egui::vec2(2048.0, 1120.0);
const ORIGIN: egui::Vec2 = egui::vec2(300.0, 50.0);

/// Label box size, in canvas units.
const BOX: egui::Vec2 = egui::vec2(250.0, 54.0);

/// Line weight of the drawing, in canvas units.
const LINE: f32 = 6.0;

/// Maps canvas coordinates onto the screen rect the dialog handed us.
#[derive(Clone, Copy)]
struct View {
    origin: egui::Pos2,
    scale: f32,
}

impl View {
    /// Fit [`CANVAS`] into `rect`, centred, without distorting it.
    fn fit(rect: egui::Rect) -> Self {
        let scale = (rect.width() / CANVAS.x).min(rect.height() / CANVAS.y);
        let used = CANVAS * scale;
        Self {
            origin: rect.center() - used / 2.0,
            scale,
        }
    }

    /// A point in canvas coordinates.
    fn at(self, x: f32, y: f32) -> egui::Pos2 {
        self.origin + egui::vec2(x, y) * self.scale
    }

    /// A point in the controller drawing's own coordinates.
    fn art(self, x: f32, y: f32) -> egui::Pos2 {
        self.at(x + ORIGIN.x, y + ORIGIN.y)
    }

    fn len(self, v: f32) -> f32 {
        v * self.scale
    }

    fn stroke(self, width: f32, color: egui::Color32) -> egui::Stroke {
        egui::Stroke::new(self.len(width).max(1.0), color)
    }
}

/// Fill a polygon by fanning triangles out from its centroid. egui's own
/// path fill assumes a convex outline, and the shoulder pads are not one:
/// their lower edge curves up around the button ring.
fn fill_poly(p: &egui::Painter, pts: &[egui::Pos2], color: egui::Color32) {
    if pts.len() < 3 {
        return;
    }
    let centre = pts.iter().fold(egui::Vec2::ZERO, |a, p| a + p.to_vec2()) / pts.len() as f32;
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(centre.to_pos2(), color);
    for &pt in pts {
        mesh.colored_vertex(pt, color);
    }
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    p.add(egui::Shape::mesh(mesh));
}

/// A quarter turn clockwise about the origin, `turns` times. Screen
/// coordinates run y down, so that is (x, y) -> (-y, x).
fn turn(turns: u8, x: f32, y: f32) -> (f32, f32) {
    match turns % 4 {
        1 => (-y, x),
        2 => (-x, -y),
        3 => (y, -x),
        _ => (x, y),
    }
}

/// The outline of a pressable part, in the drawing's coordinates.
#[derive(Clone, Copy)]
enum Art {
    /// A traced outline; `true` mirrors it about [`AXIS`].
    Poly(&'static [(f32, f32)], bool),
    Circle {
        x: f32,
        y: f32,
        r: f32,
    },
    /// `x`/`y` is the top-left corner, as in the source drawing.
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
    },
    Tri([(f32, f32); 3]),
    /// One of the d-pad arms: [`DPAD_ARM`] turned this many quarter turns
    /// clockwise from "up".
    Arm(u8),
}

impl Art {
    /// The outline as screen points, for everything but the two shapes egui
    /// draws better itself.
    fn points(self, view: View) -> Vec<egui::Pos2> {
        match self {
            Art::Poly(pts, mirror) => pts
                .iter()
                .map(|&(x, y)| view.art(if mirror { 2.0 * AXIS - x } else { x }, y))
                .collect(),
            Art::Tri(pts) => pts.iter().map(|&(x, y)| view.art(x, y)).collect(),
            Art::Arm(turns) => DPAD_ARM
                .iter()
                .map(|&(x, y)| {
                    let (dx, dy) = turn(turns, x, y);
                    view.art(DPAD.0 + dx, DPAD.1 + dy)
                })
                .collect(),
            Art::Circle { .. } | Art::Rect { .. } => Vec::new(),
        }
    }

    fn paint(self, p: &egui::Painter, view: View, fill: egui::Color32, stroke: egui::Stroke) {
        match self {
            Art::Rect { x, y, w, h, r } => {
                let rect = egui::Rect::from_min_max(view.art(x, y), view.art(x + w, y + h));
                let cr = egui::CornerRadius::same(view.len(r).round().clamp(0.0, 255.0) as u8);
                p.rect_filled(rect, cr, fill);
                p.rect_stroke(rect, cr, stroke, egui::StrokeKind::Middle);
            }
            Art::Circle { x, y, r } => {
                p.circle(view.art(x, y), view.len(r), fill, stroke);
            }
            _ => {
                let pts = self.points(view);
                fill_poly(p, &pts, fill);
                p.add(egui::Shape::closed_line(pts, stroke));
            }
        }
    }

    /// Where a mark printed on this shape goes.
    fn centre(self) -> (f32, f32) {
        match self {
            Art::Circle { x, y, .. } => (x, y),
            Art::Rect { x, y, w, h, .. } => (x + w / 2.0, y + h / 2.0),
            _ => (0.0, 0.0),
        }
    }
}

/// The mark printed on a button, once the button itself is drawn.
#[derive(Clone, Copy)]
enum Mark {
    None,
    /// A caption at a fixed point in the drawing (the shoulder pads).
    Text(&'static str, (f32, f32)),
    /// Centred on the button, for the four face buttons.
    Triangle,
    Circle,
    Cross,
    Square,
}

impl Mark {
    fn paint(self, p: &egui::Painter, view: View, art: Art, ink: egui::Color32) {
        let (cx, cy) = art.centre();
        let thin = view.stroke(LINE - 1.0, ink);
        match self {
            Mark::None => {}
            Mark::Text(text, (x, y)) => {
                let font = egui::FontId::proportional(view.len(42.0));
                p.text(view.art(x, y), egui::Align2::CENTER_CENTER, text, font, ink);
            }
            Mark::Triangle => {
                let pts = [
                    (cx, cy - 26.0),
                    (cx + 23.0, cy + 20.0),
                    (cx - 23.0, cy + 20.0),
                ];
                let pts: Vec<_> = pts.iter().map(|&(x, y)| view.art(x, y)).collect();
                p.add(egui::Shape::closed_line(pts, thin));
            }
            Mark::Circle => {
                p.circle_stroke(view.art(cx, cy), view.len(26.0), thin);
            }
            Mark::Cross => {
                p.line_segment(
                    [
                        view.art(cx - 20.0, cy - 20.0),
                        view.art(cx + 20.0, cy + 20.0),
                    ],
                    thin,
                );
                p.line_segment(
                    [
                        view.art(cx + 20.0, cy - 20.0),
                        view.art(cx - 20.0, cy + 20.0),
                    ],
                    thin,
                );
            }
            Mark::Square => {
                let rect = egui::Rect::from_min_max(
                    view.art(cx - 20.0, cy - 20.0),
                    view.art(cx + 20.0, cy + 20.0),
                );
                p.rect_stroke(
                    rect,
                    egui::CornerRadius::ZERO,
                    thin,
                    egui::StrokeKind::Middle,
                );
            }
        }
    }
}

/// How a leader line leaves its label box: horizontal first, or vertical
/// first. Either way it turns once before reaching the button.
#[derive(Clone, Copy)]
enum Route {
    Horizontal,
    Vertical,
}

/// One bindable button on the diagram.
struct Slot {
    /// Index into [`BUTTON_NAMES`] and [`KeyBindings::pairs`].
    idx: usize,
    art: Art,
    mark: Mark,
    /// Where the leader line meets the button, in drawing coordinates.
    anchor: (f32, f32),
    /// Centre of the label box, in canvas coordinates.
    label: (f32, f32),
    route: Route,
    /// Canvas coordinate the leader turns at. `None` turns level with the
    /// anchor, which is what all but one of them can do; that one would take
    /// the corner through the analog stick.
    elbow: Option<f32>,
    /// Drawn before the two rings, the way the shoulder pads sit behind them
    /// on the controller. Everything else goes on top.
    behind: bool,
}

const fn slot(
    idx: usize,
    art: Art,
    mark: Mark,
    anchor: (f32, f32),
    label: (f32, f32),
    route: Route,
) -> Slot {
    Slot {
        idx,
        art,
        mark,
        anchor,
        label,
        route,
        elbow: None,
        behind: false,
    }
}

const fn elbow(s: Slot, at: f32) -> Slot {
    Slot {
        elbow: Some(at),
        ..s
    }
}

const fn behind(s: Slot) -> Slot {
    Slot { behind: true, ..s }
}

/// The layout. Anchors sit on the edge of the button the line comes in
/// from, so no leader ends inside the shape it points at.
#[rustfmt::skip]
const SLOTS: [Slot; 14] = [
    // D-pad, labelled down the left margin.
    slot(0, Art::Arm(0), Mark::None, (237.0, 364.0), (150.0, 350.0), Route::Horizontal),
    slot(1, Art::Arm(2), Mark::None, (274.0, 542.0), (150.0, 560.0), Route::Horizontal),
    slot(2, Art::Arm(3), Mark::None, (164.0, 432.0), (150.0, 440.0), Route::Horizontal),
    slot(3, Art::Arm(1), Mark::None, (342.0, 469.0), (150.0, 660.0), Route::Horizontal),
    // Face buttons, labelled down the right margin. Square is the far side
    // of the cluster: its leader turns in the gap between the stick and
    // cross rather than level with the button.
    slot(4, Art::Circle { x: 1175.0, y: 535.0, r: 51.0 }, Mark::Cross, (1226.0, 535.0), (1898.0, 560.0), Route::Horizontal),
    slot(5, Art::Circle { x: 1281.0, y: 432.0, r: 51.0 }, Mark::Circle, (1332.0, 432.0), (1898.0, 440.0), Route::Horizontal),
    elbow(slot(6, Art::Circle { x: 1069.0, y: 432.0, r: 51.0 }, Mark::Square, (1069.0, 483.0), (1898.0, 660.0), Route::Horizontal), 1372.0),
    slot(7, Art::Circle { x: 1175.0, y: 329.0, r: 51.0 }, Mark::Triangle, (1226.0, 329.0), (1898.0, 350.0), Route::Horizontal),
    // Shoulder pads. L1/R1 continue the side columns, L2/R2 go along the top.
    behind(slot(8, Art::Poly(&L1_PAD, false), Mark::Text("L1", (285.0, 197.0)), (150.0, 240.0), (150.0, 260.0), Route::Horizontal)),
    behind(slot(9, Art::Poly(&L1_PAD, true), Mark::Text("R1", (1163.0, 197.0)), (1298.0, 240.0), (1898.0, 260.0), Route::Horizontal)),
    behind(slot(10, Art::Poly(&L2_PAD, false), Mark::Text("L2", (288.0, 107.0)), (290.0, 60.0), (590.0, 45.0), Route::Vertical)),
    behind(slot(11, Art::Poly(&L2_PAD, true), Mark::Text("R2", (1160.0, 107.0)), (1158.0, 60.0), (1458.0, 45.0), Route::Vertical)),
    // Start and select reach down from the top margin, between the pads.
    slot(12, Art::Tri([(812.0, 380.0), (892.0, 406.0), (812.0, 433.0)]), Mark::None, (812.0, 380.0), (1120.0, 45.0), Route::Vertical),
    slot(13, Art::Rect { x: 553.0, y: 388.0, w: 81.0, h: 40.0, r: 12.0 }, Mark::None, (593.0, 388.0), (860.0, 45.0), Route::Vertical),
];

/// The dialog's own state: the bindings being edited, and which button is
/// waiting for a keypress. The caller keeps the live bindings untouched
/// until [`Outcome::Accept`].
pub struct Binder {
    draft: KeyBindings,
    /// Index into [`SLOTS`]' `idx` space, set by clicking a label box.
    armed: Option<usize>,
}

/// What the dialog wants done once a frame of it has been drawn.
pub enum Outcome {
    /// Still open; nothing to do.
    Open,
    /// Boxed: a whole `KeyBindings` inline would make every `Outcome` as
    /// wide as one, for the sake of the branch that carries it.
    Accept(Box<KeyBindings>),
    Cancel,
}

impl Binder {
    pub fn new(keys: &KeyBindings) -> Self {
        Self {
            draft: keys.clone(),
            armed: None,
        }
    }

    /// True while a keypress would be swallowed as a binding. The caller
    /// uses this to keep that press off the pad and off its own shortcuts.
    pub fn capturing(&self) -> bool {
        self.armed.is_some()
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        self.capture(ctx);

        let mut outcome = Outcome::Open;
        let mut open = true;
        egui::Window::new("Keyboard mapping")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(1000.0, 600.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Defaults").clicked() {
                        self.draft = KeyBindings::default();
                        self.armed = None;
                    }
                    ui.separator();
                    if ui.button("OK").clicked() {
                        outcome = Outcome::Accept(Box::new(self.draft.clone()));
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = Outcome::Cancel;
                    }
                    ui.separator();
                    ui.label(match self.armed {
                        Some(i) => format!("press a key for {}  (Esc to cancel)", BUTTON_NAMES[i]),
                        None => "click a box, then press the key to bind".to_string(),
                    });
                });
                ui.separator();
                // Whatever is left of the window after the button row; the
                // diagram scales into it rather than forcing a scroll bar.
                let rect = ui.available_rect_before_wrap();
                ui.advance_cursor_after_rect(rect);
                self.diagram(ui, rect);
            });

        if !open && matches!(outcome, Outcome::Open) {
            // The window's own close button means the same as Cancel.
            outcome = Outcome::Cancel;
        }
        outcome
    }

    /// Take the first real keypress while armed. Repeats are ignored so a
    /// held key binds once, and Escape disarms instead of binding itself —
    /// otherwise the only way out of a misclick would be to bind Escape.
    fn capture(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.armed else { return };
        let key = ctx.input_mut(|input| {
            let mut found = None;
            input.events.retain(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } if found.is_none() => {
                    found = Some(*key);
                    false
                }
                _ => true,
            });
            found
        });
        let Some(key) = key else { return };
        self.armed = None;
        if key != egui::Key::Escape {
            *self.draft.fields_mut()[idx] = key.name().to_string();
        }
    }

    fn diagram(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let view = View::fit(rect);
        let visuals = ui.visuals().clone();
        let ink = visuals.strong_text_color();
        let line = view.stroke(LINE, ink);
        let leader = view.stroke(LINE - 2.0, visuals.weak_text_color());

        let painter = ui.painter_at(rect);
        body(&painter, view, line);

        // Leader lines before the buttons, so a button's fill covers the end
        // of the line rather than the line running across the button.
        for slot in &SLOTS {
            painter.add(egui::Shape::line(leader_points(view, slot), leader));
        }

        let paint = |behind: bool| {
            for slot in SLOTS.iter().filter(|s| s.behind == behind) {
                let armed = self.armed == Some(slot.idx);
                // Only an armed button is filled. The drawing's shapes are
                // meant to cross each other -- the ring and the stick do --
                // so an opaque fill would rub out the arc underneath.
                let fill = if armed {
                    visuals.selection.bg_fill
                } else {
                    egui::Color32::TRANSPARENT
                };
                slot.art.paint(&painter, view, fill, line);
                slot.mark.paint(&painter, view, slot.art, ink);
            }
        };
        // The rings cross the shoulder pads and the face buttons sit inside
        // the right one, so the order is pads, rings, then everything else.
        paint(true);
        for x in [DPAD.0, 2.0 * AXIS - DPAD.0] {
            for r in [RING.0, RING.1] {
                painter.circle_stroke(view.art(x, 433.0), view.len(r), line);
            }
        }
        paint(false);
        chassis(&painter, view, line);

        let duplicates = self.duplicates();
        for slot in &SLOTS {
            self.label_box(ui, view, slot, duplicates, &visuals);
        }
    }

    /// A bitmask of the buttons whose key is bound to some other button as
    /// well. Both are flagged; neither is an error the dialog refuses, since
    /// two buttons on one key is occasionally what someone wants.
    fn duplicates(&self) -> u16 {
        let keys: Vec<&str> = self.draft.pairs().iter().map(|&(k, _)| k).collect();
        let mut mask = 0u16;
        for (i, a) in keys.iter().enumerate() {
            if keys
                .iter()
                .enumerate()
                .any(|(j, b)| j != i && a.eq_ignore_ascii_case(b))
            {
                mask |= 1 << i;
            }
        }
        mask
    }

    fn label_box(
        &mut self,
        ui: &mut egui::Ui,
        view: View,
        slot: &Slot,
        duplicates: u16,
        visuals: &egui::Visuals,
    ) {
        let armed = self.armed == Some(slot.idx);
        let centre = view.at(slot.label.0, slot.label.1);
        let rect = egui::Rect::from_center_size(centre, BOX * view.scale);

        let text = if armed {
            "press a key".to_string()
        } else {
            self.draft.pairs()[slot.idx].0.to_string()
        };
        let stroke = if duplicates & (1 << slot.idx) != 0 {
            egui::Stroke::new(2.0, visuals.error_fg_color)
        } else {
            visuals.widgets.inactive.bg_stroke
        };
        let button = egui::Button::new(text).stroke(stroke).selected(armed);
        let response = ui.put(rect, button).on_hover_text(format!(
            "{} — click, then press a key",
            BUTTON_NAMES[slot.idx]
        ));
        if response.clicked() {
            self.armed = if armed { None } else { Some(slot.idx) };
        }
    }
}

/// The two-segment leader line from a label box to its button.
fn leader_points(view: View, slot: &Slot) -> Vec<egui::Pos2> {
    let (bx, by) = slot.label;
    let anchor = (slot.anchor.0 + ORIGIN.x, slot.anchor.1 + ORIGIN.y);
    let half = BOX / 2.0;
    match slot.route {
        Route::Horizontal => {
            let edge = bx + half.x * (anchor.0 - bx).signum();
            let turn = slot.elbow.unwrap_or(anchor.0);
            vec![
                view.at(edge, by),
                view.at(turn, by),
                view.at(turn, anchor.1),
                view.at(anchor.0, anchor.1),
            ]
        }
        Route::Vertical => {
            let edge = by + half.y * (anchor.1 - by).signum();
            let turn = slot.elbow.unwrap_or(anchor.1);
            vec![
                view.at(bx, edge),
                view.at(bx, turn),
                view.at(anchor.0, turn),
                view.at(anchor.0, anchor.1),
            ]
        }
    }
}

/// The body silhouette: the traced half plus its mirror image.
fn body(p: &egui::Painter, view: View, line: egui::Stroke) {
    let mut pts: Vec<_> = BODY_HALF.iter().map(|&(x, y)| view.art(x, y)).collect();
    pts.extend(
        BODY_HALF
            .iter()
            .rev()
            .map(|&(x, y)| view.art(2.0 * AXIS - x, y)),
    );
    p.add(egui::Shape::closed_line(pts, line));
}

/// Everything the dialog draws but does not bind: the analog sticks and the
/// bar between them, the d-pad's direction arrows, the analog toggle and the
/// three printed captions.
fn chassis(p: &egui::Painter, view: View, line: egui::Stroke) {
    for x in [STICK.0, 2.0 * AXIS - STICK.0] {
        p.circle_stroke(view.art(x, STICK.1), view.len(STICK_R[0]), line);
        for r in &STICK_R[1..] {
            p.circle_stroke(view.art(x, STICK_INNER_Y), view.len(*r), line);
        }
    }
    for (x, y, w, h) in [(664.0, 664.0, 120.0, 48.0), (693.0, 536.0, 62.0, 35.0)] {
        Art::Rect {
            x,
            y,
            w,
            h,
            r: 10.0,
        }
        .paint(p, view, egui::Color32::TRANSPARENT, line);
    }

    // The solid arrowheads printed outside each d-pad arm.
    for turns in 0..4u8 {
        let pts: Vec<_> = [(0.0f32, -152.0f32), (13.0, -131.0), (-13.0, -131.0)]
            .iter()
            .map(|&(x, y)| {
                let (dx, dy) = turn(turns, x, y);
                view.art(DPAD.0 + dx, DPAD.1 + dy)
            })
            .collect();
        p.add(egui::Shape::convex_polygon(
            pts,
            line.color,
            egui::Stroke::NONE,
        ));
    }

    let font = egui::FontId::proportional(view.len(29.0));
    for (x, y, text) in [
        (593.0, 456.0, "SELECT"),
        (855.0, 458.0, "START"),
        (724.0, 511.0, "ANALOG"),
    ] {
        p.text(
            view.art(x, y),
            egui::Align2::CENTER_CENTER,
            text,
            font.clone(),
            line.color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diagram has to cover the pad exactly once, or a button would be
    /// unreachable from the dialog (or two boxes would edit one binding).
    #[test]
    fn every_button_has_one_slot() {
        let mut seen = 0u16;
        for slot in &SLOTS {
            assert!(
                seen & (1 << slot.idx) == 0,
                "slot {} listed twice",
                slot.idx
            );
            seen |= 1 << slot.idx;
        }
        assert_eq!(seen, (1u16 << BUTTON_NAMES.len()) - 1);
    }

    /// Capture stores `Key::name()`, and startup resolves the stored name
    /// with `Key::from_name()`. Anything bindable has to survive that trip.
    #[test]
    fn key_names_round_trip() {
        for &key in egui::Key::ALL {
            assert_eq!(egui::Key::from_name(key.name()), Some(key), "{key:?}");
        }
    }

    /// `fields_mut` is indexed with `pairs()` indices, so the two have to
    /// walk the bindings in the same order.
    #[test]
    fn field_order_matches_pair_order() {
        let mut keys = KeyBindings::default();
        for i in 0..BUTTON_NAMES.len() {
            *keys.fields_mut()[i] = format!("probe{i}");
        }
        for (i, (name, _)) in keys.pairs().iter().enumerate() {
            assert_eq!(*name, format!("probe{i}"));
        }
    }

    /// The four d-pad arms are one traced shape turned about the pad's
    /// centre, so they have to come out the same size and evenly spread.
    #[test]
    fn the_dpad_arms_are_quarter_turns_of_one_shape() {
        let view = View {
            origin: egui::Pos2::ZERO,
            scale: 1.0,
        };
        let centre = view.art(DPAD.0, DPAD.1);
        let radii = |turns| {
            let mut r: Vec<i32> = Art::Arm(turns)
                .points(view)
                .iter()
                .map(|p| (*p - centre).length().round() as i32)
                .collect();
            r.sort();
            r
        };
        // Same shape every time...
        for turns in 1..4u8 {
            assert_eq!(radii(turns), radii(0), "arm {turns} is not arm 0 turned");
        }
        // ...pointing a different way each time.
        let mut aim: Vec<i32> = (0..4)
            .map(|t| {
                let pts = Art::Arm(t).points(view);
                let v =
                    pts.iter().fold(egui::Vec2::ZERO, |a, p| a + (*p - centre)) / pts.len() as f32;
                (v.y.atan2(v.x).to_degrees().round() as i32 + 360) % 360
            })
            .collect();
        aim.sort();
        for pair in aim.windows(2) {
            assert_eq!(pair[1] - pair[0], 90, "arms aim at {aim:?}");
        }
    }
}
