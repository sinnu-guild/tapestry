//! Real-FFmpeg tests: decoding to frames, playback ending, live-style
//! reconnects, recording + graceful stop, snapshots. Skipped (pass) if no
//! ffmpeg is on PATH.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tapestry_core::ffmpeg;
use tapestry_core::recorder::{utc_namer, Recorder, RecorderEvent};
use tapestry_core::stream::{StreamSpec, StreamStatus, VideoStream};

fn ff() -> Option<PathBuf> {
    ffmpeg::locate("")
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tapestry-ff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A short H.264 + A-law clip, like a Tapo camera sends.
fn make_sample(ff: &Path, dir: &Path, secs: u32) -> PathBuf {
    let out = dir.join("sample.mkv");
    let st = Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size=640x360:rate=15:duration={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate=8000:duration={secs}"))
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "15", "-pix_fmt", "yuv420p", "-c:a", "pcm_alaw"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    out
}

fn wait<F: FnMut() -> bool>(secs: u64, mut f: F) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn file_playback_delivers_frames_then_ends() {
    let Some(ff) = ff() else { return eprintln!("no ffmpeg; skipping") };
    let dir = tmp("play");
    let sample = make_sample(&ff, &dir, 2);
    let s = VideoStream::start(ff.clone(), StreamSpec::file(&sample, Duration::ZERO, 4.0, 320, 180, 10), None);
    assert!(wait(10, || s.status() == StreamStatus::Ended), "status: {:?}", s.status());
    let f = s.latest().expect("got a frame");
    assert_eq!((f.width, f.height, f.rgba.len()), (320, 180, 320 * 180 * 4));
    // 2 s at 10 fps ≈ 20 frames.
    assert!((15..=25).contains(&s.frames_this_run()), "frames: {}", s.frames_this_run());
    // A test pattern isn't all black.
    assert!(f.rgba.chunks(4).any(|p| p[0] > 50 || p[1] > 50 || p[2] > 50));
}

#[test]
fn letterboxes_odd_aspect_into_fixed_size() {
    let Some(ff) = ff() else { return };
    let dir = tmp("aspect");
    let sample = make_sample(&ff, &dir, 1);
    // Ask for a square output: 16:9 video must be padded, size must be exact.
    let s = VideoStream::start(ff, StreamSpec::file(&sample, Duration::ZERO, 8.0, 200, 200, 5), None);
    assert!(wait(10, || s.latest().is_some()));
    let f = s.latest().unwrap();
    assert_eq!(f.rgba.len(), 200 * 200 * 4);
    // Top-left pixel is padding (black).
    assert_eq!(&f.rgba[0..3], &[0, 0, 0]);
}

#[test]
fn live_stream_reconnects_after_input_ends() {
    let Some(ff) = ff() else { return };
    let dir = tmp("live");
    let sample = make_sample(&ff, &dir, 1);
    // A "live" source that ends after 1 s: the supervisor should restart it.
    let spec = StreamSpec {
        input_args: vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-re".into(),
            "-i".into(),
            sample.to_string_lossy().into(),
        ],
        width: 160,
        height: 90,
        fps: 10,
        reconnect: true,
    };
    let s = VideoStream::start(ff, spec, None);
    assert!(wait(10, || matches!(s.status(), StreamStatus::Retrying { .. })), "status: {:?}", s.status());
    assert!(wait(10, || s.status() == StreamStatus::Live), "came back: {:?}", s.status());
    s.set_paused(true);
    assert!(wait(5, || s.status() == StreamStatus::Paused));
    s.set_paused(false);
    assert!(wait(10, || s.status() == StreamStatus::Live));
}

#[test]
fn missing_ffmpeg_fails_cleanly() {
    let s = VideoStream::start(PathBuf::from("/definitely/not/ffmpeg"), StreamSpec::rtsp("rtsp://x/y", 64, 36, 5), None);
    assert!(wait(5, || matches!(s.status(), StreamStatus::Failed(_))));
}

#[test]
fn records_playable_clip_and_stops_gracefully() {
    let Some(ff) = ff() else { return };
    let dir = tmp("rec");
    let sample = make_sample(&ff, &dir, 3);
    let input = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-re".into(),
        "-stream_loop".into(),
        "-1".into(),
        "-i".into(),
        sample.to_string_lossy().into_owned(),
    ];
    let mut r =
        Recorder::new(ff.clone(), String::new(), dir.join("cam"), utc_namer(), Duration::from_secs(60)).with_input_args(input);

    let ev = r.update(true);
    let path = match ev.as_slice() {
        [RecorderEvent::Started(p)] => p.clone(),
        other => panic!("expected start, got {other:?}"),
    };
    assert!(r.is_recording());
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(100));
        assert!(r.update(true).is_empty());
    }
    let ev = r.update(false);
    assert_eq!(ev, vec![RecorderEvent::Finished(path.clone())]);
    assert!(!r.is_recording());

    // Layout: cam/<day>/<HH-MM-SS>.mp4 and it plays for ~3 s.
    let clips = tapestry_core::clips::scan(&dir);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].path, path);
    let d = ffmpeg::probe_duration(&ff, &path).expect("has duration");
    assert!(d > Duration::from_millis(2000) && d < Duration::from_secs(6), "duration {d:?}");

    // Audio was transcoded A-law → AAC so it fits in MP4.
    let out = Command::new(&ff).args(["-hide_banner", "-i"]).arg(&path).output().unwrap();
    let info = String::from_utf8_lossy(&out.stderr);
    assert!(info.contains("Audio: aac"), "{info}");
    assert!(info.contains("Video: h264"), "{info}");
}

#[test]
fn recorder_rotates_long_clips() {
    let Some(ff) = ff() else { return };
    let dir = tmp("rotate");
    let sample = make_sample(&ff, &dir, 2);
    let input = vec![
        "-loglevel".into(),
        "error".into(),
        "-re".into(),
        "-stream_loop".into(),
        "-1".into(),
        "-i".into(),
        sample.to_string_lossy().into_owned(),
    ];
    let mut r = Recorder::new(ff, String::new(), dir.join("cam"), utc_namer(), Duration::from_secs(10)).with_input_args(input);
    r.max_clip = Duration::from_secs(2); // shorten after construction for the test
    let mut finished = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(5500) {
        for e in r.update(true) {
            if matches!(e, RecorderEvent::Finished(_)) {
                finished += 1;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    r.update(false);
    assert!(finished >= 2, "rotated {finished} times");
    // Same-second names get a -2 suffix rather than overwriting.
    assert!(tapestry_core::clips::scan(&dir).len() >= 3);
}

#[test]
fn recorder_reports_bad_source() {
    let Some(ff) = ff() else { return };
    let dir = tmp("bad");
    let mut r = Recorder::new(ff, "rtsp://127.0.0.1:9/nothing".into(), dir.join("cam"), utc_namer(), Duration::from_secs(60));
    let mut failed = None;
    let ok = wait(15, || {
        for e in r.update(true) {
            if let RecorderEvent::Failed(m) = e {
                failed = Some(m);
            }
        }
        failed.is_some()
    });
    assert!(ok);
    assert!(tapestry_core::clips::scan(&dir).is_empty(), "no junk files left behind");
}

#[test]
fn snapshot_and_probe() {
    let Some(ff) = ff() else { return };
    let dir = tmp("snap");
    let sample = make_sample(&ff, &dir, 1);
    // snapshot() adds RTSP-only input flags, so exercise the same output side directly.
    let out = dir.join("snap.jpg");
    let st = ffmpeg::command(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&sample)
        .args(["-frames:v", "1", "-q:v", "2", "-update", "1", "-y"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success() && out.metadata().unwrap().len() > 1000);
    let d = ffmpeg::probe_duration(&ff, &sample).unwrap();
    assert!(d >= Duration::from_millis(900));
    assert!(ffmpeg::version(&ff).unwrap().starts_with("ffmpeg version"));
}
