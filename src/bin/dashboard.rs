use clap::Parser;
use eframe::{egui, egui::{Color32, Stroke, Shape, Pos2, Sense, Vec2}};
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
    smoothed_x: f32,
    smoothed_y: f32,
    trail: VecDeque<(f32, f32, Instant)>, // NEW: stores (x, y, timestamp)
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
}

impl App {
    fn new(state: Arc<Mutex<AppState>>, world_extent: f32) -> Self {
        Self { state, world_extent }
    }
}

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
                            let entry = guard.drones.entry(t.id).or_insert(DroneState {
                                x: t.x,
                                y: t.y,
                                z: t.z,
                                battery: t.battery,
                                status: t.status.clone(),
                                last_ts_ms: t.ts_ms,
                                last_seen: Instant::now(),
                                smoothed_x: t.x,                  // <-- add
                                smoothed_y: t.y,                  // <-- add
                                trail: VecDeque::with_capacity(64),
                            });
                            
                            entry.x = t.x;
                            entry.y = t.y;
                            entry.z = t.z;
                            entry.battery = t.battery;
                            entry.status = t.status;
                            entry.last_ts_ms = t.ts_ms;
                            entry.last_seen = Instant::now();

                            let alpha = 0.25_f32; // lower = smoother, higher = snappier
                            entry.smoothed_x = entry.smoothed_x + alpha * (t.x - entry.smoothed_x);
                            entry.smoothed_y = entry.smoothed_y + alpha * (t.y - entry.smoothed_y);
                            
                            // Record trail using smoothed coords
                            entry.trail.push_back((entry.smoothed_x, entry.smoothed_y, Instant::now()));
                            
                            // Prune trail
                            const TRAIL_MAX_POINTS: usize = 60;
                            const TRAIL_MAX_AGE: Duration = Duration::from_secs(2);

                            while entry.trail.len() > TRAIL_MAX_POINTS {
                                entry.trail.pop_front();
                            }
                            while let Some(&(_, _, when)) = entry.trail.front() {
                                if when.elapsed() > TRAIL_MAX_AGE { entry.trail.pop_front(); } else { break; }
                            }

                            guard.total_packets += 1;
                            guard.last_packet_at = Some(Instant::now());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available right now; small nap to avoid busy spin
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_e) => {
                    // Ignore other transient errors; short backoff
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Top bar: metrics
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            let guard = self.state.lock().unwrap();
            let drones = guard.drones.len();
            let total = guard.total_packets;
            let age_ms = guard
                .last_packet_at
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0);

            ui.horizontal_wrapped(|ui| {
                ui.heading("Telemetry Fusion Dashboard");
                ui.separator();
                ui.label(format!("Drones: {}", drones));
                ui.separator();
                ui.label(format!("Total packets: {}", total));
                ui.separator();
                ui.label(format!("Last packet age: {} ms", age_ms));
            });
        });

        // Center panel: canvas
        egui::CentralPanel::default().show(ctx, |ui| {
            let available = ui.available_size();
            let rect = ui.allocate_space(available).1;
            let painter = ui.painter_at(rect);

            // Draw a subtle background
            painter.rect_filled(rect, 8.0, Color32::from_gray(18));

            // Draw grid
            let grid_spacing = 40.0;
            let mut x = rect.left();
            while x <= rect.right() {
                painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                    (1.0, Color32::from_gray(40)));
                x += grid_spacing;
            }
            let mut y = rect.top();
            while y <= rect.bottom() {
                painter.line_segment([Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                    (1.0, Color32::from_gray(40)));
                y += grid_spacing;
            }

            // World -> screen transform
            let world = self.world_extent;
            let to_screen = |wx: f32, wy: f32| -> Pos2 {
                // Map [-world, world] -> rect
                let nx = (wx + world) / (2.0 * world);
                let ny = (wy + world) / (2.0 * world);
                Pos2::new(
                    rect.left() + nx * rect.width(),
                    rect.bottom() - ny * rect.height(), // flip y for screen coords
                )
            };

            // Draw drones
            let guard = self.state.lock().unwrap();
            for (id, d) in guard.drones.iter() {
                let p = to_screen(d.smoothed_x, d.smoothed_y);
                // Color derived from ID (stable, no Hsva API needed)
                let mut h = *id as u32;
                h ^= h >> 16;
                h = h.wrapping_mul(0x7feb_352d);
                h ^= h >> 15;
                h = h.wrapping_mul(0x846c_a68b);
                h ^= h >> 16;

                let r = (h & 0xFF) as u8;
                let g = ((h >> 8) & 0xFF) as u8;
                let b = ((h >> 16) & 0xFF) as u8;

                let age = d.last_seen.elapsed();
                let alpha = if age > std::time::Duration::from_secs(2) { 80 } else { 220 };
                let color = Color32::from_rgba_unmultiplied(r, g, b, alpha);

                // Fade if stale (> 2s)
                let age = d.last_seen.elapsed();
                let alpha = if age > Duration::from_secs(2) { 80 } else { 220 };
                let color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha);
                
                // Draw trail: fade older segments
                if d.trail.len() >= 2 {
                    let n = d.trail.len();
                    // Pre-map world→screen once
                    let mut pts: Vec<Pos2> = Vec::with_capacity(n);
                    for &(wx, wy, _) in d.trail.iter() {
                        pts.push(to_screen(wx, wy));
                    }
                
                    for w in 1..n {
                        let t = w as f32 / n as f32;          // 0..1 along the trail (older→newer)
                        let seg_alpha = (40.0 + 160.0 * t) as u8; // older segments are faint
                        let seg_color = Color32::from_rgba_unmultiplied(r, g, b, seg_alpha);
                        let stroke = Stroke::new(2.0, seg_color);
                        painter.add(Shape::line_segment([pts[w - 1], pts[w]], stroke));
                    }
                }
                

                painter.circle_filled(p, 10.0, color);

                let label = format!("#{id}  {:.0}%  {}", d.battery, d.status);
                painter.text(
                    p + Vec2::new(12.0, -12.0),
                    egui::Align2::LEFT_TOP,
                    label,
                    egui::FontId::proportional(14.0),
                    Color32::WHITE,
                );


            }

            // Interaction overlay (optional)
            let _resp = ui.interact(rect, ui.id().with("canvas"), Sense::click());
        });

        // ~30 FPS
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    let shared = Arc::new(Mutex::new(AppState::default()));
    spawn_udp_listener(args.bind.clone(), shared.clone());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 720.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("Telemetry Fusion Dashboard"),
        ..Default::default()
    };

    eframe::run_native(
        "Telemetry Fusion Dashboard",
        native_options,
        Box::new(move |_| Box::new(App::new(shared.clone(), args.world_extent))),
    )
}
