/// Platform-agnostic audio backend trait.
/// Desktop implements this with cpal; Android could implement with Oboe/AAudio.
pub trait AudioBackend: Send + 'static {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize;
    fn write_playback(&mut self, buf: &[f32]) -> usize;
    fn capture_level(&self) -> f32;
    fn playback_level(&self) -> f32;
    fn has_error(&self) -> bool;
    fn capture_sample_rate(&self) -> u32;
    fn playback_sample_rate(&self) -> u32;
    /// Number of samples currently in the playback ring buffer.
    /// Used by the engine to pull extra frames when the buffer is low.
    fn playback_buffer_level(&self) -> usize { 0 }
    /// True if backend supports stereo output (desktop). False = mono only (Android).
    fn supports_stereo(&self) -> bool { false }
    /// Write stereo playback (left + right interleaved into device channels).
    /// Default: writes left channel only (mono fallback).
    fn write_playback_stereo(&mut self, left: &[f32], _right: &[f32]) -> usize {
        // Default (Android): play only L channel (RX1). R channel (RX2) is ignored.
        self.write_playback(left)
    }
    /// Gate mic capture: when false, the capture callback discards audio
    /// instead of writing to the ring buffer.
    fn set_capture_gate(&mut self, _open: bool) {}
    /// Mute speaker output: when true, playback callback outputs zeros
    /// regardless of ring buffer contents (instant silence).
    fn set_playback_mute(&mut self, _mute: bool) {}
}
