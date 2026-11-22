//! Live camera sound: an FFmpeg child decodes the camera's audio to 16-bit
//! PCM, and a tiny waveOut player (Windows) plays it with a short, bounded
//! buffer so it stays close to live.
//!
//! Uses the same supervisor as video (reconnects, stall detection, pause).

use crate::ffmpeg;
use crate::stream::{Run, StreamStatus, Supervisor, Waker};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Tapo cameras send 8 kHz mono A-law; we ask FFmpeg for exactly that as PCM.
pub const SAMPLE_RATE: u32 = 8000;
/// 40 ms of 16-bit mono per chunk.
pub const CHUNK_BYTES: usize = (SAMPLE_RATE as usize / 25) * 2;
/// Highest volume, in percent (camera audio is quiet, so allow a boost).
pub const MAX_VOLUME: u32 = 400;

/// FFmpeg arguments: `input_args` (everything up to `-i <input>`), then
/// audio only, as raw 16-bit mono PCM on stdout.
pub fn audio_args(input_args: Vec<String>) -> Vec<String> {
    let mut a = input_args;
    a.extend(["-vn", "-sn", "-dn", "-ac", "1", "-ar"].map(String::from));
    a.push(SAMPLE_RATE.to_string());
    a.extend(["-f", "s16le", "pipe:1"].map(String::from));
    a
}

/// Scale 16-bit little-endian samples by `percent`, clipping instead of wrapping.
pub fn apply_volume(pcm: &mut [u8], percent: u32) {
    if percent == 100 {
        return;
    }
    let gain = percent.min(MAX_VOLUME) as i32;
    for s in pcm.chunks_exact_mut(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as i32 * gain / 100;
        let v = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub struct AudioStream {
    sup: Supervisor,
    output_error: Arc<Mutex<Option<String>>>,
}

impl AudioStream {
    /// Play a camera's sound live. `volume` (percent) can be changed at any time.
    pub fn rtsp(ffmpeg: PathBuf, url: &str, volume: Arc<AtomicU32>, waker: Option<Waker>) -> Self {
        Self::start(ffmpeg, ffmpeg::rtsp_input_args(url, true), true, volume, waker)
    }

    /// `input_args`: everything up to and including `-i <input>`.
    pub fn start(
        ffmpeg: PathBuf,
        input_args: Vec<String>,
        reconnect: bool,
        volume: Arc<AtomicU32>,
        waker: Option<Waker>,
    ) -> Self {
        let output_error = Arc::new(Mutex::new(None));
        let oe = output_error.clone();
        let wk = waker.clone();
        let run = Run { args: audio_args(input_args), chunk_len: CHUNK_BYTES, reconnect, wait_for_out_stream: false };
        let sup = Supervisor::start(ffmpeg, run, waker, move |_| {
            // Open the sound device on the reader thread, on first audio.
            let mut player: Option<Result<Player, String>> = None;
            Box::new(move |mut pcm| {
                let p = player.get_or_insert_with(|| {
                    let p = Player::open(SAMPLE_RATE);
                    if let Err(e) = &p {
                        *oe.lock().unwrap() = Some(e.clone());
                        if let Some(w) = &wk {
                            w();
                        }
                    }
                    p
                });
                if let Ok(p) = p {
                    apply_volume(&mut pcm, volume.load(Ordering::Relaxed));
                    p.push(&pcm);
                }
                // The player copied it; reuse the buffer for the next chunk.
                Some(pcm)
            })
        });
        AudioStream { sup, output_error }
    }

    pub fn status(&self) -> StreamStatus {
        self.sup.status()
    }

    /// Set when there's no usable sound output on this PC.
    pub fn output_error(&self) -> Option<String> {
        self.output_error.lock().unwrap().clone()
    }

    /// 40 ms chunks received from the current FFmpeg run.
    pub fn chunks_this_run(&self) -> u64 {
        self.sup.frames_this_run()
    }

    pub fn set_paused(&self, paused: bool) {
        self.sup.set_paused(paused);
    }

    pub fn request_stop(&self) {
        self.sup.request_stop();
    }

    /// Stop and wait until FFmpeg is gone.
    pub fn stop(&mut self) {
        self.sup.stop();
    }
}

pub use player::Player;

#[cfg(windows)]
mod player {
    //! Minimal waveOut playback: a fixed pool of buffers, a short prebuffer
    //! after every underrun, and a cap on queued audio so latency can't creep.

    type Hwaveout = *mut core::ffi::c_void;

    #[repr(C, packed(1))]
    struct WaveFormatEx {
        format_tag: u16,
        channels: u16,
        samples_per_sec: u32,
        avg_bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        cb_size: u16,
    }

    #[repr(C)]
    struct WaveHdr {
        data: *mut u8,
        buffer_length: u32,
        bytes_recorded: u32,
        user: usize,
        flags: u32,
        loops: u32,
        next: *mut WaveHdr,
        reserved: usize,
    }

    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_MAPPER: u32 = u32::MAX;
    const CALLBACK_NULL: u32 = 0;
    const WHDR_DONE: u32 = 0x1;
    const HDR_SIZE: u32 = std::mem::size_of::<WaveHdr>() as u32;

    #[link(name = "winmm")]
    extern "system" {
        fn waveOutOpen(h: *mut Hwaveout, dev: u32, fmt: *const WaveFormatEx, cb: usize, inst: usize, flags: u32) -> u32;
        fn waveOutPrepareHeader(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutUnprepareHeader(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutWrite(h: Hwaveout, hdr: *mut WaveHdr, size: u32) -> u32;
        fn waveOutPause(h: Hwaveout) -> u32;
        fn waveOutRestart(h: Hwaveout) -> u32;
        fn waveOutReset(h: Hwaveout) -> u32;
        fn waveOutClose(h: Hwaveout) -> u32;
    }

    /// Buffers in the pool.
    const SLOTS: usize = 12;
    /// Most chunks queued at once (8 × 40 ms = 320 ms); newer audio is dropped past this.
    const MAX_QUEUED: usize = 8;
    /// Chunks to collect before (re)starting playback (120 ms of jitter cover).
    const PREBUFFER: usize = 3;

    struct Slot {
        hdr: Box<WaveHdr>,
        data: Vec<u8>,
        queued: bool,
    }

    pub struct Player {
        h: Hwaveout,
        slots: Vec<Slot>,
        waiting: bool,
    }

    // The handle and buffers are only touched by the thread that owns the Player.
    unsafe impl Send for Player {}

    impl Player {
        pub fn open(rate: u32) -> Result<Player, String> {
            let fmt = WaveFormatEx {
                format_tag: WAVE_FORMAT_PCM,
                channels: 1,
                samples_per_sec: rate,
                avg_bytes_per_sec: rate * 2,
                block_align: 2,
                bits_per_sample: 16,
                cb_size: 0,
            };
            let mut h: Hwaveout = std::ptr::null_mut();
            let r = unsafe { waveOutOpen(&mut h, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL) };
            if r != 0 || h.is_null() {
                return Err(format!("no sound output available (waveOut error {r})"));
            }
            unsafe { waveOutPause(h) };
            let slots = (0..SLOTS)
                .map(|_| Slot {
                    hdr: Box::new(WaveHdr {
                        data: std::ptr::null_mut(),
                        buffer_length: 0,
                        bytes_recorded: 0,
                        user: 0,
                        flags: 0,
                        loops: 0,
                        next: std::ptr::null_mut(),
                        reserved: 0,
                    }),
                    data: Vec::new(),
                    queued: false,
                })
                .collect();
            Ok(Player { h, slots, waiting: true })
        }

        /// Queue PCM for playback (drops it if too much is already queued).
        pub fn push(&mut self, pcm: &[u8]) {
            self.reclaim();
            let queued = self.slots.iter().filter(|s| s.queued).count();
            if queued == 0 && !self.waiting {
                // Ran dry: hold playback until a little audio has built up again.
                unsafe { waveOutPause(self.h) };
                self.waiting = true;
            }
            if queued >= MAX_QUEUED {
                return;
            }
            let Some(slot) = self.slots.iter_mut().find(|s| !s.queued) else { return };
            slot.data.clear();
            slot.data.extend_from_slice(pcm);
            slot.hdr.data = slot.data.as_mut_ptr();
            slot.hdr.buffer_length = slot.data.len() as u32;
            slot.hdr.flags = 0;
            let hdr: *mut WaveHdr = &mut *slot.hdr;
            unsafe {
                if waveOutPrepareHeader(self.h, hdr, HDR_SIZE) != 0 {
                    return;
                }
                if waveOutWrite(self.h, hdr, HDR_SIZE) != 0 {
                    waveOutUnprepareHeader(self.h, hdr, HDR_SIZE);
                    return;
                }
            }
            slot.queued = true;
            if self.waiting && queued + 1 >= PREBUFFER {
                unsafe { waveOutRestart(self.h) };
                self.waiting = false;
            }
        }

        /// Chunks queued and not yet played.
        pub fn queued_chunks(&mut self) -> usize {
            self.reclaim();
            self.slots.iter().filter(|s| s.queued).count()
        }

        /// Recycle buffers the device has finished playing.
        fn reclaim(&mut self) {
            for s in self.slots.iter_mut().filter(|s| s.queued) {
                // The device sets WHDR_DONE from its own thread.
                let flags = unsafe { std::ptr::read_volatile(&s.hdr.flags) };
                if flags & WHDR_DONE != 0 {
                    unsafe { waveOutUnprepareHeader(self.h, &mut *s.hdr, HDR_SIZE) };
                    s.queued = false;
                }
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            unsafe {
                waveOutReset(self.h); // marks every queued buffer done
                for s in self.slots.iter_mut().filter(|s| s.queued) {
                    waveOutUnprepareHeader(self.h, &mut *s.hdr, HDR_SIZE);
                }
                waveOutClose(self.h);
            }
        }
    }
}

#[cfg(not(windows))]
mod player {
    /// Sound output is only implemented on Windows; elsewhere audio is discarded.
    pub struct Player;

    impl Player {
        pub fn open(_rate: u32) -> Result<Player, String> {
            Err("sound output is only supported on Windows".into())
        }

        pub fn push(&mut self, _pcm: &[u8]) {}

        pub fn queued_chunks(&mut self) -> usize {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_args_are_pcm_only() {
        let a = audio_args(ffmpeg::rtsp_input_args("rtsp://cam/stream2", true));
        let pos = |s: &str| a.iter().position(|x| x == s);
        assert!(pos("-use_wallclock_as_timestamps").unwrap() < pos("-i").unwrap());
        assert!(pos("-vn").unwrap() > pos("-i").unwrap());
        assert!(pos("-vf").is_none() && pos("rawvideo").is_none());
        assert_eq!(&a[a.len() - 3..], ["-f", "s16le", "pipe:1"]);
        assert_eq!(a[pos("-ar").unwrap() + 1], "8000");
    }

    /// Needs a sound device, so it's opt-in: `cargo test -- --ignored`.
    /// Plays a quiet 0.6 s tone.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn player_plays_and_recycles_buffers() {
        let mut p = Player::open(SAMPLE_RATE).expect("sound device");
        let tone: Vec<u8> = (0..CHUNK_BYTES / 2)
            .map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 1500.0) as i16)
            .flat_map(|s| s.to_le_bytes())
            .collect();
        // A burst bigger than the cap: the excess is dropped, not queued.
        for _ in 0..20 {
            p.push(&tone);
        }
        assert_eq!(p.queued_chunks(), 8);
        // Playback drains the queue in real time (8 × 40 ms).
        let t = std::time::Instant::now();
        while p.queued_chunks() > 0 && t.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(p.queued_chunks(), 0, "device never finished the buffers");
        assert!(t.elapsed() >= std::time::Duration::from_millis(200), "drained too fast: {:?}", t.elapsed());
        // And it starts again after running dry.
        for _ in 0..4 {
            p.push(&tone);
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(p.queued_chunks(), 0, "didn't restart after an underrun");
    }

    #[test]
    fn volume_scales_and_clips() {
        let mut pcm: Vec<u8> = [1000i16, -1000, 20000, -20000].iter().flat_map(|s| s.to_le_bytes()).collect();
        apply_volume(&mut pcm, 200);
        let out: Vec<i16> = pcm.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(out, [2000, -2000, i16::MAX, i16::MIN]);

        let mut pcm: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        apply_volume(&mut pcm, 0);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 0);
    }
}
