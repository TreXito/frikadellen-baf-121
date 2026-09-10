/// Server-side OG image generator.
///
/// Produces a 1200×630 PNG stats card for OpenGraph / Discord embeds.
/// Uses a built-in 5×7 bitmap font (scaled up) so no external font
/// files or heavy dependencies are needed — only the `image` crate.
use image::{ImageBuffer, Rgba, RgbaImage};
use std::io::Cursor;

// ── Colour palette (matches the panel's "midnight" theme) ────

const BG: Rgba<u8> = Rgba([11, 13, 23, 255]);
const CARD_BG: Rgba<u8> = Rgba([17, 20, 37, 255]);
const BORDER: Rgba<u8> = Rgba([30, 34, 64, 255]);
const TEXT_DIM: Rgba<u8> = Rgba([123, 130, 168, 255]);
const SUCCESS: Rgba<u8> = Rgba([0, 206, 201, 255]); // stat values
const HEADING: Rgba<u8> = Rgba([234, 237, 249, 255]);
const ACCENT2: Rgba<u8> = Rgba([162, 155, 254, 255]);

// ── 5×7 bitmap font ─────────────────────────────────────────

/// Each glyph is 5 columns × 7 rows, packed as `[u8; 5]` where each
/// byte holds one column (LSB = top row).
const FONT: &[(char, [u8; 5])] = &[
    ('A', [0x7E, 0x11, 0x11, 0x11, 0x7E]),
    ('B', [0x7F, 0x49, 0x49, 0x49, 0x36]),
    ('C', [0x3E, 0x41, 0x41, 0x41, 0x22]),
    ('D', [0x7F, 0x41, 0x41, 0x41, 0x3E]),
    ('E', [0x7F, 0x49, 0x49, 0x49, 0x41]),
    ('F', [0x7F, 0x09, 0x09, 0x09, 0x01]),
    ('G', [0x3E, 0x41, 0x49, 0x49, 0x7A]),
    ('H', [0x7F, 0x08, 0x08, 0x08, 0x7F]),
    ('I', [0x00, 0x41, 0x7F, 0x41, 0x00]),
    ('J', [0x20, 0x40, 0x41, 0x3F, 0x01]),
    ('K', [0x7F, 0x08, 0x14, 0x22, 0x41]),
    ('L', [0x7F, 0x40, 0x40, 0x40, 0x40]),
    ('M', [0x7F, 0x02, 0x0C, 0x02, 0x7F]),
    ('N', [0x7F, 0x04, 0x08, 0x10, 0x7F]),
    ('O', [0x3E, 0x41, 0x41, 0x41, 0x3E]),
    ('P', [0x7F, 0x09, 0x09, 0x09, 0x06]),
    ('Q', [0x3E, 0x41, 0x51, 0x21, 0x5E]),
    ('R', [0x7F, 0x09, 0x19, 0x29, 0x46]),
    ('S', [0x46, 0x49, 0x49, 0x49, 0x31]),
    ('T', [0x01, 0x01, 0x7F, 0x01, 0x01]),
    ('U', [0x3F, 0x40, 0x40, 0x40, 0x3F]),
    ('V', [0x1F, 0x20, 0x40, 0x20, 0x1F]),
    ('W', [0x3F, 0x40, 0x38, 0x40, 0x3F]),
    ('X', [0x63, 0x14, 0x08, 0x14, 0x63]),
    ('Y', [0x07, 0x08, 0x70, 0x08, 0x07]),
    ('Z', [0x61, 0x51, 0x49, 0x45, 0x43]),
    ('a', [0x20, 0x54, 0x54, 0x54, 0x78]),
    ('b', [0x7F, 0x48, 0x44, 0x44, 0x38]),
    ('c', [0x38, 0x44, 0x44, 0x44, 0x20]),
    ('d', [0x38, 0x44, 0x44, 0x48, 0x7F]),
    ('e', [0x38, 0x54, 0x54, 0x54, 0x18]),
    ('f', [0x08, 0x7E, 0x09, 0x01, 0x02]),
    ('g', [0x0C, 0x52, 0x52, 0x52, 0x3E]),
    ('h', [0x7F, 0x08, 0x04, 0x04, 0x78]),
    ('i', [0x00, 0x44, 0x7D, 0x40, 0x00]),
    ('j', [0x20, 0x40, 0x44, 0x3D, 0x00]),
    ('k', [0x7F, 0x10, 0x28, 0x44, 0x00]),
    ('l', [0x00, 0x41, 0x7F, 0x40, 0x00]),
    ('m', [0x7C, 0x04, 0x18, 0x04, 0x78]),
    ('n', [0x7C, 0x08, 0x04, 0x04, 0x78]),
    ('o', [0x38, 0x44, 0x44, 0x44, 0x38]),
    ('p', [0x7C, 0x14, 0x14, 0x14, 0x08]),
    ('q', [0x08, 0x14, 0x14, 0x18, 0x7C]),
    ('r', [0x7C, 0x08, 0x04, 0x04, 0x08]),
    ('s', [0x48, 0x54, 0x54, 0x54, 0x20]),
    ('t', [0x04, 0x3F, 0x44, 0x40, 0x20]),
    ('u', [0x3C, 0x40, 0x40, 0x20, 0x7C]),
    ('v', [0x1C, 0x20, 0x40, 0x20, 0x1C]),
    ('w', [0x3C, 0x40, 0x30, 0x40, 0x3C]),
    ('x', [0x44, 0x28, 0x10, 0x28, 0x44]),
    ('y', [0x0C, 0x50, 0x50, 0x50, 0x3C]),
    ('z', [0x44, 0x64, 0x54, 0x4C, 0x44]),
    ('0', [0x3E, 0x51, 0x49, 0x45, 0x3E]),
    ('1', [0x00, 0x42, 0x7F, 0x40, 0x00]),
    ('2', [0x42, 0x61, 0x51, 0x49, 0x46]),
    ('3', [0x21, 0x41, 0x45, 0x4B, 0x31]),
    ('4', [0x18, 0x14, 0x12, 0x7F, 0x10]),
    ('5', [0x27, 0x45, 0x45, 0x45, 0x39]),
    ('6', [0x3C, 0x4A, 0x49, 0x49, 0x30]),
    ('7', [0x01, 0x71, 0x09, 0x05, 0x03]),
    ('8', [0x36, 0x49, 0x49, 0x49, 0x36]),
    ('9', [0x06, 0x49, 0x49, 0x29, 0x1E]),
    (' ', [0x00, 0x00, 0x00, 0x00, 0x00]),
    ('.', [0x00, 0x60, 0x60, 0x00, 0x00]),
    (',', [0x00, 0x80, 0x60, 0x00, 0x00]),
    (':', [0x00, 0x36, 0x36, 0x00, 0x00]),
    ('/', [0x20, 0x10, 0x08, 0x04, 0x02]),
    ('-', [0x08, 0x08, 0x08, 0x08, 0x08]),
    ('+', [0x08, 0x08, 0x3E, 0x08, 0x08]),
    ('(', [0x00, 0x1C, 0x22, 0x41, 0x00]),
    (')', [0x00, 0x41, 0x22, 0x1C, 0x00]),
    ('|', [0x00, 0x00, 0x7F, 0x00, 0x00]),
    ('%', [0x23, 0x13, 0x08, 0x64, 0x62]),
    ('_', [0x40, 0x40, 0x40, 0x40, 0x40]),
];

fn glyph(ch: char) -> [u8; 5] {
    for &(c, bits) in FONT {
        if c == ch {
            return bits;
        }
    }
    // Fallback: return a small filled rectangle for unknown chars
    [0x7E, 0x7E, 0x7E, 0x7E, 0x7E]
}

/// Width of a string in pixels at the given scale.
fn text_width(s: &str, scale: u32) -> u32 {
    let chars = s.chars().count() as u32;
    if chars == 0 {
        return 0;
    }
    // 5 pixels per char + 1 pixel gap, minus trailing gap
    (chars * (5 * scale + scale)) - scale
}

/// Draw a single character at (x, y) with the given colour and scale.
fn draw_char(img: &mut RgbaImage, ch: char, x: u32, y: u32, scale: u32, color: Rgba<u8>) {
    let bits = glyph(ch);
    for col in 0..5u32 {
        let col_bits = bits[col as usize];
        for row in 0..7u32 {
            if (col_bits >> row) & 1 == 1 {
                let px = x + col * scale;
                let py = y + row * scale;
                for dy in 0..scale {
                    for dx in 0..scale {
                        let fx = px + dx;
                        let fy = py + dy;
                        if fx < img.width() && fy < img.height() {
                            img.put_pixel(fx, fy, color);
                        }
                    }
                }
            }
        }
    }
}

/// Draw a string at (x, y).
fn draw_text(img: &mut RgbaImage, text: &str, x: u32, y: u32, scale: u32, color: Rgba<u8>) {
    let mut cx = x;
    let char_advance = 5 * scale + scale; // 5px char + 1px gap, scaled
    for ch in text.chars() {
        draw_char(img, ch, cx, y, scale, color);
        cx += char_advance;
    }
}

/// Draw a string centred horizontally at the given y.
fn draw_text_centered(img: &mut RgbaImage, text: &str, y: u32, scale: u32, color: Rgba<u8>) {
    let w = text_width(text, scale);
    let x = if w < img.width() {
        (img.width() - w) / 2
    } else {
        0
    };
    draw_text(img, text, x, y, scale, color);
}

/// Draw a filled rectangle.
fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    for py in y..y.saturating_add(h).min(img.height()) {
        for px in x..x.saturating_add(w).min(img.width()) {
            img.put_pixel(px, py, color);
        }
    }
}

/// Draw a rectangle outline (1px border, scaled).
fn draw_rect_outline(
    img: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    thickness: u32,
    color: Rgba<u8>,
) {
    // Top
    fill_rect(img, x, y, w, thickness, color);
    // Bottom
    fill_rect(img, x, y + h - thickness, w, thickness, color);
    // Left
    fill_rect(img, x, y, thickness, h, color);
    // Right
    fill_rect(img, x + w - thickness, y, thickness, h, color);
}

// ── Public API ───────────────────────────────────────────────

/// Draw a line between two points using Bresenham's algorithm.
/// `dashed`: if true, alternates 4px on / 4px off.
fn draw_line(
    img: &mut RgbaImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    color: Rgba<u8>,
    thickness: u32,
    dashed: bool,
) {
    let dx = (x1 as i64 - x0 as i64).abs();
    let dy = -(y1 as i64 - y0 as i64).abs();
    let sx: i64 = if (x0 as i64) < (x1 as i64) { 1 } else { -1 };
    let sy: i64 = if (y0 as i64) < (y1 as i64) { 1 } else { -1 };
    let mut err = dx + dy;
    let mut cx = x0 as i64;
    let mut cy = y0 as i64;
    let mut step = 0u32;
    let half_t = thickness / 2;

    loop {
        let visible = !dashed || (step / 4) % 2 == 0;
        if visible {
            for dt in 0..thickness {
                let py = (cy + dt as i64 - half_t as i64).max(0) as u32;
                let px = cx.max(0) as u32;
                if px < img.width() && py < img.height() {
                    img.put_pixel(px, py, color);
                }
            }
        }
        if cx == x1 as i64 && cy == y1 as i64 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cx += sx;
        }
        if e2 <= dx {
            err += dx;
            cy += sy;
        }
        step += 1;
    }
}

/// Generate a 1200×630 stats card PNG and return the raw bytes.
///
/// `ah_points` and `bz_points` are `(unix_timestamp, cumulative_profit)` series
/// from `ProfitTracker`.  The chart plots these as AH, BZ, and Total lines.
pub fn generate_og_image(
    total_profit: i64,
    per_hour: f64,
    uptime_secs: u64,
    ah_points: &[(u64, i64)],
    bz_points: &[(u64, i64)],
) -> Vec<u8> {
    let (w, h): (u32, u32) = (1200, 630);
    let mut img: RgbaImage = ImageBuffer::from_pixel(w, h, BG);

    // ── Title bar ────────────────────────────────────────────
    let title = "Frikadellen BAF";
    draw_text_centered(&mut img, title, 30, 5, ACCENT2);

    let subtitle = "Control Panel";
    draw_text_centered(&mut img, subtitle, 75, 3, TEXT_DIM);

    // ── Three stat boxes ─────────────────────────────────────
    let box_w: u32 = 340;
    let box_h: u32 = 160;
    let box_y: u32 = 130;
    let gap: u32 = 30;
    let total_w = 3 * box_w + 2 * gap;
    let start_x = (w - total_w) / 2;

    let stats: [(&str, String); 3] = [
        ("TOTAL PROFIT", format_og_value(total_profit as f64)),
        ("PROFIT / HOUR", format_og_value(per_hour)),
        ("UPTIME", format_uptime(uptime_secs)),
    ];

    for (i, (label, value)) in stats.iter().enumerate() {
        let bx = start_x + i as u32 * (box_w + gap);

        // Card background
        fill_rect(&mut img, bx, box_y, box_w, box_h, CARD_BG);
        // Border
        draw_rect_outline(&mut img, bx, box_y, box_w, box_h, 2, BORDER);

        // Label (centred inside the box)
        let label_scale = 3u32;
        let lw = text_width(label, label_scale);
        let lx = bx + (box_w.saturating_sub(lw)) / 2;
        draw_text(&mut img, label, lx, box_y + 30, label_scale, TEXT_DIM);

        // Value (centred inside the box, larger)
        let value_scale = 5u32;
        let vw = text_width(value, value_scale);
        let vx = bx + (box_w.saturating_sub(vw)) / 2;
        draw_text(&mut img, value, vx, box_y + 80, value_scale, SUCCESS);
    }

    // ── Simple chart area placeholder ────────────────────────
    let chart_y: u32 = 330;
    let chart_h: u32 = 240;
    let chart_x: u32 = start_x;
    let chart_w: u32 = total_w;

    // Chart background card
    fill_rect(&mut img, chart_x, chart_y, chart_w, chart_h, CARD_BG);
    draw_rect_outline(&mut img, chart_x, chart_y, chart_w, chart_h, 2, BORDER);

    // Chart axes
    let ax_left = chart_x + 60;
    let ax_bottom = chart_y + chart_h - 40;
    let ax_right = chart_x + chart_w - 40;
    let ax_top = chart_y + 30;

    // Y axis
    fill_rect(&mut img, ax_left, ax_top, 2, ax_bottom - ax_top, BORDER);
    // X axis
    fill_rect(&mut img, ax_left, ax_bottom, ax_right - ax_left, 2, BORDER);

    // Axis labels
    draw_text(&mut img, "PROFIT", chart_x + 8, ax_top + 40, 2, TEXT_DIM);

    let time_label = "TIME";
    let tl_w = text_width(time_label, 2);
    let tl_x = ax_left + (ax_right - ax_left - tl_w) / 2;
    draw_text(&mut img, time_label, tl_x, ax_bottom + 12, 2, TEXT_DIM);

    // Legend
    let legend_y = chart_y + 10;
    let legend_center = chart_x + chart_w / 2;

    // AH Profit legend box (green)
    let ah_color = Rgba([46, 204, 113, 255]);
    let ah_label = "AH Profit";
    let bz_color = Rgba([230, 126, 34, 255]);
    let bz_label = "BZ Profit";
    let total_color = Rgba([52, 152, 219, 255]);
    let total_label = "Total";

    let legend_scale = 2u32;
    let swatch = 14u32;
    let legend_gap = 24u32;

    let ah_w = swatch + 6 + text_width(ah_label, legend_scale);
    let bz_w = swatch + 6 + text_width(bz_label, legend_scale);
    let tot_w = swatch + 6 + text_width(total_label, legend_scale);
    let total_legend_w = ah_w + legend_gap + bz_w + legend_gap + tot_w;
    let lx = legend_center.saturating_sub(total_legend_w / 2);

    // AH
    fill_rect(&mut img, lx, legend_y + 1, swatch, swatch, ah_color);
    draw_text(
        &mut img,
        ah_label,
        lx + swatch + 6,
        legend_y,
        legend_scale,
        HEADING,
    );

    // BZ
    let bz_x = lx + ah_w + legend_gap;
    fill_rect(&mut img, bz_x, legend_y + 1, swatch, swatch, bz_color);
    draw_text(
        &mut img,
        bz_label,
        bz_x + swatch + 6,
        legend_y,
        legend_scale,
        HEADING,
    );

    // Total
    let tot_x = bz_x + bz_w + legend_gap;
    // Dashed swatch for total
    for dx in (0..swatch).step_by(4) {
        fill_rect(
            &mut img,
            tot_x + dx,
            legend_y + 1,
            2.min(swatch - dx),
            swatch,
            total_color,
        );
    }
    draw_text(
        &mut img,
        total_label,
        tot_x + swatch + 6,
        legend_y,
        legend_scale,
        HEADING,
    );

    // ── Plot actual profit lines ──────────────────────────────
    // Merge timestamps from both series so we can compute a Total line.
    let mut all_times: Vec<u64> = Vec::new();
    for &(t, _) in ah_points {
        all_times.push(t);
    }
    for &(t, _) in bz_points {
        all_times.push(t);
    }
    all_times.sort_unstable();
    all_times.dedup();

    if all_times.len() >= 2 {
        // Step-interpolate helper: for a given `t`, find the last known value.
        fn interp(points: &[(u64, i64)], t: u64) -> i64 {
            let mut val: i64 = 0;
            for &(pt, pv) in points {
                if pt <= t {
                    val = pv;
                } else {
                    break;
                }
            }
            val
        }

        // Build interpolated series
        let ah_vals: Vec<i64> = all_times.iter().map(|&t| interp(ah_points, t)).collect();
        let bz_vals: Vec<i64> = all_times.iter().map(|&t| interp(bz_points, t)).collect();
        let total_vals: Vec<i64> = ah_vals
            .iter()
            .zip(bz_vals.iter())
            .map(|(&a, &b)| a + b)
            .collect();

        // Determine value range across all three series
        let all_vals = ah_vals
            .iter()
            .chain(bz_vals.iter())
            .chain(total_vals.iter());
        let min_val = *all_vals.clone().min().unwrap_or(&0);
        let max_val = *all_vals.max().unwrap_or(&0);
        let range = (max_val - min_val).max(1) as f64;

        let t_min = *all_times.first().unwrap();
        let t_max = *all_times.last().unwrap();
        let t_range = (t_max - t_min).max(1) as f64;

        let plot_left = ax_left + 4;
        let plot_right = ax_right - 4;
        let plot_top = ax_top + 6;
        let plot_bottom = ax_bottom - 4;
        let plot_w = plot_right.saturating_sub(plot_left).max(1) as f64;
        let plot_h = plot_bottom.saturating_sub(plot_top).max(1) as f64;

        // Map (time, value) → (px_x, px_y)
        let map_xy = |t: u64, v: i64| -> (u32, u32) {
            let x = plot_left as f64 + ((t - t_min) as f64 / t_range) * plot_w;
            let y = plot_bottom as f64 - ((v - min_val) as f64 / range) * plot_h;
            (x as u32, y as u32)
        };

        // Draw a zero-line if the range crosses zero
        if min_val < 0 && max_val > 0 {
            let zero_y_px = (plot_bottom as f64 - ((0 - min_val) as f64 / range) * plot_h) as u32;
            for px in (plot_left..plot_right).step_by(6) {
                fill_rect(&mut img, px, zero_y_px, 3, 1, Rgba([50, 55, 90, 255]));
            }
        }

        // Helper: draw a polyline between consecutive data points.
        let draw_line_series =
            |img: &mut RgbaImage, vals: &[i64], color: Rgba<u8>, thickness: u32, dashed: bool| {
                for i in 1..all_times.len() {
                    let (x0, y0) = map_xy(all_times[i - 1], vals[i - 1]);
                    let (x1, y1) = map_xy(all_times[i], vals[i]);
                    draw_line(img, x0, y0, x1, y1, color, thickness, dashed);
                }
            };

        // Draw AH (green), BZ (orange), Total (blue dashed) — back to front
        draw_line_series(&mut img, &total_vals, total_color, 2, true);
        draw_line_series(&mut img, &bz_vals, bz_color, 2, false);
        draw_line_series(&mut img, &ah_vals, ah_color, 2, false);
    } else {
        // Not enough data — draw a simple zero-line placeholder
        let zero_y = ax_top + (ax_bottom - ax_top) / 2;
        for px in (ax_left + 4..ax_right).step_by(6) {
            fill_rect(&mut img, px, zero_y, 3, 1, Rgba([50, 55, 90, 255]));
        }
    }

    // ── Footer ───────────────────────────────────────────────
    let footer = "Live stats at your control panel";
    draw_text_centered(&mut img, footer, h - 40, 2, TEXT_DIM);

    // ── Encode to PNG ────────────────────────────────────────
    let mut buf = Cursor::new(Vec::new());
    if let Err(e) = img.write_to(&mut buf, image::ImageFormat::Png) {
        tracing::error!("[OGImage] PNG encoding failed: {}", e);
        return Vec::new();
    }
    buf.into_inner()
}

fn format_og_value(val: f64) -> String {
    let abs = val.abs();
    if abs >= 1e9 {
        format!("{:.1}B", val / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", val / 1e6)
    } else if abs >= 1e3 {
        format!("{:.1}K", val / 1e3)
    } else {
        format!("{:.0}", val)
    }
}

fn format_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{}d {}h {}m", d, h, m)
    } else if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else {
        format!("{}m {}s", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn og_image_is_valid_png() {
        let ah = vec![(1000u64, 0i64), (2000, 500_000), (3000, 1_500_000)];
        let bz = vec![(1000u64, 0i64), (2500, 300_000), (3000, 600_000)];
        let bytes = generate_og_image(1_500_000, 250_000.0, 7265, &ah, &bz);
        // PNG magic bytes
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);
        assert!(bytes.len() > 100, "PNG should be non-trivial");
    }

    #[test]
    fn format_values() {
        assert_eq!(format_og_value(0.0), "0");
        assert_eq!(format_og_value(1_500_000.0), "1.5M");
        assert_eq!(format_og_value(2_300_000_000.0), "2.3B");
        assert_eq!(format_og_value(750.0), "750");
        assert_eq!(format_og_value(75_000.0), "75.0K");
    }

    #[test]
    fn format_uptime_values() {
        assert_eq!(format_uptime(65), "1m 5s");
        assert_eq!(format_uptime(3661), "1h 1m 1s");
        assert_eq!(format_uptime(90061), "1d 1h 1m");
    }

    #[test]
    fn text_width_calculation() {
        // Single char at scale 1: 5px
        assert_eq!(text_width("A", 1), 5);
        // Two chars at scale 1: 5 + 1 + 5 = 11
        assert_eq!(text_width("AB", 1), 11);
        // Empty string
        assert_eq!(text_width("", 1), 0);
    }

    #[test]
    #[ignore] // Only run manually to inspect the image
    fn save_og_image_to_disk() {
        let ah = vec![(1000u64, 0i64), (2000, 500_000), (3000, 1_500_000)];
        let bz = vec![(1000u64, 0i64), (2500, 300_000), (3000, 600_000)];
        let bytes = generate_og_image(1_500_000, 250_000.0, 7265, &ah, &bz);
        let path = std::env::temp_dir().join("og_image_test.png");
        std::fs::write(&path, &bytes).unwrap();
    }
}
