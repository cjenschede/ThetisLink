// SPDX-License-Identifier: GPL-2.0-or-later
//
// VRX-channelizer spike — proves the FFT-channelizer pipeline end-to-end
// with a synthetic test signal. Generates a complex test tone, runs it
// through window+FFT, bin-select, iFFT, overlap-add, USB demod, AGC,
// and writes the result as a mono 8 kHz WAV to `target/release/vrx-spike-out.wav`.
//
// Expected result: a clean 1 kHz tone (the synthesized USB-modulated
// audio at +5 kHz carrier offset, tuned in by selecting bins
// [+5 kHz, +8 kHz] from the 384 kHz wideband, demodulated as SSB-USB).
//
// Parameters per M1 design (see brief):
//   input rate   = 384 kHz complex I/Q
//   FFT size N   = 6144  (bin width 62.5 Hz; mixed-radix)
//   FFT window   = Hann
//   input hop    = 3072  (50% overlap)
//   iFFT size    = 128   (zero-padded; output rate = 8 kHz)
//   VRX BW       = 3 kHz (48 bins above carrier)
//   demod        = SSB-USB (audio = real part after bin isolation)
//   AGC          = EMA, τ_attack=10ms, τ_decay=500ms, target=0.5

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;
use std::f32::consts::TAU;
use std::fs::File;
use std::io::Write;

const INPUT_RATE: f32 = 384_000.0;
const FFT_N: usize = 6144;
const INPUT_HOP: usize = 3072;
const IFFT_N: usize = 128;
const OUTPUT_RATE: f32 = 8000.0;
const OUTPUT_HOP: usize = 64;
const VRX_BW_BINS: usize = 48; // 3 kHz at 62.5 Hz/bin
const VRX_CARRIER_OFFSET_HZ: f32 = 5000.0;

fn main() {
    // ---- 0. Args parsing (minimal — no clap dep) ----
    // Usage:
    //   vrx-spike                                    → synthetic test
    //   vrx-spike --input <path>                     → load IQ from file
    //   vrx-spike --input <path> --offset <hz>       → tune offset
    //   vrx-spike --input <path> --offset <hz> --out <wav>
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input_path: Option<String> = None;
    let mut carrier_offset_hz: f32 = VRX_CARRIER_OFFSET_HZ;
    let mut out_path: String = "vrx-spike-out.wav".to_string();
    let mut mode_is_lsb: bool = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--input" => { input_path = args.get(i + 1).cloned(); i += 2; }
            "--offset" => {
                carrier_offset_hz = args.get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(VRX_CARRIER_OFFSET_HZ);
                i += 2;
            }
            "--out" => { out_path = args.get(i + 1).cloned().unwrap_or(out_path); i += 2; }
            "--mode" => {
                match args.get(i + 1).map(|s| s.to_lowercase()).as_deref() {
                    Some("usb") => mode_is_lsb = false,
                    Some("lsb") => mode_is_lsb = true,
                    Some(other) => eprintln!("Unknown mode '{}', using USB", other),
                    None => {}
                }
                i += 2;
            }
            "--help" | "-h" => {
                println!("vrx-spike — FFT-channelizer test tool");
                println!("  (no args)              synthetic 1 kHz tone via +5 kHz USB carrier");
                println!("  --input <path>         read IQ from VRX_DUMP file");
                println!("  --offset <hz>          carrier offset from VFO (default +5000)");
                println!("  --mode usb|lsb         sideband (default usb)");
                println!("  --out <wav>            output WAV path (default vrx-spike-out.wav)");
                return;
            }
            _ => { eprintln!("Unknown arg: {}", args[i]); i += 1; }
        }
    }

    let (iq_input, input_rate_actual) = if let Some(path) = input_path.as_ref() {
        println!("VRX spike: load IQ from {} → FFT-channelizer → WAV", path);
        load_iq_file(path).expect("VRX_DUMP file read failed")
    } else {
        println!("VRX spike: synthetic test signal → FFT-channelizer → WAV");
        (generate_synthetic_signal(), INPUT_RATE as u32)
    };

    println!(
        "Input rate: {} Hz, {} samples ({:.2} s), carrier offset {} Hz",
        input_rate_actual,
        iq_input.len(),
        iq_input.len() as f32 / input_rate_actual as f32,
        carrier_offset_hz
    );

    if input_rate_actual != INPUT_RATE as u32 {
        eprintln!(
            "WARNING: input file rate {} Hz ≠ expected {} Hz; FFT-channelizer parameters \
             are tuned for {} Hz. Bin width and output rate will be off.",
            input_rate_actual, INPUT_RATE as u32, INPUT_RATE as u32
        );
    }

    // ---- 2. FFT-channelizer setup ----
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_N);
    let ifft = planner.plan_fft_inverse(IFFT_N);

    // Hann window of length N
    let window: Vec<f32> = (0..FFT_N)
        .map(|i| {
            let x = i as f32 / (FFT_N - 1) as f32;
            0.5 - 0.5 * (TAU * x).cos()
        })
        .collect();

    // Bin index of the carrier inside the 6144-point FFT output.
    let bin_width = INPUT_RATE / FFT_N as f32;
    let carrier_bin = (carrier_offset_hz / bin_width).round() as i32;
    println!(
        "FFT N={}, bin_width={} Hz, carrier_bin={}, mode={}, select {} bins {} carrier",
        FFT_N,
        bin_width,
        carrier_bin,
        if mode_is_lsb { "LSB" } else { "USB" },
        VRX_BW_BINS,
        if mode_is_lsb { "below" } else { "above" }
    );

    let mut fft_buf = vec![Complex32::new(0.0, 0.0); FFT_N];
    let mut ifft_buf = vec![Complex32::new(0.0, 0.0); IFFT_N];
    let mut ola_tail = vec![0.0_f32; OUTPUT_HOP]; // length = iFFT_N/2

    // ---- 3. AGC state ----
    let t_sample = 1.0 / OUTPUT_RATE;
    let attack_alpha = 1.0 - (-t_sample / 0.010).exp();
    let decay_alpha = 1.0 - (-t_sample / 0.500).exp();
    let agc_target = 0.5_f32;
    let agc_max_gain = 10.0_f32;
    // Initialize envelope at a sane default (~ target signal amplitude)
    // so the first samples don't hit max_gain and overshoot.
    let mut agc_envelope = 0.05_f32;

    // ---- 4. Main loop — slide a 6144-sample window with 3072 hop ----
    let mut audio_out: Vec<f32> = Vec::new();
    let mut raw_pre_agc_peak: f32 = 0.0; // diagnostic
    let mut cursor: usize = 0;
    while cursor + FFT_N <= iq_input.len() {
        // Window + load into FFT buffer
        for i in 0..FFT_N {
            fft_buf[i] = iq_input[cursor + i] * window[i];
        }
        // Forward FFT in-place
        fft.process(&mut fft_buf);

        // Bin-select → iFFT input (zero everything first)
        for slot in ifft_buf.iter_mut() {
            *slot = Complex32::new(0.0, 0.0);
        }
        if mode_is_lsb {
            // LSB: select bins BELOW the carrier and place them at the
            // negative-frequency side of the iFFT (high indices). The
            // iFFT output is then a complex signal whose negative-
            // frequency content matches the LSB sideband; taking the
            // real part recovers the audio (real signals have mirror-
            // symmetric spectra).
            for i in 0..VRX_BW_BINS {
                let bin = ((carrier_bin - 1 - i as i32).rem_euclid(FFT_N as i32)) as usize;
                ifft_buf[IFFT_N - 1 - i] = fft_buf[bin];
            }
        } else {
            // USB: select bins ABOVE the carrier, place at positive
            // iFFT slots. Real part recovers the audio.
            for i in 0..VRX_BW_BINS {
                let bin = ((carrier_bin + i as i32).rem_euclid(FFT_N as i32)) as usize;
                ifft_buf[i] = fft_buf[bin];
            }
        }
        // unused iFFT slots stay zero (zero-pad)

        // Inverse FFT in-place
        ifft.process(&mut ifft_buf);

        // FFT channelizer normalization. Empirical: for an input cos of amplitude A the
        // pre-AGC peak after OLA is A at norm = 1/N_fft. (The channelizer iFFT over
        // selected bins only does not reconstruct Hann-shaped output - the output is
        // constant across the frame - so the OLA of two overlapping constants gives a 2x
        // factor, which is compensated here.)
        let norm = 1.0 / FFT_N as f32;

        // Overlap-add: emit OUTPUT_HOP samples; sum new[0..64] + old_tail[0..64].
        // Then save new[64..128] as the next frame's tail.
        for i in 0..OUTPUT_HOP {
            let sample = ifft_buf[i].re * norm + ola_tail[i];
            raw_pre_agc_peak = raw_pre_agc_peak.max(sample.abs());
            // AGC
            let abs_in = sample.abs();
            let alpha = if abs_in > agc_envelope { attack_alpha } else { decay_alpha };
            agc_envelope += alpha * (abs_in - agc_envelope);
            let gain = (agc_target / agc_envelope.max(1e-6)).min(agc_max_gain);
            audio_out.push(sample * gain);
        }
        for i in 0..OUTPUT_HOP {
            ola_tail[i] = ifft_buf[i + OUTPUT_HOP].re * norm;
        }

        cursor += INPUT_HOP;
    }

    println!(
        "Channelizer produced {} audio samples ({:.2} s @ {} Hz)",
        audio_out.len(),
        audio_out.len() as f32 / OUTPUT_RATE,
        OUTPUT_RATE as u32
    );

    // ---- 5. Write WAV ----
    write_wav_mono_f32_as_pcm16(&out_path, OUTPUT_RATE as u32, &audio_out)
        .expect("WAV write failed");
    println!("Wrote {} ({} samples)", out_path, audio_out.len());

    let observed_peak = audio_out.iter().fold(0.0_f32, |a, &b| a.max(b.abs()));
    println!("Pre-AGC peak: {:.4}", raw_pre_agc_peak);
    println!("Audio peak after AGC: {:.3} (target ~{})", observed_peak, agc_target);
}

fn generate_synthetic_signal() -> Vec<Complex32> {
    // SSB-USB suppressed-carrier modulated with 1 kHz audio tone, on a
    // 5 kHz carrier offset → equivalent to a single complex exponential
    // at +(5000+1000) = +6000 Hz from baseband. Amplitude on/off every
    // 0.5 s so the AGC behaviour is visible.
    let duration_sec = 3.0_f32;
    let total_samples = (INPUT_RATE * duration_sec) as usize;
    let test_freq_hz = VRX_CARRIER_OFFSET_HZ + 1000.0;
    let mut out = Vec::with_capacity(total_samples);
    for n in 0..total_samples {
        let t = n as f32 / INPUT_RATE;
        let on = ((t * 2.0) as i32) & 1 == 0;
        let amp = if on { 0.05 } else { 0.0 };
        let phase = TAU * test_freq_hz * t;
        out.push(Complex32::new(amp * phase.cos(), amp * phase.sin()));
    }
    out
}

/// Load IQ samples from the VRX_DUMP file format: u32 LE sample_rate_hz
/// followed by interleaved f32 LE I, f32 LE Q pairs.
fn load_iq_file(path: &str) -> std::io::Result<(Vec<Complex32>, u32)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hdr = [0u8; 4];
    f.read_exact(&mut hdr)?;
    let rate = u32::from_le_bytes(hdr);
    let mut rest = Vec::new();
    f.read_to_end(&mut rest)?;
    if rest.len() % 8 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IQ file payload length is not a multiple of 8 (f32 I + f32 Q)",
        ));
    }
    let n = rest.len() / 8;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * 8;
        let i_val = f32::from_le_bytes([rest[off], rest[off + 1], rest[off + 2], rest[off + 3]]);
        let q_val = f32::from_le_bytes([rest[off + 4], rest[off + 5], rest[off + 6], rest[off + 7]]);
        out.push(Complex32::new(i_val, q_val));
    }
    Ok((out, rate))
}

/// Minimal mono PCM-16 WAV writer (no `hound` dep). Clips f32 to [-1, 1].
fn write_wav_mono_pcm16_inner(
    out: &mut impl Write,
    rate: u32,
    samples: &[f32],
) -> std::io::Result<()> {
    let n_samples = samples.len() as u32;
    let byte_rate = rate * 2; // 1 channel × 2 bytes/sample
    let data_bytes = n_samples * 2;
    let riff_size = 36 + data_bytes;

    // RIFF header
    out.write_all(b"RIFF")?;
    out.write_all(&riff_size.to_le_bytes())?;
    out.write_all(b"WAVE")?;
    // fmt chunk
    out.write_all(b"fmt ")?;
    out.write_all(&16u32.to_le_bytes())?; // PCM fmt size
    out.write_all(&1u16.to_le_bytes())?; // PCM
    out.write_all(&1u16.to_le_bytes())?; // mono
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&byte_rate.to_le_bytes())?;
    out.write_all(&2u16.to_le_bytes())?; // block align
    out.write_all(&16u16.to_le_bytes())?; // bits/sample
    // data chunk
    out.write_all(b"data")?;
    out.write_all(&data_bytes.to_le_bytes())?;
    for &s in samples {
        let clipped = s.clamp(-1.0, 1.0);
        let i16val = (clipped * 32767.0).round() as i16;
        out.write_all(&i16val.to_le_bytes())?;
    }
    Ok(())
}

fn write_wav_mono_f32_as_pcm16(
    path: &str,
    rate: u32,
    samples: &[f32],
) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    write_wav_mono_pcm16_inner(&mut f, rate, samples)
}
