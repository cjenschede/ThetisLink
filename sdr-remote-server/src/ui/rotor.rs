use egui::Color32;

pub(super) fn render_rotor_panel(
    ui: &mut egui::Ui,
    rotor: &crate::rotor::Rotor,
    status: &crate::rotor::RotorStatus,
    goto_input: &mut String,
) {
    ui.horizontal(|ui| {
        ui.heading("Rotor");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if status.connected {
                ui.colored_label(Color32::GREEN, "Online");
            } else {
                ui.colored_label(Color32::RED, "Offline");
            }
        });
    });
    ui.separator();

    let angle_deg = status.angle_x10 as f32 / 10.0;
    let target_deg = if status.rotating { Some(status.target_x10 as f32 / 10.0) } else { None };

    // Compass circle — click to GoTo
    if let Some(goto) = render_compass(ui, angle_deg, target_deg, status.connected) {
        rotor.send_command(crate::rotor::RotorCmd::GoTo(goto));
    }

    ui.add_space(4.0);

    // Stop button
    ui.horizontal(|ui| {
        if ui.add_enabled(status.connected, egui::Button::new("STOP").min_size(egui::vec2(70.0, 30.0))).clicked() {
            rotor.send_command(crate::rotor::RotorCmd::Stop);
        }

        // GoTo text input
        ui.label("GoTo:");
        let resp = ui.add(egui::TextEdit::singleline(goto_input).desired_width(60.0));
        if (ui.add_enabled(status.connected, egui::Button::new("Go")).clicked()
            || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
            && status.connected
        {
            if let Ok(deg) = goto_input.trim().parse::<f32>() {
                let angle_x10 = (deg * 10.0).round() as u16;
                if angle_x10 <= 3600 {
                    rotor.send_command(crate::rotor::RotorCmd::GoTo(angle_x10));
                }
            }
        }
    });
}

/// Draw a clickable compass circle. Returns Some(angle_x10) if the user clicked a position.
fn render_compass(ui: &mut egui::Ui, angle_deg: f32, target_deg: Option<f32>, connected: bool) -> Option<u16> {
    let avail = ui.available_width().min(ui.available_height()).min(300.0).max(80.0);
    let size = avail;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let radius = size * 0.45;

    let bg = ui.visuals().extreme_bg_color;
    let ring_color = ui.visuals().text_color().gamma_multiply(0.3);
    let text_color = ui.visuals().text_color().gamma_multiply(0.6);
    let needle_color = Color32::from_rgb(50, 200, 50);
    let target_color = Color32::from_rgb(255, 200, 40);

    // Background circle
    painter.circle_filled(center, radius + 2.0, bg);
    painter.circle_stroke(center, radius, egui::Stroke::new(1.5, ring_color));

    // Tick marks and labels
    let labels: [(&str, f32); 4] = [("N", 0.0), ("E", 90.0), ("S", 180.0), ("W", 270.0)];
    for (label, deg) in labels {
        let rad = (deg - 90.0).to_radians();
        let outer = center + egui::vec2(rad.cos(), rad.sin()) * radius;
        let inner = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 8.0);
        painter.line_segment([inner, outer], egui::Stroke::new(1.0, ring_color));

        let text_pos = center + egui::vec2(rad.cos(), rad.sin()) * (radius + 12.0);
        painter.text(
            text_pos,
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            if label == "N" { Color32::from_rgb(255, 80, 80) } else { text_color },
        );
    }

    // Minor ticks every 30°
    for i in 0..12 {
        let deg = i as f32 * 30.0;
        if deg % 90.0 == 0.0 { continue; }
        let rad = (deg - 90.0).to_radians();
        let outer = center + egui::vec2(rad.cos(), rad.sin()) * radius;
        let inner = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 5.0);
        painter.line_segment([inner, outer], egui::Stroke::new(0.5, ring_color));
    }

    // Target line (dashed feel — draw shorter line)
    if let Some(tgt) = target_deg {
        let rad = (tgt - 90.0).to_radians();
        let tip = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 10.0);
        let mid = center + egui::vec2(rad.cos(), rad.sin()) * (radius * 0.3);
        painter.line_segment([mid, tip], egui::Stroke::new(2.0, target_color));
    }

    // Current angle needle
    let rad = (angle_deg - 90.0).to_radians();
    let tip = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 4.0);
    painter.line_segment([center, tip], egui::Stroke::new(2.5, needle_color));
    painter.circle_filled(center, 4.0, needle_color);

    // Angle text in center
    painter.text(
        center + egui::vec2(0.0, radius * 0.55),
        egui::Align2::CENTER_CENTER,
        format!("{:.1}\u{00B0}", angle_deg),
        egui::FontId::proportional(18.0),
        ui.visuals().text_color(),
    );

    // Handle click — calculate angle from click position
    if connected && response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let dx = pos.x - center.x;
            let dy = pos.y - center.y;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > 10.0 {
                // atan2 gives angle from positive X axis, we want from North (up)
                let mut deg = dy.atan2(dx).to_degrees() + 90.0;
                if deg < 0.0 { deg += 360.0; }
                if deg >= 360.0 { deg -= 360.0; }
                let angle_x10 = (deg * 10.0).round() as u16;
                return Some(angle_x10);
            }
        }
    }

    None
}
