//! A fake Tapo camera speaking ONVIF over HTTP, to exercise the real client
//! code end to end: WS-Security digests, clock offset, service discovery
//! (with a deliberately wrong advertised IP, like some firmwares do), PTZ,
//! presets, and motion events via pull-point subscription.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tapestry_core::crypto::base64_decode;
use tapestry_core::onvif::{password_digest, OnvifClient, OnvifError};
use tapestry_core::xml;

pub struct MockCamera {
    pub port: u16,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub reject_continuous: Arc<AtomicBool>,
    /// Motion values to hand out on successive PullMessages (then empty).
    pub motion_script: Arc<Mutex<Vec<bool>>>,
    pub pulls: Arc<AtomicUsize>,
    /// Behave like real Tapo firmware: hang up on idle long-polls.
    pub drop_idle_pulls: Arc<AtomicBool>,
}

const USER: &str = "tapestry";
const PASS: &str = "s3cret&<pw>";
/// The mock's clock runs 90 s ahead, to prove clock sync works.
const CLOCK_SKEW: i64 = 90;

impl MockCamera {
    pub fn start() -> MockCamera {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cam = MockCamera {
            port,
            calls: Arc::new(Mutex::new(Vec::new())),
            reject_continuous: Arc::new(AtomicBool::new(false)),
            motion_script: Arc::new(Mutex::new(Vec::new())),
            pulls: Arc::new(AtomicUsize::new(0)),
            drop_idle_pulls: Arc::new(AtomicBool::new(false)),
        };
        let (calls, rej, script, pulls) =
            (cam.calls.clone(), cam.reject_continuous.clone(), cam.motion_script.clone(), cam.pulls.clone());
        let drop_idle = cam.drop_idle_pulls.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (calls, rej, script, pulls, drop_idle) =
                    (calls.clone(), rej.clone(), script.clone(), pulls.clone(), drop_idle.clone());
                thread::spawn(move || handle(conn, port, calls, rej, script, pulls, drop_idle));
            }
        });
        cam
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

fn soap(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="d" xmlns:trt="m" xmlns:tptz="p" xmlns:tev="e" xmlns:wsnt="n" xmlns:wsa5="a"><SOAP-ENV:Body>{body}</SOAP-ENV:Body></SOAP-ENV:Envelope>"#
    )
}

fn fault(code: &str, reason: &str) -> String {
    soap(&format!(
        r#"<SOAP-ENV:Fault><SOAP-ENV:Code><SOAP-ENV:Value>SOAP-ENV:Sender</SOAP-ENV:Value><SOAP-ENV:Subcode><SOAP-ENV:Value>{code}</SOAP-ENV:Value></SOAP-ENV:Subcode></SOAP-ENV:Code><SOAP-ENV:Reason><SOAP-ENV:Text xml:lang="en">{reason}</SOAP-ENV:Text></SOAP-ENV:Reason></SOAP-ENV:Fault>"#
    ))
}

fn respond(mut s: &TcpStream, status: &str, body: &str, chunked: bool) {
    if chunked {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/soap+xml\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        for part in body.as_bytes().chunks(700) {
            out.push_str(&format!("{:x}\r\n", part.len()));
            out.push_str(std::str::from_utf8(part).unwrap_or(""));
            out.push_str("\r\n");
        }
        out.push_str("0\r\n\r\n");
        let _ = s.write_all(out.as_bytes());
    } else {
        let _ = write!(
            s,
            "HTTP/1.1 {status}\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

fn handle(
    stream: TcpStream,
    port: u16,
    calls: Arc<Mutex<Vec<String>>>,
    reject_continuous: Arc<AtomicBool>,
    script: Arc<Mutex<Vec<bool>>>,
    pulls: Arc<AtomicUsize>,
    drop_idle: Arc<AtomicBool>,
) {
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut len = 0usize;
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).unwrap();
    let body = String::from_utf8(body).unwrap();
    let root = xml::parse(&body).expect("client sent valid XML");
    let req = root.find("Body").unwrap().children.first().unwrap();
    let op = req.name.clone();
    calls.lock().unwrap().push(format!("{path} {op}"));

    let now = tapestry_core::time::unix_now() + CLOCK_SKEW;

    if op == "GetSystemDateAndTime" {
        let iso = tapestry_core::time::iso8601_utc(now);
        let (d, t) = iso.trim_end_matches('Z').split_once('T').unwrap();
        let d: Vec<&str> = d.split('-').collect();
        let t: Vec<&str> = t.split(':').collect();
        return respond(
            &stream,
            "200 OK",
            &soap(&format!(
                "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:DateTimeType>NTP</tt:DateTimeType><tt:UTCDateTime><tt:Time><tt:Hour>{}</tt:Hour><tt:Minute>{}</tt:Minute><tt:Second>{}</tt:Second></tt:Time><tt:Date><tt:Year>{}</tt:Year><tt:Month>{}</tt:Month><tt:Day>{}</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
                t[0].parse::<u32>().unwrap(), t[1].parse::<u32>().unwrap(), t[2].parse::<u32>().unwrap(),
                d[0], d[1].parse::<u32>().unwrap(), d[2].parse::<u32>().unwrap()
            )),
            false,
        );
    }

    // Everything else needs a valid digest, created within ±30 s of *our* clock.
    let token = root.find("UsernameToken");
    let ok = token.map_or(false, |t| {
        let user = t.find_text("Username").unwrap_or_default();
        let pw = t.find_text("Password").unwrap_or_default();
        let nonce = base64_decode(&t.find_text("Nonce").unwrap_or_default()).unwrap_or_default();
        let created = t.find_text("Created").unwrap_or_default();
        let fresh = parse_iso(&created).map_or(false, |c| (c - now).abs() <= 30);
        user == USER && fresh && pw == password_digest(&nonce, &created, PASS)
    });
    if !ok {
        return respond(&stream, "400 Bad Request", &fault("ter:NotAuthorized", "Sender not Authorized"), false);
    }

    let wrong_ip = format!("http://10.99.99.99:{port}");
    let reply = match op.as_str() {
        "GetDeviceInformation" => soap("<tds:GetDeviceInformationResponse><tds:Manufacturer>tp-link</tds:Manufacturer><tds:Model>C530WS</tds:Model><tds:FirmwareVersion>1.4.1 Build 260612</tds:FirmwareVersion><tds:SerialNumber>abc</tds:SerialNumber><tds:HardwareId>2.0</tds:HardwareId></tds:GetDeviceInformationResponse>"),
        "GetCapabilities" => soap(&format!(
            "<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Device><tt:XAddr>{w}/onvif/device_service</tt:XAddr></tt:Device><tt:Events><tt:XAddr>{w}/onvif/service</tt:XAddr></tt:Events><tt:Media><tt:XAddr>{w}/onvif/service</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>{w}/onvif/service</tt:XAddr></tt:PTZ></tds:Capabilities></tds:GetCapabilitiesResponse>",
            w = wrong_ip
        )),
        "GetProfiles" => soap(r#"<trt:GetProfilesResponse><trt:Profiles fixed="true" token="profile_1"><tt:Name>mainStream</tt:Name><tt:VideoEncoderConfiguration token="main"><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>2880</tt:Width><tt:Height>1620</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration></trt:Profiles><trt:Profiles fixed="true" token="profile_2"><tt:Name>minorStream</tt:Name><tt:VideoEncoderConfiguration token="minor"><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>640</tt:Width><tt:Height>360</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration></trt:Profiles></trt:GetProfilesResponse>"#),
        "GetStreamUri" => {
            let tok = req.find_text("ProfileToken").unwrap_or_default();
            let s = if tok == "profile_1" { "stream1" } else { "stream2" };
            soap(&format!("<trt:GetStreamUriResponse><trt:MediaUri><tt:Uri>rtsp://127.0.0.1:554/{s}</tt:Uri></trt:MediaUri></trt:GetStreamUriResponse>"))
        }
        "GetPresets" => soap(r#"<tptz:GetPresetsResponse><tptz:Preset token="1"><tt:Name>Driveway</tt:Name></tptz:Preset><tptz:Preset token="2"><tt:Name></tt:Name></tptz:Preset></tptz:GetPresetsResponse>"#),
        "ContinuousMove" => {
            if reject_continuous.load(Ordering::Relaxed) {
                return respond(&stream, "500 Internal Server Error", &fault("ter:ActionNotSupported", "Optional Action Not Implemented"), false);
            }
            let pt = req.find("PanTilt").unwrap();
            calls.lock().unwrap().push(format!("  move x={} y={}", pt.attr("x").unwrap(), pt.attr("y").unwrap()));
            soap("<tptz:ContinuousMoveResponse/>")
        }
        "RelativeMove" => {
            let pt = req.find("PanTilt").unwrap();
            calls.lock().unwrap().push(format!("  relative x={} y={}", pt.attr("x").unwrap(), pt.attr("y").unwrap()));
            soap("<tptz:RelativeMoveResponse/>")
        }
        "Stop" => soap("<tptz:StopResponse/>"),
        "GotoPreset" => {
            calls.lock().unwrap().push(format!("  preset {}", req.find_text("PresetToken").unwrap()));
            soap("<tptz:GotoPresetResponse/>")
        }
        "CreatePullPointSubscription" => soap(&format!(
            "<tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa5:Address>{wrong_ip}/event-1_2020</wsa5:Address></tev:SubscriptionReference><wsnt:CurrentTime>x</wsnt:CurrentTime><wsnt:TerminationTime>y</wsnt:TerminationTime></tev:CreatePullPointSubscriptionResponse>"
        )),
        "PullMessages" => {
            // Must carry WS-Addressing To = the subscription address.
            assert!(root.find_text("To").unwrap_or_default().ends_with("/event-1_2020"));
            pulls.fetch_add(1, Ordering::Relaxed);
            let next = {
                let mut s = script.lock().unwrap();
                if s.is_empty() { None } else { Some(s.remove(0)) }
            };
            let msgs = match next {
                Some(v) => format!(
                    r#"<wsnt:NotificationMessage><wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message UtcTime="x" PropertyOperation="Changed"><tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="vsconf"/></tt:Source><tt:Data><tt:SimpleItem Name="IsMotion" Value="{v}"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>"#
                ),
                None if drop_idle.load(Ordering::Relaxed) => {
                    // What Tapo does: wait a moment, then close without a word.
                    thread::sleep(Duration::from_millis(150));
                    return;
                }
                None => {
                    thread::sleep(Duration::from_millis(300));
                    String::new()
                }
            };
            // Use chunked encoding here to exercise that path too.
            return respond(&stream, "200 OK", &soap(&format!("<tev:PullMessagesResponse><tev:CurrentTime>x</tev:CurrentTime><tev:TerminationTime>y</tev:TerminationTime>{msgs}</tev:PullMessagesResponse>")), true);
        }
        "Renew" => soap("<wsnt:RenewResponse/>"),
        "Unsubscribe" => soap("<wsnt:UnsubscribeResponse/>"),
        other => fault("ter:ActionNotSupported", &format!("{other} not supported")),
    };
    respond(&stream, "200 OK", &reply, false);
}

fn parse_iso(s: &str) -> Option<i64> {
    let (d, t) = s.trim_end_matches('Z').split_once('T')?;
    let d: Vec<i64> = d.split('-').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let t: Vec<i64> = t.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    Some(tapestry_core::time::unix_from_utc(d[0], d[1] as u32, d[2] as u32, t[0] as u32, t[1] as u32, t[2] as u32))
}

// ---------------------------------------------------------------------------

#[test]
fn full_handshake_ptz_and_events() {
    let cam = MockCamera::start();
    let mut c = OnvifClient::new("127.0.0.1", cam.port, USER, PASS);
    let info = c.connect().expect("connect");
    assert_eq!(info.model, "C530WS");
    assert!((c.time_offset - CLOCK_SKEW).abs() <= 2, "clock offset learned: {}", c.time_offset);
    // Advertised 10.99.99.99 was rewritten to the host we reached.
    assert_eq!(c.ptz_url.as_deref(), Some(format!("http://127.0.0.1:{}/onvif/service", cam.port).as_str()));
    assert_eq!(c.profile_token.as_deref(), Some("profile_1"));

    let profiles = c.profiles().unwrap();
    assert_eq!((profiles[0].width, profiles[1].height), (2880, 360));
    assert_eq!(c.stream_uri("profile_2").unwrap(), "rtsp://127.0.0.1:554/stream2");

    let presets = c.presets().unwrap();
    assert_eq!(presets.len(), 2);
    assert_eq!(presets[0].name, "Driveway");
    assert_eq!(presets[1].name, "Preset 2");

    c.continuous_move(0.5, -0.25).unwrap();
    c.stop().unwrap();
    c.goto_preset("1").unwrap();

    *cam.motion_script.lock().unwrap() = vec![true, false];
    let sub = c.subscribe(60).unwrap();
    assert!(sub.address.starts_with("http://127.0.0.1:"));
    let a = c.pull(&sub, 1).unwrap();
    let b = c.pull(&sub, 1).unwrap();
    let empty = c.pull(&sub, 1).unwrap();
    assert_eq!(a[0].is_motion(), Some(true));
    assert_eq!(b[0].is_motion(), Some(false));
    assert!(empty.is_empty());
    c.renew(&sub, 60).unwrap();
    c.unsubscribe(&sub).unwrap();

    let calls = cam.calls();
    assert!(calls.contains(&"  move x=0.500 y=-0.250".to_string()), "{calls:#?}");
    assert!(calls.contains(&"  preset 1".to_string()));
}

#[test]
fn wrong_password_is_reported_as_auth() {
    let cam = MockCamera::start();
    let mut c = OnvifClient::new("127.0.0.1", cam.port, USER, "nope");
    match c.connect() {
        Err(OnvifError::Auth(_)) => {}
        other => panic!("expected auth error, got {other:?}"),
    }
}

#[test]
fn unreachable_is_network_error() {
    // Bind then drop to get a port that's (almost certainly) closed.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut c = OnvifClient::new("127.0.0.1", port, USER, PASS);
    c.timeout = Duration::from_millis(500);
    assert!(matches!(c.connect(), Err(OnvifError::Network(_))));
}

// ----- the CameraLink / Engine layer -----------------------------------------

use tapestry_core::link::{CameraLink, LinkEvent, PtzCommand};
use tapestry_core::{Camera, Config, Engine, EngineOptions, LinkState, RecordMode};

fn camera_for(cam: &MockCamera) -> Camera {
    Camera {
        name: "Mock Gate".into(),
        host: "127.0.0.1".into(),
        username: USER.into(),
        password: PASS.into(),
        onvif_port: cam.port,
        ..Camera::default()
    }
}

fn wait_for(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let until = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < until {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn link_reports_motion_and_falls_back_to_relative_ptz() {
    let cam = MockCamera::start();
    cam.reject_continuous.store(true, Ordering::Relaxed);
    *cam.motion_script.lock().unwrap() = vec![true, false];
    let events = Arc::new(Mutex::new(Vec::<LinkEvent>::new()));
    let ev2 = events.clone();
    let link = CameraLink::start(camera_for(&cam), move |e| ev2.lock().unwrap().push(e));

    assert!(wait_for(|| events.lock().unwrap().iter().any(|e| matches!(e, LinkEvent::Detection { active: false, .. })), 10));
    let evs = events.lock().unwrap().clone();
    assert!(matches!(
        evs.iter().find(|e| matches!(e, LinkEvent::Connected { .. })),
        Some(LinkEvent::Connected { has_ptz: true, main_size: Some((2880, 1620)), .. })
    ));
    let motion: Vec<bool> =
        evs.iter().filter_map(|e| if let LinkEvent::Detection { active, .. } = e { Some(*active) } else { None }).collect();
    assert_eq!(motion, vec![true, false]);

    // Hold "right" for ~1s with ContinuousMove refused → repeated relative steps.
    link.ptz(PtzCommand::Move { pan: 1.0, tilt: 0.0 });
    thread::sleep(Duration::from_millis(1100));
    link.ptz(PtzCommand::Stop);
    thread::sleep(Duration::from_millis(400));
    let rel = cam.calls().iter().filter(|c| c.starts_with("  relative x=0.0500")).count();
    assert!(rel >= 3, "expected several relative steps, got {rel}: {:#?}", cam.calls());
    link.shutdown();
}

#[test]
fn engine_tracks_motion_and_writes_motion_log() {
    let cam = MockCamera::start();
    let dir = std::env::temp_dir().join(format!("tapestry-engine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Config {
        recordings_dir: dir.clone(),
        record_mode: RecordMode::Off, // recording covered separately with a real FFmpeg
        cameras: vec![camera_for(&cam)],
        ..Config::default()
    };
    let mut engine = Engine::start(cfg, EngineOptions { ffmpeg: None, namer: tapestry_core::recorder::utc_namer(), waker: None });
    assert!(wait_for(|| engine.cameras()[0].link == LinkState::Online, 10));
    assert!(engine.cameras()[0].has_ptz);
    assert_eq!(engine.cameras()[0].presets.len(), 2);

    *cam.motion_script.lock().unwrap() = vec![true];
    assert!(wait_for(|| engine.cameras()[0].motion, 10));
    assert_eq!(engine.cameras()[0].motion_count_today, 1);
    *cam.motion_script.lock().unwrap() = vec![false];
    assert!(wait_for(|| !engine.cameras()[0].motion, 10));

    let log = tapestry_core::clips::read_motion(&tapestry_core::clips::motion_log_path(&dir, "mock-gate"));
    assert_eq!(log.len(), 1);
    assert!(engine.log().iter().any(|e| e.text.contains("Motion: Mock Gate")));
    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tapo_style_hangups_on_idle_polls_are_not_outages() {
    let cam = MockCamera::start();
    cam.drop_idle_pulls.store(true, Ordering::Relaxed);
    let events = Arc::new(Mutex::new(Vec::<LinkEvent>::new()));
    let ev2 = events.clone();
    let link = CameraLink::start(camera_for(&cam), move |e| ev2.lock().unwrap().push(e));

    // Several idle polls get hung up on…
    assert!(wait_for(|| cam.pulls.load(Ordering::Relaxed) >= 5, 10));
    // …then motion arrives and must still get through.
    *cam.motion_script.lock().unwrap() = vec![true];
    assert!(wait_for(|| events.lock().unwrap().iter().any(|e| matches!(e, LinkEvent::Detection { active: true, .. })), 10));
    link.shutdown();

    let evs = events.lock().unwrap().clone();
    assert!(!evs.iter().any(|e| matches!(e, LinkEvent::Offline(_))), "spurious offline: {evs:#?}");
    assert_eq!(evs.iter().filter(|e| matches!(e, LinkEvent::Connected { .. })).count(), 1, "{evs:#?}");
}
