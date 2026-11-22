//! Colour of the NV12 frames FFmpeg hands us, and the YUV → RGB transform
//! the GPU applies to them.
//!
//! Tapo cameras disagree: some send full-range ("pc") video, some limited
//! ("tv"), and none say which matrix they use, so FFmpeg's own convention
//! applies: unknown range = limited, unknown matrix = BT.601. That's also
//! what FFmpeg's RGB conversion did, so pictures look exactly as before.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorInfo {
    /// Full range (0–255) rather than limited (16–235 / 16–240).
    pub full_range: bool,
    /// BT.709 matrix rather than BT.601.
    pub bt709: bool,
}

impl ColorInfo {
    /// Read FFmpeg's description of the output stream, e.g.
    /// `Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12(pc, bt709/bt709/bt709, progressive), 2880x1620`.
    pub fn from_stream_line(line: &str) -> Option<ColorInfo> {
        let at = line.find(" nv12")? + " nv12".len();
        let rest = &line[at..];
        let props = match rest.strip_prefix('(') {
            Some(r) => &r[..r.find(')')?],
            None => "",
        };
        let mut info = ColorInfo::default();
        for p in props.split(',').map(str::trim) {
            if p == "pc" {
                info.full_range = true;
            }
            // "matrix/primaries/transfer", or just one name when all agree.
            if p.split('/').next() == Some("bt709") {
                info.bt709 = true;
            }
        }
        Some(info)
    }

    /// `rgb = m · (y, u, v) + offset`, with every value normalised to 0..1.
    /// `m` is column-major (OpenGL's order).
    pub fn rgb_transform(&self) -> ([f32; 9], [f32; 3]) {
        let (kr, kb) = if self.bt709 { (0.2126f64, 0.0722) } else { (0.299, 0.114) };
        let kg = 1.0 - kr - kb;
        // Rows R, G, B; columns Y, Cb, Cr (centred, unit range).
        let a = [
            [1.0, 0.0, 2.0 * (1.0 - kr)],
            [1.0, -2.0 * kb * (1.0 - kb) / kg, -2.0 * kr * (1.0 - kr) / kg],
            [1.0, 2.0 * (1.0 - kb), 0.0],
        ];
        // Undo the encoding range: Y' = sy·(y − oy), C' = sc·(c − ½).
        let (sy, oy, sc) = if self.full_range { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0) };
        let mid = 128.0 / 255.0;
        let scale = [sy, sc, sc];
        let sub = [sy * oy, sc * mid, sc * mid];
        let mut m = [0f32; 9];
        let mut off = [0f32; 3];
        for row in 0..3 {
            for col in 0..3 {
                m[col * 3 + row] = (a[row][col] * scale[col]) as f32;
            }
            off[row] = -(0..3).map(|col| a[row][col] * sub[col]).sum::<f64>() as f32;
        }
        (m, off)
    }

    /// The same transform on the CPU, 8-bit in and out (for tests and checks).
    pub fn to_rgb8(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let (m, off) = self.rgb_transform();
        let yuv = [y as f32 / 255.0, u as f32 / 255.0, v as f32 / 255.0];
        let mut out = [0u8; 3];
        for (row, o) in out.iter_mut().enumerate() {
            let x = (0..3).map(|col| m[col * 3 + row] * yuv[col]).sum::<f32>() + off[row];
            *o = (x.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ffmpeg_stream_lines() {
        let pc = "Stream #0:0(und): Video: rawvideo (NV12 / 0x3231564E), nv12(pc, progressive), 2880x1620, q=2-31";
        assert_eq!(ColorInfo::from_stream_line(pc), Some(ColorInfo { full_range: true, bt709: false }));
        let tv = "Stream #0:0(und): Video: rawvideo (NV12 / 0x3231564E), nv12(progressive), 2880x1620, q=2-31";
        assert_eq!(ColorInfo::from_stream_line(tv), Some(ColorInfo { full_range: false, bt709: false }));
        let hd = "Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12(tv, bt709/bt709/unknown, progressive), 1920x1080";
        assert_eq!(ColorInfo::from_stream_line(hd), Some(ColorInfo { full_range: false, bt709: true }));
        let bare = "Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12, 640x360";
        assert_eq!(ColorInfo::from_stream_line(bare), Some(ColorInfo::default()));
        assert_eq!(ColorInfo::from_stream_line("Stream #0:0: Video: h264 (Main), yuvj420p(pc, progressive)"), None);
    }

    #[test]
    fn black_white_and_primaries() {
        let tv = ColorInfo::default();
        assert_eq!(tv.to_rgb8(16, 128, 128), [0, 0, 0]);
        assert_eq!(tv.to_rgb8(235, 128, 128), [255, 255, 255]);
        let pc = ColorInfo { full_range: true, bt709: false };
        assert_eq!(pc.to_rgb8(0, 128, 128), [0, 0, 0]);
        assert_eq!(pc.to_rgb8(255, 128, 128), [255, 255, 255]);
        // BT.601 full-range pure red is (76, 85, 255).
        let red = pc.to_rgb8(76, 85, 255);
        assert!(red[0] >= 253 && red[1] <= 2 && red[2] <= 2, "{red:?}");
    }
}
