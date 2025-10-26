use clap::Parser;
use rand::Rng;
use serde::Serialize;
use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(name = "simulator", about = "Fake drone telemetry UDP broadcaster")]
struct Args {
    /// Number of drones to simulate
    #[arg(short, long, default_value_t = 8)]
    drones: u32,

    /// Target UDP address (host:port)
    #[arg(short, long, default_value = "127.0.0.1:5000")]
    target: String,

    /// Send interval in milliseconds
    #[arg(short = 'i', long, default_value_t = 200)]
    interval_ms: u64,

    /// Initial spread radius for x/y (world units)
    #[arg(long, default_value_t = 100.0)]
    spread: f32,
}

#[derive(Serialize, Clone)]
struct Telemetry {
    id: u32,
    x: f32,
    y: f32,
    z: f32,
    battery: f32,
    status: String,
    ts_ms: u128,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(&args.target)?;
    println!(
        "simulator: sending {} drones to {} every {} ms",
        args.drones, args.target, args.interval_ms
    );

    // Initialize random positions and battery
    let mut rng = rand::thread_rng();
    let mut drones: Vec<Telemetry> = (0..args.drones)
        .map(|id| Telemetry {
            id,
            x: rng.gen_range(-args.spread..args.spread),
            y: rng.gen_range(-args.spread..args.spread),
            z: rng.gen_range(0.0..50.0),
            battery: rng.gen_range(60.0..100.0),
            status: "OK".to_string(),
            ts_ms: now_ms(),
        })
        .collect();

    let interval = Duration::from_millis(args.interval_ms);

    loop {
        for d in &mut drones {
            // Simple random walk
            d.x += rng.gen_range(-1.5..1.5);
            d.y += rng.gen_range(-1.5..1.5);
            d.z = (d.z + rng.gen_range(-0.8..0.8)).clamp(0.0, 120.0);

            // Battery slowly decreases; add tiny noise
            d.battery = (d.battery - rng.gen_range(0.02..0.08)).max(0.0);

            // Status flips when low battery
            d.status = if d.battery < 15.0 { "LOW_BAT".into() } else { "OK".into() };

            d.ts_ms = now_ms();

            let payload = serde_json::to_vec(d).unwrap();
            let _ = sock.send(&payload)?;
        }

        thread::sleep(interval);
    }
}
