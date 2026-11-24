# Tapestry — design notes

## Why
Watch three Tapo cameras (1× C230, 2× C530WS) from a Windows desktop while working, with one
alt-tab, during two months home alone. The phone app is the only official viewer, and it's on
the phone.

## Goals
- Live view of all cameras in one window that's cheap to keep open all day.
- Pan/tilt from the desk.
- Notice motion without staring: red tiles, window title, taskbar flash.
- Play back what happened: motion-triggered clips saved on the PC, plus a day timeline.

## Non-goals / anti-goals
- Not a cloud or remote viewer. LAN only.
- No reverse-engineered Tapo cloud/private APIs. Siren, spotlight and privacy mode were left out
  on purpose, so firmware updates can't break the app.
- No playback of the cameras' SD cards. ONVIF Profile S can't do it, and the only other way is
  the private API.
- No two-way audio (not in Profile S).

## Approach
- **Video:** FFmpeg runs as a child process. It decodes RTSP on the GPU (`-hwaccel d3d11va`,
  software fallback) to fixed-size NV12 frames on a pipe, at each camera's own frame rate, and
  records with `-c:v copy` to fragmented MP4. The app uploads NV12 as two textures and a small
  OpenGL shader does YUV→RGB (range read from FFmpeg's stream summary; maths checked against
  FFmpeg's RGB in `ffmpeg_pipeline.rs`). RGB conversion in FFmpeg cost more CPU than decoding. Chosen over linking libav/GStreamer
  (painful on Windows) and over pure-Rust RTSP + OpenH264 (no H.265, and more code to own).
- **Control and events:** a hand-written ONVIF SOAP client (std-only HTTP, XML, SHA-1, WS-Security
  digest, clock-offset sync). Uses ContinuousMove/Stop for pan/tilt, falls back to RelativeMove
  steps, and gets motion from a PullPoint subscription.
- **UI:** egui/eframe 0.29 (pinned). Immediate-mode suits a wall of textures and overlays.
- **Split:** `tapestry-core` has no external dependencies, so it builds and tests offline,
  including against a mock ONVIF camera and real FFmpeg. `tapestry` (app) is a thin UI over the
  `Engine`. The engine keeps motion tracking and recording running when the window is minimised.

## Decisions made without asking (easy to change)
- Default recording: on motion, 20 s post-roll, files split every 5 min, keep 14 days, cap at
  100 GB, saved under `Videos\Tapestry`.
- The grid uses the cameras' SD stream (`stream2`) at 12 fps. An opened camera uses the HD
  stream (`stream1`) at 15 fps. Opening a camera pauses its grid stream, to stay under the
  camera's viewer limit.
- Settings live in an INI file in `%APPDATA%\Tapestry`. Passwords are stored as plain text
  (local camera logins only).
- Windows job object: every long-lived FFmpeg dies with Tapestry, even after a crash.

## Known limits / next ideas
- No pre-roll: a motion clip starts about 1–2 s after the camera reports motion. A rolling
  segment buffer would fix this.
- Click-to-aim strength isn't calibrated per camera. There's a manual "Aim strength" slider.
- Desktop notifications (toasts) could be added on top of the taskbar flash.
