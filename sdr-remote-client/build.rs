fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();

    // RGBA icon is needed on all platforms (for window icon)
    generate_rgba(&format!("{}/icon_rgba.bin", out_dir), false);

    // ICO + Windows resource embedding only on Windows
    #[cfg(windows)]
    {
        let icon_path = format!("{}/thetislink.ico", out_dir);
        generate_ico(&icon_path, false);

        let mut res = winresource::WindowsResource::new();
        res.set_icon(&icon_path);
        res.set("ProductName", "ThetisLink");
        res.set("FileDescription", "ThetisLink Client — SDR Remote for Thetis");
        res.compile().unwrap();
    }
}

#[cfg(windows)]
/// Generate a multi-size ICO file with a spectrum-themed icon
fn generate_ico(path: &str, is_server: bool) {
    let sizes: &[usize] = &[16, 32, 48];
    let mut data = Vec::new();

    // ICO Header
    data.extend_from_slice(&[0, 0]); // reserved
    data.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    data.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    // Calculate offsets: header(6) + directory(16 * count) + image data
    let dir_size = 16 * sizes.len();
    let mut offset = 6 + dir_size;
    let mut image_data_blocks: Vec<Vec<u8>> = Vec::new();

    for &size in sizes {
        let pixels = generate_icon_pixels(size, is_server);
        let image_data = encode_bmp_for_ico(&pixels, size);

        // ICO Directory Entry
        data.push(if size == 256 { 0 } else { size as u8 });
        data.push(if size == 256 { 0 } else { size as u8 });
        data.push(0); // colors
        data.push(0); // reserved
        data.extend_from_slice(&1u16.to_le_bytes()); // planes
        data.extend_from_slice(&32u16.to_le_bytes()); // bpp
        data.extend_from_slice(&(image_data.len() as u32).to_le_bytes());
        data.extend_from_slice(&(offset as u32).to_le_bytes());

        offset += image_data.len();
        image_data_blocks.push(image_data);
    }

    for block in image_data_blocks {
        data.extend_from_slice(&block);
    }

    std::fs::write(path, &data).unwrap();
}

#[cfg(windows)]
/// Encode pixel data as BMP inside ICO format
fn encode_bmp_for_ico(pixels: &[[u8; 4]], size: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let pixel_data_size = size * size * 4;
    let mask_row_bytes = ((size + 31) / 32) * 4;
    let mask_size = mask_row_bytes * size;

    // BITMAPINFOHEADER
    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&(size as i32).to_le_bytes());
    data.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // 2x height for ICO
    data.extend_from_slice(&1u16.to_le_bytes()); // planes
    data.extend_from_slice(&32u16.to_le_bytes()); // bpp
    data.extend_from_slice(&0u32.to_le_bytes()); // compression
    data.extend_from_slice(&((pixel_data_size + mask_size) as u32).to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes()); // x ppi
    data.extend_from_slice(&0i32.to_le_bytes()); // y ppi
    data.extend_from_slice(&0u32.to_le_bytes()); // colors used
    data.extend_from_slice(&0u32.to_le_bytes()); // important colors

    // Pixel data (BGRA, bottom-up)
    for y in (0..size).rev() {
        for x in 0..size {
            let [b, g, r, a] = pixels[y * size + x];
            data.extend_from_slice(&[b, g, r, a]);
        }
    }

    // AND mask (1-bit, bottom-up)
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_row_bytes];
        for x in 0..size {
            let [_, _, _, a] = pixels[y * size + x];
            if a < 128 {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        data.extend_from_slice(&row);
    }

    data
}

/// Generate icon pixels: spectrum display inside rounded rectangle
/// Returns BGRA pixel data
fn generate_icon_pixels(size: usize, is_server: bool) -> Vec<[u8; 4]> {
    let mut pixels = vec![[0u8, 0, 0, 0]; size * size]; // transparent
    let s = size as f64;
    let corner_r = s * 0.15; // corner radius

    for y in 0..size {
        for x in 0..size {
            let fx = x as f64 + 0.5;
            let fy = y as f64 + 0.5;

            // Rounded rectangle test
            if !in_rounded_rect(fx, fy, 0.5, 0.5, s - 0.5, s - 0.5, corner_r) {
                continue;
            }

            // Background: dark navy (matches spectrum bg)
            let mut r = 10u8;
            let mut g = 15u8;
            let mut b = 30u8;
            let mut a = 255u8;

            let t = fx / s; // 0..1 horizontal position
            let u = fy / s; // 0..1 vertical position

            // Spectrum waveform: Gaussian peak + noise ripple
            let cx = (t - 0.5) * 8.0;
            let peak = (-cx * cx * 1.5).exp() * 0.25;
            let ripple = (t * s * 1.2).sin() * 0.02 + (t * s * 2.7).cos() * 0.015;
            let wave_pos = 0.55 - peak - ripple; // vertical position (0=top, 1=bottom)

            // Subtle fill under the waveform
            if u > wave_pos && u < wave_pos + 0.3 {
                let fade = 1.0 - (u - wave_pos) / 0.3;
                let intensity = (fade * 25.0) as u8;
                b = b.saturating_add(intensity.saturating_mul(2));
                g = g.saturating_add(intensity);
            }

            // Spectrum line (anti-aliased, 2px)
            let line_dist = ((u - wave_pos) * s).abs();
            if line_dist < 1.5 {
                let line_alpha = (1.0 - line_dist / 1.5).clamp(0.0, 1.0);
                let la = (line_alpha * 255.0) as u8;
                if is_server {
                    // Server: green accent
                    r = blend(r, 0, la);
                    g = blend(g, 210, la);
                    b = blend(b, 120, la);
                } else {
                    // Client: cyan accent (matches spectrum)
                    r = blend(r, 0, la);
                    g = blend(g, 200, la);
                    b = blend(b, 255, la);
                }
            }

            // VFO marker: thin red vertical line at center
            let vfo_dist = ((t - 0.5) * s).abs();
            if vfo_dist < 0.8 {
                let vfo_alpha = (1.0 - vfo_dist / 0.8).clamp(0.0, 1.0);
                let va = (vfo_alpha * 180.0) as u8;
                r = blend(r, 255, va);
                g = blend(g, 50, va);
                b = blend(b, 50, va);
            }

            // Subtle border glow
            let edge_dist = edge_distance(fx, fy, 0.5, 0.5, s - 0.5, s - 0.5, corner_r);
            if edge_dist < 2.0 {
                let edge_alpha = ((2.0 - edge_dist) / 2.0 * 60.0) as u8;
                if is_server {
                    g = g.saturating_add(edge_alpha / 2);
                } else {
                    b = b.saturating_add(edge_alpha / 2);
                    g = g.saturating_add(edge_alpha / 4);
                }
            }

            // Anti-aliased edge
            if edge_dist < 1.0 {
                a = (edge_dist * 255.0).clamp(0.0, 255.0) as u8;
            }

            pixels[y * size + x] = [b, g, r, a];
        }
    }

    pixels
}

/// Check if point is inside rounded rectangle
fn in_rounded_rect(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    // Check corners
    for &(cx, cy) in &[(x0 + r, y0 + r), (x1 - r, y0 + r), (x0 + r, y1 - r), (x1 - r, y1 - r)] {
        let dx = x - cx;
        let dy = y - cy;
        let is_in_corner = (x < x0 + r || x > x1 - r) && (y < y0 + r || y > y1 - r);
        if is_in_corner && dx * dx + dy * dy > r * r {
            return false;
        }
    }
    true
}

/// Distance from edge of rounded rectangle (positive = inside)
fn edge_distance(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> f64 {
    let dx = (x - x0).min(x1 - x).max(0.0);
    let dy = (y - y0).min(y1 - y).max(0.0);

    // Corner regions
    for &(cx, cy) in &[(x0 + r, y0 + r), (x1 - r, y0 + r), (x0 + r, y1 - r), (x1 - r, y1 - r)] {
        let ddx = x - cx;
        let ddy = y - cy;
        let is_in_corner = (x < x0 + r || x > x1 - r) && (y < y0 + r || y > y1 - r);
        if is_in_corner {
            return r - (ddx * ddx + ddy * ddy).sqrt();
        }
    }

    dx.min(dy)
}

/// Generate a raw 32x32 RGBA file for window icon
fn generate_rgba(path: &str, is_server: bool) {
    let pixels = generate_icon_pixels(32, is_server);
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for [b, g, r, a] in &pixels {
        rgba.extend_from_slice(&[*r, *g, *b, *a]);
    }
    std::fs::write(path, &rgba).unwrap();
}

/// Blend two color values
fn blend(base: u8, overlay: u8, alpha: u8) -> u8 {
    let a = alpha as u16;
    let result = (base as u16 * (255 - a) + overlay as u16 * a) / 255;
    result.min(255) as u8
}
