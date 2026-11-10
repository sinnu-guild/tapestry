//! A long-lived ONVIF connection to one camera: keeps it connected, listens
//! for motion, and runs pan/tilt commands on their own thread so steering
//! never waits behind an event long-poll.

use crate::config::Camera;
use crate::onvif::{DeviceInfo, OnvifClient, OnvifError, Preset};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum PtzCommand {
    /// Start (or change) moving. Velocities -1.0..=1.0, right/up positive.
    Move {
        pan: f32,
        tilt: f32,
    },
    Stop,
    /// One relative step (click-to-aim).
    Nudge {
        pan: f32,
        tilt: f32,
    },
    GotoPreset(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    Connecting,
    Connected {
        info: DeviceInfo,
        has_ptz: bool,
        presets: Vec<Preset>,
    },
    /// Connected, but the camera won't give us events (no motion alerts).
    EventsUnavailable(String),
    Offline(String),
    AuthFailed(String),
    Detection {
        kind: String,
        active: bool,
    },
    PtzError(String),
}

pub struct CameraLink {
    stop: Arc<AtomicBool>,
    ptz_tx: Sender<PtzCommand>,
}

/// How often to renew the event subscription, and its lifetime.
const SUB_LIFETIME: u32 = 120;
const PULL_WAIT: u32 = 5;

impl CameraLink {
    /// `emit` receives every event; it must be cheap and non-blocking.
    pub fn start(cam: Camera, emit: impl Fn(LinkEvent) + Send + Sync + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let client: Arc<Mutex<Option<OnvifClient>>> = Arc::new(Mutex::new(None));
        let emit: Arc<dyn Fn(LinkEvent) + Send + Sync> = Arc::new(emit);
        let (ptz_tx, ptz_rx) = mpsc::channel();

        {
            let (stop, client, emit) = (stop.clone(), client.clone(), emit.clone());
            let invert = cam.invert_ptz;
            thread::Builder::new()
                .name("tapestry-ptz".into())
                .spawn(move || ptz_loop(client, ptz_rx, emit, invert, stop))
                .expect("spawn ptz thread");
        }
        {
            let (stop, client, emit) = (stop.clone(), client.clone(), emit.clone());
            thread::Builder::new()
                .name("tapestry-link".into())
                .spawn(move || link_loop(cam, client, emit, stop))
                .expect("spawn link thread");
        }
        CameraLink { stop, ptz_tx }
    }

    pub fn ptz(&self, cmd: PtzCommand) {
        let _ = self.ptz_tx.send(cmd);
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.ptz_tx.send(PtzCommand::Stop);
    }
}

impl Drop for CameraLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn sleep_checked(stop: &AtomicBool, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
}

fn link_loop(
    cam: Camera,
    shared: Arc<Mutex<Option<OnvifClient>>>,
    emit: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff_idx = 0usize;
    const BACKOFF: [u64; 6] = [2, 4, 8, 15, 30, 60];

    while !stop.load(Ordering::Relaxed) {
        emit(LinkEvent::Connecting);
        let mut client = OnvifClient::new(&cam.host, cam.onvif_port, &cam.username, &cam.password);
        let info = match client.connect() {
            Ok(info) => info,
            Err(e) => {
                *shared.lock().unwrap() = None;
                emit(match &e {
                    OnvifError::Auth(_) => LinkEvent::AuthFailed(e.to_string()),
                    _ => LinkEvent::Offline(e.to_string()),
                });
                sleep_checked(&stop, Duration::from_secs(BACKOFF[backoff_idx.min(BACKOFF.len() - 1)]));
                backoff_idx += 1;
                continue;
            }
        };
        backoff_idx = 0;
        let presets = if client.has_ptz() { client.presets().unwrap_or_default() } else { Vec::new() };
        *shared.lock().unwrap() = Some(client.clone());
        emit(LinkEvent::Connected { info, has_ptz: client.has_ptz(), presets });

        // Event loop.
        match client.subscribe(SUB_LIFETIME) {
            Ok(sub) => {
                let mut renewed = Instant::now();
                let mut failures = 0;
                let mut wait = PULL_WAIT;
                let mut last_heard = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    match client.pull(&sub, wait) {
                        Ok(notes) => {
                            failures = 0;
                            last_heard = Instant::now();
                            for n in notes {
                                if let Some((kind, active)) = n.detection() {
                                    emit(LinkEvent::Detection { kind, active });
                                }
                            }
                        }
                        Err(OnvifError::NoReply) => {
                            // Tapo hangs up on a long-poll with nothing to report
                            // instead of answering "no news". That's normal, not
                            // an outage: poll in short steps from here on.
                            failures = 0;
                            wait = 1;
                            sleep_checked(&stop, Duration::from_millis(300));
                            // Make sure the camera really is still there now and then.
                            if last_heard.elapsed() > Duration::from_secs(45) {
                                match client.device_information() {
                                    Ok(_) => last_heard = Instant::now(),
                                    Err(e) => {
                                        emit(LinkEvent::Offline(e.to_string()));
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            failures += 1;
                            if failures >= 3 {
                                emit(LinkEvent::Offline(e.to_string()));
                                break;
                            }
                            sleep_checked(&stop, Duration::from_secs(1));
                        }
                    }
                    if renewed.elapsed() > Duration::from_secs((SUB_LIFETIME / 2) as u64) {
                        if client.renew(&sub, SUB_LIFETIME).is_err() {
                            // Some cameras don't do Renew; quietly re-subscribe instead.
                            break;
                        }
                        renewed = Instant::now();
                    }
                }
                let _ = client.unsubscribe(&sub);
            }
            Err(e) => {
                emit(LinkEvent::EventsUnavailable(e.to_string()));
                // Still keep an eye on the camera so the status stays honest.
                while !stop.load(Ordering::Relaxed) {
                    sleep_checked(&stop, Duration::from_secs(30));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = client.device_information() {
                        emit(LinkEvent::Offline(e.to_string()));
                        break;
                    }
                }
            }
        }
        if !stop.load(Ordering::Relaxed) {
            sleep_checked(&stop, Duration::from_secs(2));
        }
    }
}

fn ptz_loop(
    shared: Arc<Mutex<Option<OnvifClient>>>,
    rx: Receiver<PtzCommand>,
    emit: Arc<dyn Fn(LinkEvent) + Send + Sync>,
    invert: bool,
    stop: Arc<AtomicBool>,
) {
    // Tapo supports ContinuousMove; if a camera refuses it, fall back to
    // repeated small RelativeMove steps for as long as the key is held.
    const STEP: f32 = 0.05;
    let mut relative_mode = false;
    let mut held: Option<(f32, f32)> = None; // direction already sign-adjusted
    let mut moving = false;
    let sign = if invert { -1.0 } else { 1.0 };
    let report = |r: Result<(), OnvifError>| {
        if let Err(e) = r {
            emit(LinkEvent::PtzError(e.to_string()));
        }
    };

    loop {
        if stop.load(Ordering::Relaxed) {
            if moving {
                if let Some(c) = shared.lock().unwrap().clone() {
                    let _ = c.stop();
                }
            }
            return;
        }
        let wait = if relative_mode && held.is_some() { 350 } else { 500 };
        let mut cmds = Vec::new();
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(c) => cmds.push(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        let cmds = coalesce(cmds);

        let Some(client) = shared.lock().unwrap().clone() else {
            if cmds.iter().any(|c| *c != PtzCommand::Stop) {
                emit(LinkEvent::PtzError("camera not connected yet".into()));
            }
            held = None;
            moving = false;
            continue;
        };

        if cmds.is_empty() {
            if relative_mode {
                if let Some((p, t)) = held {
                    report(client.relative_move(p * STEP, t * STEP));
                }
            }
            continue;
        }

        for c in cmds {
            match c {
                PtzCommand::Move { pan, tilt } => {
                    let (pan, tilt) = (pan * sign, tilt * sign);
                    if !relative_mode {
                        match client.continuous_move(pan, tilt) {
                            Ok(()) => moving = true,
                            Err(OnvifError::Fault(_)) => {
                                relative_mode = true;
                                held = Some((pan, tilt));
                                report(client.relative_move(pan * STEP, tilt * STEP));
                            }
                            Err(e) => report(Err(e)),
                        }
                    } else {
                        held = Some((pan, tilt));
                        report(client.relative_move(pan * STEP, tilt * STEP));
                    }
                }
                PtzCommand::Stop => {
                    held = None;
                    if moving {
                        report(client.stop());
                        moving = false;
                    }
                }
                PtzCommand::Nudge { pan, tilt } => report(client.relative_move(pan * sign, tilt * sign)),
                PtzCommand::GotoPreset(tok) => report(client.goto_preset(&tok)),
            }
        }
    }
}

/// Collapse runs of Move/Stop to their last element (only the latest
/// direction matters); keep everything else in order.
fn coalesce(cmds: Vec<PtzCommand>) -> Vec<PtzCommand> {
    let mut out: Vec<PtzCommand> = Vec::new();
    for c in cmds {
        let steer = matches!(c, PtzCommand::Move { .. } | PtzCommand::Stop);
        if steer && matches!(out.last(), Some(PtzCommand::Move { .. } | PtzCommand::Stop)) {
            *out.last_mut().unwrap() = c;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_steering() {
        let m = |p| PtzCommand::Move { pan: p, tilt: 0.0 };
        let out = coalesce(vec![m(0.1), m(0.2), PtzCommand::Stop, PtzCommand::GotoPreset("1".into()), m(0.3), m(0.4)]);
        assert_eq!(out, vec![PtzCommand::Stop, PtzCommand::GotoPreset("1".into()), m(0.4)]);
    }
}
