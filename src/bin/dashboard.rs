use clap::Parser;
use eframe::{
    egui,
    egui::{
        Color32, FontId, Id, Label, Margin, Pos2, Rect, RichText, Rounding, Sense, Shape, Stroke,
        TextStyle, Vec2,
    },
};
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    net::UdpSocket,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(name = "dashboard", about = "Telemetry Fusion Dashboard (UDP listener + egui)")]
struct Args {
    /// UDP bind address for listening
    #[arg(short, long, default_value = "127.0.0.1:5000")]
    bind: String,

    /// World coordinate extent (+/- this many units on both axes)
    #[arg(long, default_value_t = 120.0)]
    world_extent: f32,
}

#[derive(Debug, Deserialize, Clone)]
struct Telemetry {
    id: u32,
    x: f32,
    y: f32,
    z: f32,
    battery: f32,
    status: String,
    ts_ms: u128,
}

#[derive(Debug, Clone)]
struct DroneState {
    x: f32,
    y: f32,
    z: f32,
    battery: f32,
    status: String,
    last_ts_ms: u128,
    last_seen: Instant,

    // Visual smoothing / trails
    smoothed_x: f32,
    smoothed_y: f32,
    // (x, y, when recorded)
    trail: VecDeque<(f32, f32, Instant)>,
}

#[derive(Default)]
struct AppState {
    drones: HashMap<u32, DroneState>,
    total_packets: u64,
    last_packet_at: Option<Instant>,
}

struct App {
    state: Arc<Mutex<AppState>>,
    world_extent: f32,
    show_trails: bool,
    styled_once: bool,
    selected: Option<u32>,

    // Anchored HUD overlay
    hud_open: bool,   // desired (target) state
    hud_t: f32,       // animation progress 0..1
    hud_expanded: bool,
}

impl App {
    fn new(state: Arc<Mutex<AppState>>, world_extent: f32) -> Self {
        Self {
            state,
            world_extent,
            show_trails: true,
            styled_once: false,
            selected: None,
            hud_open: false,
            hud_t: 0.0,
            hud_expanded: false,
        }
    }
}

/* ------------------------------ UDP listener ------------------------------ */

fn spawn_udp_listener(bind: String, shared: Arc<Mutex<AppState>>) {
    thread::spawn(move || {
        let socket = UdpSocket::bind(&bind).expect("failed to bind UDP socket");
        socket
            .set_nonblocking(true)
            .expect("failed to set non-blocking");

        println!("dashboard: listening on {}", bind);

        let mut buf = [0u8; 2048];

        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, _addr)) => {
                    if let Ok(msg) = std::str::from_utf8(&buf[..n]) {
                        if let Ok(t) = serde_json::from_str::<Telemetry>(msg) {
                            let mut guard = shared.lock().unwrap();

                            // Insert or get the drone
                            let entry = guard.drones.entry(t.id).or_insert(DroneState {
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                battery: t.battery,
                                status: t.status.clone(),
                                last_ts_ms: t.ts_ms,
                                last_seen: Instant::now(),
                                smoothed_x: t.x,
                                smoothed_y: t.y,
                                trail: VecDeque::with_capacity(128),
                            });

                            // Update latest raw values
                            entry.x = t.x;
                            entry.y = t.y;
                            entry.z = t.z;
                            entry.battery = t.battery;
                            entry.status = t.status;
                            entry.last_ts_ms = t.ts_ms;
                            entry.last_seen = Instant::now();

                            // EMA smoothing for visual position
                            let alpha = 0.25_f32; // lower = smoother, higher = snappier
                            entry.smoothed_x =
                                entry.smoothed_x + alpha * (entry.x - entry.smoothed_x);
                            entry.smoothed_y =
                                entry.smoothed_y + alpha * (entry.y - entry.smoothed_y);

                            // Record trail using smoothed coords
                            entry
                                .trail
                                .push_back((entry.smoothed_x, entry.smoothed_y, Instant::now()));

                            // Prune trail by size and age (keep a long history)
                            const TRAIL_MAX_POINTS: usize = 600;
                            const TRAIL_MAX_AGE: Duration = Duration::from_secs(20);
                            while entry.trail.len() > TRAIL_MAX_POINTS {
                                entry.trail.pop_front();
                            }
                            while let Some(&(_, _, when)) = entry.trail.front() {
                                if when.elapsed() > TRAIL_MAX_AGE {
                                    entry.trail.pop_front();
                                } else {
                                    break;
                                }
                            }

                            guard.total_packets += 1;
                            guard.last_packet_at = Some(Instant::now());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data right now; avoid busy spin
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_e) => {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    });
}

/* ----------------------------- UI helpers ----------------------------- */

fn glass_card(ui: &mut egui::Ui, size: Vec2, body: impl FnOnce(&mut egui::Ui, Rect)) {
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
        .stroke(Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 26),
        ))
        .rounding(Rounding::same(14.0))
        .inner_margin(Margin::symmetric(12.0, 12.0))
        .show(ui, |ui| {
            let (rect, _resp) = ui.allocate_exact_size(size, Sense::hover());
            body(ui, rect);
        });
}

fn draw_ring_gauge(
    painter: &egui::Painter,
    rect: Rect,
    value_01: f32,
    ring_color: Color32,
    bg_color: Color32,
    text: &str,
    subtext: &str,
) {
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.36;
    let thickness = 10.0;

    // background ring
    painter.circle_stroke(center, radius, Stroke::new(thickness, bg_color));

    // arc value
    let clamped = value_01.clamp(0.0, 1.0);
    let start = -std::f32::consts::FRAC_PI_2; // top
    let end = start + clamped * std::f32::consts::TAU;
    let n = 64;
    let mut pts = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let a = start + t * (end - start);
        pts.push(Pos2::new(center.x + a.cos() * radius, center.y + a.sin() * radius));
    }
    painter.add(Shape::line(pts, Stroke::new(thickness, ring_color)));

    // labels
    painter.text(
        center + Vec2::new(0.0, -8.0),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(22.0),
        Color32::from_rgb(235, 240, 248),
    );
    painter.text(
        center + Vec2::new(0.0, 16.0),
        egui::Align2::CENTER_CENTER,
        subtext,
        FontId::proportional(13.0),
        Color32::from_rgb(200, 208, 220),
    );
}

fn status_badge(ui: &mut egui::Ui, status: &str) {
    let (col, text_col) = match status {
        s if s.eq_ignore_ascii_case("ok") => (
            Color32::from_rgba_unmultiplied(38, 201, 97, 40),
            Color32::from_rgb(171, 255, 202),
        ),
        s if s.eq_ignore_ascii_case("low_bat") => (
            Color32::from_rgba_unmultiplied(250, 70, 70, 40),
            Color32::from_rgb(255, 208, 208),
        ),
        _ => (
            Color32::from_rgba_unmultiplied(140, 150, 170, 40),
            Color32::from_rgb(220, 225, 235),
        ),
    };

    egui::Frame::none()
        .fill(col)
        .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 26)))
        .rounding(10.0)
        .inner_margin(Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.add(
                Label::new(
                    RichText::new(status.to_uppercase())
                        .monospace()
                        .color(text_col)
                        .size(13.0),
                )
                .selectable(false),
            );
        });
}

fn numeric_tile_wh(ui: &mut egui::Ui, title: &str, value: &str, w: f32, h: f32) {
    glass_card(ui, egui::vec2(w, h), |ui, rect| {
        let painter = ui.painter_at(rect);
        painter.text(
            rect.left_top() + egui::vec2(4.0, 0.0),
            egui::Align2::LEFT_TOP,
            title,
            FontId::proportional(13.0),
            Color32::from_rgb(190, 200, 215),
        );
        painter.text(
            rect.center() + egui::vec2(0.0, 6.0),
            egui::Align2::CENTER_CENTER,
            value,
            FontId::monospace(20.0),
            Color32::from_rgb(235, 240, 248),
        );
    });
}

/* ------------------------------- App impl ------------------------------- */

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // One-time global style
        if !self.styled_once {
            let mut v = egui::Visuals::dark();
            v.panel_fill = Color32::from_rgb(12, 13, 16);
            v.window_fill = Color32::from_rgb(18, 20, 24);

            v.widgets.inactive.bg_fill = Color32::from_rgb(24, 26, 31);
            v.widgets.hovered.bg_fill = Color32::from_rgb(32, 35, 42);
            v.widgets.active.bg_fill = Color32::from_rgb(40, 44, 52);

            v.widgets.inactive.rounding = 12.0.into();
            v.widgets.hovered.rounding = 12.0.into();
            v.widgets.active.rounding = 12.0.into();

            v.window_rounding = 14.0.into();
            v.menu_rounding = 12.0.into();
            ctx.set_visuals(v);

            let mut style = (*ctx.style()).clone();
            style.text_styles = [
                (TextStyle::Heading, FontId::proportional(24.0)),
                (TextStyle::Body, FontId::proportional(16.5)),
                (TextStyle::Button, FontId::proportional(16.0)),
                (TextStyle::Monospace, FontId::monospace(15.0)),
                (TextStyle::Small, FontId::proportional(12.5)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(12.0, 10.0);
            style.spacing.button_padding = egui::vec2(14.0, 9.0);
            ctx.set_style(style);

            self.styled_once = true;
        }

        /* ------------------------ top bar: chips ------------------------ */
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            let (drones, total, age_ms) = {
                let guard = self.state.lock().unwrap();
                (
                    guard.drones.len(),
                    guard.total_packets,
                    guard
                        .last_packet_at
                        .map(|t| t.elapsed().as_millis())
                        .unwrap_or(0),
                )
            };

            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.heading("Telemetry Fusion Dashboard");
                ui.add_space(12.0);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut chip_fixed = |ui: &mut egui::Ui, text: String, min_w: f32| {
                        egui::Frame::none()
                            .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
                            .stroke(Stroke::new(
                                1.0,
                                Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                            ))
                            .rounding(10.0)
                            .inner_margin(Margin::symmetric(12.0, 8.0))
                            .show(ui, |ui| {
                                ui.set_min_width(min_w);
                                ui.label(RichText::new(text).monospace());
                            });
                    };

                    let last_secs = (age_ms as f32) / 1000.0;
                    let last_text = if age_ms < 1000 {
                        format!("Last pkt: {:>4} ms", age_ms)
                    } else {
                        format!("Last pkt: {:>5.1}s", last_secs)
                    };

                    chip_fixed(ui, last_text, 160.0);
                    chip_fixed(ui, format!("Packets: {total}"), 140.0);
                    chip_fixed(ui, format!("Drones: {drones}"), 120.0);

                    egui::Frame::none()
                        .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
                        .stroke(Stroke::new(
                            1.0,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                        ))
                        .rounding(10.0)
                        .inner_margin(Margin::symmetric(12.0, 6.0))
                        .show(ui, |ui| {
                            ui.toggle_value(&mut self.show_trails, "Trails");
                        });
                });
            });
        });

        /* ------------------------ center panel: map ----------------------- */
        egui::CentralPanel::default().show(ctx, |ui| {
            let available = ui.available_size();
            let rect = ui.allocate_space(available).1;
            let painter = ui.painter_at(rect);

            // deep base + soft inner border/vignette
            painter.rect_filled(rect, 14.0, Color32::from_rgb(10, 11, 14));
            painter.rect_stroke(
                rect.shrink(30.0),
                14.0,
                Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 255, 255, 18)),
            );
            painter.rect_filled(
                rect.shrink(4.0),
                14.0,
                Color32::from_rgba_unmultiplied(255, 255, 255, 4),
            );

            // soft grid
            let grid_spacing = 56.0;
            let grid_col = Color32::from_rgba_unmultiplied(140, 150, 170, 26);
            let mut gx = rect.left().ceil();
            while gx <= rect.right() {
                painter.line_segment(
                    [Pos2::new(gx, rect.top()), Pos2::new(gx, rect.bottom())],
                    (1.0, grid_col),
                );
                gx += grid_spacing;
            }
            let mut gy = rect.top().ceil();
            while gy <= rect.bottom() {
                painter.line_segment(
                    [Pos2::new(rect.left(), gy), Pos2::new(rect.right(), gy)],
                    (1.0, grid_col),
                );
                gy += grid_spacing;
            }

            // World -> screen transform
            let world = self.world_extent;
            let to_screen = |wx: f32, wy: f32| -> Pos2 {
                let nx = (wx + world) / (2.0 * world);
                let ny = (wy + world) / (2.0 * world);
                Pos2::new(
                    rect.left() + nx * rect.width(),
                    rect.bottom() - ny * rect.height(),
                )
            };

            // Snapshot the state so we don't hold the mutex while painting
            let snapshot: Vec<(u32, DroneState)> = {
                let guard = self.state.lock().unwrap();
                guard.drones.iter().map(|(k, v)| (*k, v.clone())).collect()
            };

            let mut screen_positions: Vec<(u32, Pos2, Color32)> = Vec::with_capacity(snapshot.len());

            for (id, d) in snapshot.iter() {
                // Stable color derived from ID
                let mut h = *id as u32;
                h ^= h >> 16;
                h = h.wrapping_mul(0x7feb_352d);
                h ^= h >> 15;
                h = h.wrapping_mul(0x846c_a68b);
                h ^= h >> 16;
                let r = (h & 0xFF) as u8;
                let g = ((h >> 8) & 0xFF) as u8;
                let b = ((h >> 16) & 0xFF) as u8;

                let p = to_screen(d.smoothed_x, d.smoothed_y);

                // Fade whole drone if no packet for >2s
                let age = d.last_seen.elapsed();
                let dot_alpha = if age > Duration::from_secs(2) { 80 } else { 220 };
                let dot_color = Color32::from_rgba_unmultiplied(r, g, b, dot_alpha);

                screen_positions.push((*id, p, dot_color));

                // ---- Trail ----
                if self.show_trails && d.trail.len() >= 2 {
                    let mut pts: Vec<(Pos2, Instant)> = Vec::with_capacity(d.trail.len());
                    for &(wx, wy, when) in d.trail.iter() {
                        pts.push((to_screen(wx, wy), when));
                    }

                    const FADE_START: Duration = Duration::from_secs(10);
                    const FADE_END: Duration = Duration::from_secs(20);
                    const ALPHA_MIN: u8 = 30;
                    const ALPHA_MAX: u8 = 240;

                    for w in 1..pts.len() {
                        let (p1, _t1) = pts[w - 1];
                        let (p2, t2) = pts[w];

                        let age = t2.elapsed();
                        let alpha = if age <= FADE_START {
                            ALPHA_MAX
                        } else if age >= FADE_END {
                            ALPHA_MIN
                        } else {
                            let total = (FADE_END - FADE_START).as_secs_f32();
                            let over = (age - FADE_START).as_secs_f32();
                            let t = (over / total).clamp(0.0, 1.0).powf(0.7);
                            let a =
                                (ALPHA_MAX as f32) + t * ((ALPHA_MIN as f32) - (ALPHA_MAX as f32));
                            a as u8
                        };

                        let nr = (r as u16 + 30).min(255) as u8;
                        let ng = (g as u16 + 30).min(255) as u8;
                        let nb = (b as u16 + 30).min(255) as u8;

                        let stroke =
                            Stroke::new(1.4, Color32::from_rgba_unmultiplied(nr, ng, nb, alpha));
                        painter.add(Shape::line_segment([p1, p2], stroke));
                    }
                }

                // Glow + dot + outline (highlight if selected)
                let selected = self.selected == Some(*id);
                let halo_alpha = if selected { 100 } else { 60 };
                let halo = Color32::from_rgba_unmultiplied(r, g, b, halo_alpha);
                let dot_radius = if selected { 12.0 } else { 10.0 };
                painter.circle_filled(p + Vec2::new(0.0, 1.0), 18.0, halo);
                painter.circle_filled(p, dot_radius, dot_color);
                painter.circle_stroke(
                    p,
                    dot_radius,
                    if selected {
                        Stroke::new(2.4, Color32::from_rgb(255, 255, 255))
                    } else {
                        Stroke::new(1.6, Color32::from_rgba_unmultiplied(255, 255, 255, 36))
                    },
                );

                // Label pill
                let label_text = format!("#{id}  {:.0}%  {}", d.battery, d.status);
                let label_pos = p + Vec2::new(14.0, -16.0);
                let label_bg = Color32::from_rgba_unmultiplied(0, 0, 0, 120);
                let label_stroke =
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 30));
                let pill = Rect::from_min_size(label_pos, Vec2::new(170.0, 24.0));
                painter.rect_filled(pill, 8.0, label_bg);
                painter.rect_stroke(pill, 8.0, label_stroke);
                painter.text(
                    label_pos + Vec2::new(8.0, 5.0),
                    egui::Align2::LEFT_TOP,
                    label_text,
                    FontId::proportional(14.0),
                    Color32::from_rgb(230, 235, 245),
                );
            }

            // Click handling (hit-test near a drone)
            let resp = ui.interact(rect, Id::new("canvas"), Sense::click());
            if resp.clicked() {
                if let Some(click_pos) = resp.interact_pointer_pos() {
                    let mut best: Option<(u32, f32)> = None;
                    let threshold_sq = 20.0 * 20.0;
                    for (id, p, _color) in &screen_positions {
                        let d2 = (p.x - click_pos.x).powi(2) + (p.y - click_pos.y).powi(2);
                        if d2 <= threshold_sq {
                            match best {
                                None => best = Some((*id, d2)),
                                Some((_bid, bd2)) if d2 < bd2 => best = Some((*id, d2)),
                                _ => {}
                            }
                        }
                    }
                    self.selected = best.map(|(id, _)| id);
                } else {
                    self.selected = None;
                }
            }

            // ===== Anchored HUD overlay next to the selected drone =====
            self.hud_open = self.selected.is_some();

            if let Some(sel) = self.selected {
                if let Some((_, anchor, _)) = screen_positions.iter().find(|(id, _, _)| *id == sel)
                {
                    // Animate t toward target (ease)
                    let target = if self.hud_open { 1.0 } else { 0.0 };
                    self.hud_t += (target - self.hud_t) * 0.18;

                    // Card metrics
                    let card_w = 260.0;
                    let card_h = 200.0;

                    // Prefer placing to the right/top of the drone, but clamp inside rect
                    let mut pos = *anchor + Vec2::new(18.0, -card_h - 12.0);
                    pos.x = pos.x.clamp(rect.left() + 12.0, rect.right() - card_w - 12.0);
                    pos.y = pos.y.clamp(rect.top() + 12.0, rect.bottom() - card_h - 12.0);

                    // Subtle slide + fade in
                    let slide_px = (1.0 - self.hud_t) * 16.0; // from the right
                    let opacity = ((self.hud_t * 255.0) as i32).clamp(0, 255) as u8;
                    let bg =
                        Color32::from_rgba_unmultiplied(24, 26, 31, (opacity as f32 * 0.92) as u8);
                    let stroke =
                        Color32::from_rgba_unmultiplied(255, 255, 255, (opacity as f32 * 0.22) as u8);

                    egui::Area::new(Id::new("anchored_hud"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(Pos2::new(pos.x + slide_px, pos.y))
                        .interactable(true)
                        .show(ctx, |ui| {
                            egui::Frame::none()
                                .fill(bg)
                                .stroke(Stroke::new(1.0, stroke))
                                .rounding(Rounding::same(14.0))
                                .inner_margin(Margin::symmetric(12.0, 10.0))
                                .show(ui, |ui| {
                                    ui.set_min_size(Vec2::new(card_w, card_h));
                                    ui.set_max_size(Vec2::new(card_w, card_h));

                                    // Snapshot drone
                                    let snap = {
                                        let guard = self.state.lock().unwrap();
                                        guard.drones.get(&sel).cloned()
                                    };

                                    if let Some(d) = snap {
                                        // Header
                                        ui.horizontal(|ui| {
                                            ui.monospace(format!("#{:04}", sel));
                                            ui.add_space(8.0);
                                            status_badge(ui, &d.status);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui.button("×").clicked() {
                                                        self.selected = None;
                                                        self.hud_open = false;
                                                    }
                                                },
                                            );
                                        });
                                        ui.add_space(6.0);

                                        // Two mini rings (battery, last packet)
                                        let ring_h = 110.0;
                                        let ring_w = 110.0;
                                        ui.horizontal(|ui| {
                                            glass_card(ui, Vec2::new(ring_w, ring_h), |ui, rect| {
                                                let p = ui.painter_at(rect);
                                                let v = (d.battery / 100.0).clamp(0.0, 1.0);
                                                let col = if d.battery < 15.0 {
                                                    Color32::from_rgb(255, 110, 110)
                                                } else {
                                                    Color32::from_rgb(120, 220, 160)
                                                };
                                                draw_ring_gauge(
                                                    &p,
                                                    rect,
                                                    v,
                                                    col,
                                                    Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 26,
                                                    ),
                                                    &format!("{:>3.0}%", d.battery),
                                                    "Battery",
                                                );
                                            });
                                            let age = d.last_seen.elapsed();
                                            glass_card(ui, Vec2::new(ring_w, ring_h), |ui, rect| {
                                                let p = ui.painter_at(rect);
                                                let secs = age.as_secs_f32();
                                                let freshness = (1.0 - (secs / 5.0)).clamp(0.0, 1.0);
                                                let col = if secs > 2.0 {
                                                    Color32::from_rgb(255, 200, 120)
                                                } else {
                                                    Color32::from_rgb(140, 190, 255)
                                                };
                                                draw_ring_gauge(
                                                    &p,
                                                    rect,
                                                    freshness,
                                                    col,
                                                    Color32::from_rgba_unmultiplied(
                                                        255, 255, 255, 26),
                                                    &if age < Duration::from_secs(1) {
                                                        format!("{} ms", age.as_millis())
                                                    } else {
                                                        format!("{:.1} s", secs)
                                                    },
                                                    "Last pkt",
                                                );
                                            });
                                        });

                                        ui.add_space(6.0);

                                        // Compact stats row
                                        egui::Frame::none()
                                            .fill(Color32::from_rgba_unmultiplied(
                                                255, 255, 255, 8,
                                            ))
                                            .stroke(Stroke::new(
                                                1.0,
                                                Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                                            ))
                                            .rounding(10.0)
                                            .inner_margin(Margin::symmetric(10.0, 8.0))
                                            .show(ui, |ui| {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label(
                                                        RichText::new(format!("x:{:>6.2}", d.x))
                                                            .monospace(),
                                                    );
                                                    ui.separator();
                                                    ui.label(
                                                        RichText::new(format!("y:{:>6.2}", d.y))
                                                            .monospace(),
                                                    );
                                                    ui.separator();
                                                    ui.label(
                                                        RichText::new(format!(
                                                            "z:{:>5.1} m",
                                                            d.z
                                                        ))
                                                        .monospace(),
                                                    );
                                                });
                                            });

                                        ui.add_space(6.0);

                                        // Actions
                                        ui.horizontal(|ui| {
                                            if ui.button("Expand").clicked() {
                                                self.hud_expanded = true;
                                            }
                                            if ui.button("Center on drone").clicked() {
                                                // Hook: when you add pan/zoom camera, jump to this drone
                                            }
                                        });
                                    } else {
                                        ui.label("No recent packets.");
                                    }
                                });
                        });

                    // Optional: click outside to dismiss HUD (best-effort)
                    if self.hud_t > 0.95 && ui.input(|i| i.pointer.any_pressed()) {
                        // The HUD Area consumes clicks inside it; background clicks fall here.
                        // Uncomment if you want outside-click to close:
                        // self.hud_open = false;
                        // self.selected = None;
                    }
                }
            }

            // Keep animation smooth
            ctx.request_repaint_after(Duration::from_millis(33));
        });

        // ===== Optional centered sheet when "Expand" is pressed =====
        if self.hud_expanded {
            let mut open = self.hud_expanded;
            egui::Window::new("")
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .pivot(egui::Align2::CENTER_CENTER)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .frame(
                    egui::Frame::none()
                        .fill(Color32::from_rgba_unmultiplied(20, 22, 26, 240))
                        .stroke(Stroke::new(
                            1.0,
                            Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                        ))
                        .rounding(14.0)
                        .inner_margin(Margin::symmetric(16.0, 14.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Drone Details");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.button("Close").clicked() {
                                    open = false;
                                }
                            },
                        );
                    });
                    ui.separator();

                    if let Some(id) = self.selected {
                        let snap = {
                            let guard = self.state.lock().unwrap();
                            guard.drones.get(&id).cloned()
                        };
                        if let Some(d) = snap {
                            // Bigger tiles for the modal
                            let gap = 10.0;
                            let ring_h = 170.0;
                            let ring_w = 170.0;

                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                glass_card(ui, Vec2::new(ring_w, ring_h), |ui, rect| {
                                    let p = ui.painter_at(rect);
                                    let v = (d.battery / 100.0).clamp(0.0, 1.0);
                                    let col = if d.battery < 15.0 {
                                        Color32::from_rgb(255, 110, 110)
                                    } else {
                                        Color32::from_rgb(120, 220, 160)
                                    };
                                    draw_ring_gauge(
                                        &p,
                                        rect,
                                        v,
                                        col,
                                        Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                                        &format!("{:>3.0}%", d.battery),
                                        "Battery",
                                    );
                                });
                                ui.add_space(gap);
                                let age = d.last_seen.elapsed();
                                glass_card(ui, Vec2::new(ring_w, ring_h), |ui, rect| {
                                    let p = ui.painter_at(rect);
                                    let secs = age.as_secs_f32();
                                    let freshness = (1.0 - (secs / 5.0)).clamp(0.0, 1.0);
                                    let col = if secs > 2.0 {
                                        Color32::from_rgb(255, 200, 120)
                                    } else {
                                        Color32::from_rgb(140, 190, 255)
                                    };
                                    draw_ring_gauge(
                                        &p,
                                        rect,
                                        freshness,
                                        col,
                                        Color32::from_rgba_unmultiplied(255, 255, 255, 26),
                                        &if age < Duration::from_secs(1) {
                                            format!("{} ms", age.as_millis())
                                        } else {
                                            format!("{:.1} s", secs)
                                        },
                                        "Last pkt",
                                    );
                                });
                            });

                            ui.add_space(8.0);

                            // Numeric tiles row
                            ui.horizontal(|ui| {
                                numeric_tile_wh(ui, "Altitude", &format!("{:>6.1} m", d.z), 160.0, 84.0);
                                ui.add_space(8.0);
                                let speed = if d.trail.len() >= 2 {
                                    let (x2, y2, t2) = d.trail.back().copied().unwrap();
                                    let (x1, y1, t1) = d.trail.get(d.trail.len() - 2).copied().unwrap();
                                    let dt = (t2 - t1).as_secs_f32().max(1e-3);
                                    let dx = x2 - x1;
                                    let dy = y2 - y1;
                                    (dx * dx + dy * dy).sqrt() / dt
                                } else {
                                    0.0
                                };
                                numeric_tile_wh(ui, "Speed", &format!("{:>6.2} u/s", speed), 160.0, 84.0);
                            });

                            ui.add_space(8.0);

                            // Position card
                            glass_card(ui, Vec2::new(360.0, 96.0), |ui, rect| {
                                let p = ui.painter_at(rect);
                                p.text(
                                    rect.left_top(),
                                    egui::Align2::LEFT_TOP,
                                    "Position",
                                    FontId::proportional(13.0),
                                    Color32::from_rgb(190, 200, 215),
                                );
                                let txt =
                                    format!("x = {:>7.2}\ny = {:>7.2}\nz = {:>7.2}", d.x, d.y, d.z);
                                p.text(
                                    rect.left_top() + Vec2::new(0.0, 20.0),
                                    egui::Align2::LEFT_TOP,
                                    txt,
                                    FontId::monospace(16.0),
                                    Color32::from_rgb(235, 240, 248),
                                );
                            });
                        } else {
                            ui.label("No recent packets.");
                        }
                    } else {
                        ui.label("Click a drone on the map to open details.");
                    }
                });
            self.hud_expanded = open;
        }
    }
}

/* ------------------------------- main ------------------------------- */

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    let shared = Arc::new(Mutex::new(AppState::default()));
    spawn_udp_listener(args.bind.clone(), shared.clone());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 730.0])
            .with_min_inner_size([920.0, 560.0])
            .with_title("Telemetry Fusion Dashboard"),
        ..Default::default()
    };

    eframe::run_native(
        "Telemetry Fusion Dashboard",
        native_options,
        Box::new(move |_| Box::new(App::new(shared.clone(), args.world_extent))),
    )
}
