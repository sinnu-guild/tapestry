//! `tapestry-probe` — check a camera from the command line before (or
//! instead of) using the app. Prints what the camera says about itself,
//! its streams, PTZ support, and listens for motion.
//!
//! Usage:
//!   tapestry-probe <ip> <camera-account-username> <password> [--wiggle] [--listen SECS]

use std::time::{Duration, Instant};
use tapestry_core::onvif::OnvifClient;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: tapestry-probe <ip> <username> <password> [--wiggle] [--listen SECS] [--port 2020]");
        std::process::exit(2);
    }
    let (host, user, pass) = (&args[1], &args[2], &args[3]);
    let wiggle = args.iter().any(|a| a == "--wiggle");
    let listen =
        args.iter().position(|a| a == "--listen").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(20u64);
    let port =
        args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(2020u16);

    let mut c = OnvifClient::new(host, port, user, pass);
    match c.sync_time() {
        Ok(off) => println!("clock      camera is {off:+}s vs this PC"),
        Err(e) => println!("clock      (couldn't read: {e})"),
    }
    let info = match c.connect() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("FAILED     {e}");
            std::process::exit(1);
        }
    };
    println!("camera     {} {} (firmware {})", info.manufacturer, info.model, info.firmware);
    println!("services   media={:?}\n           ptz={:?}\n           events={:?}", c.media_url, c.ptz_url, c.events_url);
    if let Ok(profiles) = c.profiles() {
        for p in profiles {
            let uri = c.stream_uri(&p.token).unwrap_or_else(|e| format!("({e})"));
            println!("profile    {:<10} {:<12} {}x{} {:<5} {}", p.token, p.name, p.width, p.height, p.encoding, uri);
        }
    }
    if c.has_ptz() {
        match c.presets() {
            Ok(p) if !p.is_empty() => println!("presets    {}", p.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")),
            Ok(_) => println!("presets    (none)"),
            Err(e) => println!("presets    ({e})"),
        }
        if wiggle {
            println!("ptz        wiggling right then left…");
            for dir in [0.5f32, -0.5] {
                match c.continuous_move(dir, 0.0) {
                    Ok(()) => {
                        std::thread::sleep(Duration::from_millis(700));
                        let _ = c.stop();
                        println!("           continuous move {dir:+}: ok");
                    }
                    Err(e) => {
                        println!("           continuous move failed ({e}); trying relative");
                        println!("           relative move: {:?}", c.relative_move(dir * 0.1, 0.0));
                    }
                }
            }
        }
    } else {
        println!("ptz        not available");
    }

    match c.subscribe(60) {
        Ok(sub) => {
            println!("events     subscribed at {} — wave at the camera ({listen}s)…", sub.address);
            let until = Instant::now() + Duration::from_secs(listen);
            while Instant::now() < until {
                match c.pull(&sub, 1) {
                    Ok(notes) => {
                        for n in notes {
                            let d = n.data.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
                            println!("event      {} [{}] {}", n.topic, n.operation, d);
                        }
                    }
                    // Tapo hangs up on idle polls; that just means "no news".
                    Err(tapestry_core::onvif::OnvifError::NoReply) => std::thread::sleep(Duration::from_millis(300)),
                    Err(e) => {
                        println!("event      pull failed: {e}");
                        break;
                    }
                }
            }
            let _ = c.unsubscribe(&sub);
        }
        Err(e) => println!("events     unavailable: {e}"),
    }
}
