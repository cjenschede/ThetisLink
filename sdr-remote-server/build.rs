// SPDX-License-Identifier: GPL-2.0-or-later

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let icon_path = format!("{}/thetislink.ico", out_dir);

    generate_ico(&icon_path, true);
    generate_rgba(&format!("{}/icon_rgba.bin", out_dir), true);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(&icon_path);
    res.set("ProductName", "ThetisLink");
    res.set("FileDescription", "ThetisLink Server — SDR Remote for Thetis");
    res.set("LegalCopyright", "Copyright © 2025-2026 Chiron van der Burgt");
    res.compile().unwrap();
}

/// Generate a multi-size ICO file with a spectrum-themed icon
fn generate_ico(path: &str, is_server: bool) {
    let sizes: &[usize] = &[16, 32, 48];
    let mut data = Vec::new();

    // ICO Header
    data.extend_from_slice(&[0, 0]); // reserved
    data.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    data.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    // Calculate offsets
    let dir_size = 16 * sizes.len();
    let mut offset = 6 + dir_size;
    let mut image_data_blocks: Vec<Vec<u8>> = Vec::new();

    for &size in sizes {
        let pixels = generate_icon_pixels(size, is_server);
        let image_data = encode_bmp_for_ico(&pixels, size);

        // ICO Directory Entry
        data.push(if size == 256 { 0 } else { size as u8 });
        data.push(if size == 256 { 0 } else { size as u8 });
        data.push(0);
        data.push(0);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&32u16.to_le_bytes());
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

fn encode_bmp_for_ico(pixels: &[[u8; 4]], size: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let pixel_data_size = size * size * 4;
    let mask_row_bytes = ((size + 31) / 32) * 4;
    let mask_size = mask_row_bytes * size;

    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&(size as i32).to_le_bytes());
    data.extend_from_slice(&(size as i32 * 2).to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&32u16.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&((pixel_data_size + mask_size) as u32).to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());

    for y in (0..size).rev() {
        for x in 0..size {
            let [b, g, r, a] = pixels[y * size + x];
            data.extend_from_slice(&[b, g, r, a]);
        }
    }

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

fn generate_icon_pixels(size: usize, is_server: bool) -> Vec<[u8; 4]> {
    let mut pixels = vec![[0u8, 0, 0, 0]; size * size];
    let s = size as f64;
    let corner_r = s * 0.15;

    for y in 0..size {
        for x in 0..size {
            let fx = x as f64 + 0.5;
            let fy = y as f64 + 0.5;

            if !in_rounded_rect(fx, fy, 0.5, 0.5, s - 0.5, s - 0.5, corner_r) {
                continue;
            }

            let mut r = 10u8;
            let mut g = 15u8;
            let mut b = 30u8;
            let mut a = 255u8;

            let t = fx / s;
            let u = fy / s;

            let cx = (t - 0.5) * 8.0;
            let peak = (-cx * cx * 1.5).exp() * 0.25;
            let ripple = (t * s * 1.2).sin() * 0.02 + (t * s * 2.7).cos() * 0.015;
            let wave_pos = 0.55 - peak - ripple;

            if u > wave_pos && u < wave_pos + 0.3 {
                let fade = 1.0 - (u - wave_pos) / 0.3;
                let intensity = (fade * 25.0) as u8;
                b = b.saturating_add(intensity.saturating_mul(2));
                g = g.saturating_add(intensity);
            }

            let line_dist = ((u - wave_pos) * s).abs();
            if line_dist < 1.5 {
                let line_alpha = (1.0 - line_dist / 1.5).clamp(0.0, 1.0);
                let la = (line_alpha * 255.0) as u8;
                if is_server {
                    r = blend(r, 0, la);
                    g = blend(g, 210, la);
                    b = blend(b, 120, la);
                } else {
                    r = blend(r, 0, la);
                    g = blend(g, 200, la);
                    b = blend(b, 255, la);
                }
            }

            let vfo_dist = ((t - 0.5) * s).abs();
            if vfo_dist < 0.8 {
                let vfo_alpha = (1.0 - vfo_dist / 0.8).clamp(0.0, 1.0);
                let va = (vfo_alpha * 180.0) as u8;
                r = blend(r, 255, va);
                g = blend(g, 50, va);
                b = blend(b, 50, va);
            }

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

            if edge_dist < 1.0 {
                a = (edge_dist * 255.0).clamp(0.0, 255.0) as u8;
            }

            pixels[y * size + x] = [b, g, r, a];
        }
    }

    pixels
}

fn in_rounded_rect(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
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

fn edge_distance(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> f64 {
    for &(cx, cy) in &[(x0 + r, y0 + r), (x1 - r, y0 + r), (x0 + r, y1 - r), (x1 - r, y1 - r)] {
        let ddx = x - cx;
        let ddy = y - cy;
        let is_in_corner = (x < x0 + r || x > x1 - r) && (y < y0 + r || y > y1 - r);
        if is_in_corner {
            return r - (ddx * ddx + ddy * ddy).sqrt();
        }
    }
    let dx = (x - x0).min(x1 - x).max(0.0);
    let dy = (y - y0).min(y1 - y).max(0.0);
    dx.min(dy)
}

fn generate_rgba(path: &str, is_server: bool) {
    let pixels = generate_icon_pixels(32, is_server);
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for [b, g, r, a] in &pixels {
        rgba.extend_from_slice(&[*r, *g, *b, *a]);
    }
    std::fs::write(path, &rgba).unwrap();
}

fn blend(base: u8, overlay: u8, alpha: u8) -> u8 {
    let a = alpha as u16;
    ((base as u16 * (255 - a) + overlay as u16 * a) / 255).min(255) as u8
}
