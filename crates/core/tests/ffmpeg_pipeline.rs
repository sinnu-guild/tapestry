//! Real-FFmpeg tests: decoding to frames, playback ending, live-style
//! reconnects, recording + graceful stop, snapshots. Skipped (pass) if no
//! ffmpeg is on PATH.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use tapestry_core::audio::AudioStream;
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
    assert_eq!((f.width, f.height, f.nv12.len()), (320, 180, 320 * 180 * 3 / 2));
    // 2 s at 10 fps ≈ 20 frames.
    assert!((15..=25).contains(&s.frames_this_run()), "frames: {}", s.frames_this_run());
    // A test pattern isn't all black.
    assert!(f.y_plane().iter().any(|&y| y > 80));
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
    assert_eq!(f.nv12.len(), 200 * 200 * 3 / 2);
    // Top-left pixel is padding (black).
    assert_eq!(f.color.to_rgb8(f.y_plane()[0], f.uv_plane()[0], f.uv_plane()[1]), [0, 0, 0]);
}

/// Encode a test pattern with the given pixel format and colour tags.
fn make_colour_sample(ff: &Path, dir: &Path, name: &str, pix_fmt: &str, extra: &[&str]) -> PathBuf {
    let out = dir.join(name);
    let st = Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=5:duration=0.2"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", pix_fmt])
        .args(extra)
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    out
}

/// FFmpeg's own RGB conversion of the first frame (what Tapestry used to show).
fn ffmpeg_rgb(ff: &Path, file: &Path) -> Vec<u8> {
    let o = Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"])
        .output()
        .unwrap();
    assert!(o.status.success());
    o.stdout
}

/// Mean per-channel difference between FFmpeg's RGB and ours (computed from
/// the NV12 frame and the colour info the stream detected), on flat areas so
/// chroma-upsampling differences at edges don't count.
fn colour_error(ff: &Path, file: &Path) -> (tapestry_core::ColorInfo, f64) {
    colour_error_sized(ff, file, 320, 180)
}

/// `colour_error` for a single-frame file of the given (native) size.
fn colour_error_sized(ff: &Path, file: &Path, width: u32, height: u32) -> (tapestry_core::ColorInfo, f64) {
    let s = VideoStream::start(ff.to_path_buf(), StreamSpec::file(file, Duration::ZERO, 16.0, width, height, 5), None);
    assert!(wait(10, || s.latest().is_some()), "status: {:?}", s.status());
    let f = s.latest().unwrap();
    // The sample is a single frame: the one FFmpeg converts below.
    assert_eq!(f.seq, 1);
    let rgb = ffmpeg_rgb(ff, file);
    let (w, h) = (f.width as usize, f.height as usize);
    let (y, uv) = (f.y_plane(), f.uv_plane());
    let (mut sum, mut n) = (0f64, 0usize);
    let mut bias = [0f64; 3];
    for row in (2..h - 2).step_by(2) {
        for col in (2..w - 2).step_by(2) {
            let c = (row / 2) * w + (col / 2) * 2;
            // Flat neighbourhood only.
            let near = |dr: isize, dc: isize| y[((row as isize + dr) as usize) * w + (col as isize + dc) as usize];
            let v0 = near(0, 0) as i32;
            if [(-2, 0), (2, 0), (0, -2), (0, 2)].iter().any(|&(a, b)| (near(a, b) as i32 - v0).abs() > 2) {
                continue;
            }
            let ours = f.color.to_rgb8(y[row * w + col], uv[c], uv[c + 1]);
            for k in 0..3 {
                let d = ours[k] as f64 - rgb[(row * w + col) * 3 + k] as f64;
                sum += d.abs();
                bias[k] += d;
                n += 1;
            }
        }
    }
    assert!(n > 1000, "too few flat pixels ({n})");
    let px = (n / 3) as f64;
    eprintln!("  signed bias R/G/B: {:+.2} {:+.2} {:+.2}", bias[0] / px, bias[1] / px, bias[2] / px);
    (f.color, sum / n as f64)
}

/// The GPU conversion must match what FFmpeg produced before, for both
/// colour ranges the cameras use (and for BT.709-tagged video).
#[test]
fn gpu_colour_maths_matches_ffmpeg() {
    let Some(ff) = ff() else { return };
    let dir = tmp("colour");
    let cases = [
        ("limited.mp4", "yuv420p", vec![], false, false),
        ("full.mp4", "yuvj420p", vec![], true, false),
        ("hd.mp4", "yuv420p", vec!["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"], false, true),
    ];
    for (name, pix, extra, full, bt709) in cases {
        let file = make_colour_sample(&ff, &dir, name, pix, &extra);
        let (info, err) = colour_error(&ff, &file);
        assert_eq!((info.full_range, info.bt709), (full, bt709), "{name}: detected {info:?}");
        assert!(err < 1.0, "{name}: mean colour error {err:.2} (of 255)");
        eprintln!("{name}: {info:?}, mean error {err:.3}");
    }
}

#[test]
fn audio_decodes_camera_style_sound_in_real_time() {
    let Some(ff) = ff() else { return };
    let dir = tmp("audio");
    let sample = make_sample(&ff, &dir, 2);
    let input: Vec<String> = ["-hide_banner", "-loglevel", "error", "-re", "-i"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(sample.to_string_lossy().into_owned()))
        .collect();
    let started = Instant::now();
    let s = AudioStream::start(ff, input, false, Arc::new(AtomicU32::new(100)), None);
    assert!(wait(10, || s.status() == StreamStatus::Ended), "status: {:?}", s.status());
    // 2 s of 8 kHz audio in 40 ms chunks ≈ 50 chunks, delivered in about real time.
    assert!((45..=52).contains(&s.chunks_this_run()), "chunks: {}", s.chunks_this_run());
    assert!(started.elapsed() >= Duration::from_millis(1500), "not real time: {:?}", started.elapsed());
}

/// Plays a real camera's sound for 5 s through the speakers:
/// `TAPESTRY_TEST_RTSP=rtsp://user:pass@ip:554/stream2 cargo test -- --ignored live_camera_sound`
#[test]
#[ignore]
fn live_camera_sound() {
    let (Some(ff), Ok(url)) = (ff(), std::env::var("TAPESTRY_TEST_RTSP")) else { return };
    let s = AudioStream::rtsp(ff, &url, Arc::new(AtomicU32::new(200)), None);
    assert!(wait(15, || s.status() == StreamStatus::Live), "status: {:?}", s.status());
    let at_live = s.chunks_this_run();
    thread::sleep(Duration::from_secs(5));
    let got = s.chunks_this_run() - at_live;
    assert!(s.output_error().is_none(), "{:?}", s.output_error());
    // 5 s at 25 chunks/s, allowing for network jitter.
    assert!((110..=140).contains(&got), "chunks in 5 s: {got}");
    assert_eq!(s.status(), StreamStatus::Live);
}

#[test]
fn audio_from_silent_input_fails_without_retrying() {
    let Some(ff) = ff() else { return };
    let dir = tmp("noaudio");
    let out = dir.join("video-only.mkv");
    let st = Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=5:duration=1"])
        .args(["-c:v", "libx264", "-preset", "ultrafast"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    let input = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-i".into(), out.to_string_lossy().into()];
    // reconnect = true, like a live camera: a missing audio track must still be final.
    let s = AudioStream::start(ff, input, true, Arc::new(AtomicU32::new(100)), None);
    assert!(wait(10, || matches!(s.status(), StreamStatus::Failed(_))), "status: {:?}", s.status());
    let StreamStatus::Failed(msg) = s.status() else { unreachable!() };
    assert!(msg.contains("sound"), "{msg}");
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
        fps: None,
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

/// Native rate: every source frame arrives exactly once (no duplicates that
/// would judder, no drops), and frames keep coming while buffers are recycled.
#[test]
fn native_rate_delivers_each_frame_once() {
    let Some(ff) = ff() else { return };
    let dir = tmp("native");
    let sample = make_sample(&ff, &dir, 2); // 2 s at 15 fps = 30 frames
    let spec = StreamSpec {
        input_args: vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-use_wallclock_as_timestamps".into(),
            "1".into(),
            "-readrate".into(),
            "4".into(),
            "-i".into(),
            sample.to_string_lossy().into(),
        ],
        width: 320,
        height: 180,
        fps: None,
        reconnect: false,
    };
    let s = VideoStream::start(ff, spec, None);
    assert!(wait(15, || !matches!(s.status(), StreamStatus::Connecting | StreamStatus::Live)), "status: {:?}", s.status());
    assert_eq!(s.status(), StreamStatus::Ended);
    assert_eq!(s.frames_this_run(), 30);
    let f = s.latest().expect("a frame");
    assert_eq!((f.nv12.len(), f.seq), (320 * 180 * 3 / 2, 30));
}

#[test]
fn missing_ffmpeg_fails_cleanly() {
    let s = VideoStream::start(PathBuf::from("/definitely/not/ffmpeg"), StreamSpec::rtsp("rtsp://x/y", 64, 36, Some(5)), None);
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

/// Same check on real recordings: `TAPESTRY_COLOUR_CLIPS=a.mp4;b.mp4 cargo test -- --ignored real_clip`.
#[test]
#[ignore]
fn real_clip_colours_match_ffmpeg() {
    let Some(ff) = ff() else { return };
    let Ok(list) = std::env::var("TAPESTRY_COLOUR_CLIPS") else { return eprintln!("set TAPESTRY_COLOUR_CLIPS") };
    let dir = tmp("real-colour");
    for (i, clip) in list.split(';').filter(|s| !s.is_empty()).enumerate() {
        // First frame only, without re-encoding.
        let one = dir.join(format!("clip{i}.mp4"));
        let st = Command::new(&ff)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i", clip, "-an", "-c:v", "copy", "-frames:v", "1"])
            .arg(&one)
            .status()
            .unwrap();
        assert!(st.success());
        let (info, err) = colour_error_sized(&ff, &one, 2880, 1620);
        eprintln!("{clip}: {info:?}, mean error {err:.3}");
        // FFmpeg rounds down in fixed point; the GPU (and this check) round to
        // nearest, so ~0.5-1 step brighter on average is expected, not a bug.
        assert!(err < 1.5, "{clip}: mean colour error {err:.2}");
    }
}
