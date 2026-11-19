//! Shared popup components in the battery popup's card language.

use super::{Action, Hit, Ui};
use crate::canvas::{Color, FW_NORMAL, FW_SEMIBOLD};

/// A row of equal buttons, the chosen one filled with the accent: (label, chosen, action,
/// tooltip). A button without an action shows a state that cannot be chosen here (dimmed).
pub fn segmented(ui: &mut Ui, items: &[(String, bool, Option<Action>, String)]) {
    let n = items.len().max(1) as i32;
    let gap = ui.t.s(4.0);
    let w = (ui.w - gap * (n - 1)) / n;
    let h = ui.t.s(26.0);
    let r = ui.t.s(6.0) as f32;
    let f = ui.font(10.0, FW_NORMAL);
    let fb = ui.font(10.0, FW_SEMIBOLD);
    let (fill, stroke) = ui.t.style.card(ui.t.light);
    let stroke = stroke.unwrap_or(ui.t.dim.alpha(0.22));
    let top = ui.y;
    for (i, (label, on, action, tip)) in items.iter().enumerate() {
        let x = i as i32 * (w + gap);
        let fixed = action.is_none();
        if *on && fixed {
            // in force, but set elsewhere: outlined in the accent
            ui.c.fill_round_rect(x, top, w, h, r, ui.t.accent.alpha(0.18));
            ui.c.stroke_round_rect_r(x, top, w, h, r, ui.t.accent);
        } else if *on {
            ui.c.fill_round_rect(x, top, w, h, r, ui.t.accent);
        } else {
            ui.c.fill_round_rect(x, top, w, h, r, fill);
            ui.c.stroke_round_rect_r(x, top, w, h, r, stroke);
        }
        let font = if *on { fb } else { f };
        let (tw, th) = ui.c.measure_text(label, font);
        let tw = tw.min(w - ui.t.s(6.0));
        // text on the accent: black or white, whichever reads
        let a = ui.t.accent;
        let lum = (0.2126 * a.r as f32 + 0.7152 * a.g as f32 + 0.0722 * a.b as f32) / 255.0;
        let col = if *on && !fixed { if lum > 0.55 { Color::rgb(0, 0, 0) } else { Color::rgb(255, 255, 255) } } else if fixed { ui.t.dim } else { ui.t.text };
        ui.clipped(label, x + (w - tw) / 2, top + (h - th) / 2, w - ui.t.s(6.0), font, col);
        if let Some(a) = action {
            ui.hits.push(Hit { x, y: top, w, h, action: a.clone() });
        }
        if !tip.is_empty() {
            ui.tips.push(super::Tip { ax: None, x, y: top, w, h, font: ui.font(9.5, FW_NORMAL), text: tip.clone() });
        }
    }
    ui.y = top + h + ui.t.s(6.0);
}

#[cfg(test)]
mod style_tests {
    /// `cargo test style_previews -- --ignored`: the battery and CPU popups in every style,
    /// composited over a light-grey acrylic like the one the system material shows over a
    /// bright wallpaper, to target/previews/style_*.bmp.
    #[test]
    #[ignore]
    fn style_previews() {
        use super::super::{battery_view, draw_cpu, Ui};
        use crate::canvas::{Canvas, Color};
        use crate::metrics::power::{Cell, MeterChannel, MeterRole, Telemetry};
        use crate::style::{Style, STYLES};
        use crate::widgets::{ColorMode, Theme};
        use std::sync::Arc;
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("previews");
        std::fs::create_dir_all(&out).unwrap();
        crate::i18n::set_language("de");
        let mut m = crate::metrics::Metrics::new("C:", 8, &[], &[]);
        m.begin_details();
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            m.update(true);
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        m.data.power_tel = Some(Arc::new(Telemetry {
            at_unix: now,
            level: Some(0.92),
            on_ac: Some(true),
            cells: vec![Cell { name: "Battery".into(), manufacturer: "Example".into(), chemistry: "LIon".into(), capacity_mwh: Some(66_300), full_mwh: Some(71_916), design_mwh: Some(75_076), voltage_mv: Some(17_580), rate_mw: Some(16_640), cycles: Some(16), charging: true, ..Default::default() }],
            meters: vec![MeterChannel { name: "RAPL_Package0_PKG".into(), watts: 14.73, role: MeterRole::CpuPackage }],
            estimate: Some((19.4, Some(1.1))),
            ..Default::default()
        }));
        m.data.battery = Some(crate::metrics::battery::Battery { level: 0.92, charging: true, on_ac: true, remaining_secs: None });
        let mut sheet: Vec<Canvas> = Vec::new();
        for (value, _) in STYLES {
            let style = Style::from_config(value);
            for light in [false, true] {
                if light && style != Style::Windows {
                    continue;
                }
                let (text, dim) = if light { (Color::rgb(0x1B, 0x1B, 0x1B), Color::rgba(0x1B, 0x1B, 0x1B, style.dim_alpha(true))) } else { (Color::rgb(255, 255, 255), Color::rgba(255, 255, 255, style.dim_alpha(false))) };
                let mut modes = [ColorMode::Accent; crate::widgets::MODULE_COUNT];
                if style.mono() {
                    modes = [ColorMode::Mono; crate::widgets::MODULE_COUNT];
                } else if let Some(c) = style.module_colors(light) {
                    for i in 0..6 {
                        modes[i] = ColorMode::Fixed(c[i]);
                    }
                }
                let accent = if style.system_accent() { crate::util::system_accent_for(light).unwrap() } else { crate::widgets::named_color("blue").unwrap() };
                let theme = Theme {
                    text, dim, accent, outline: None, light, scale: 1.25, inner_h: 20, labels: true, font_scale: 1.0, split_pe: true, chart_w: 34.0, bar_w: 3.0,
                    modes, label: None, net_rx: true, net_tx: true, net_bits: false, net_upload_first: false, temp_f: false, animate: true, battery_hours: 6, style, battery_view: Default::default(),
                };
                for kind in 0..2 {
                    let w = theme.s(if kind == 0 { 400.0 } else { 300.0 } - 28.0);
                    let mut c = Canvas::new();
                    c.resize(w, theme.s(1000.0));
                    c.clear();
                    let mut icons = crate::procicon::IconCache::default();
                    let (mut locks, mut frozen) = (Vec::new(), None);
                    let expanded = std::collections::HashSet::new();
                    let mut ui = Ui { c: &mut c, t: &theme, w, y: 0, hover: None, pie: None, tips: Vec::new(), hits: Vec::new(), icons: &mut icons, indent: 0, locks: &mut locks, frozen: &mut frozen, expanded: &expanded, proc_filter: (true, true), mem_bar: false, gpu_by_vram: false, charts: Vec::new(), last_chart: (0, 0, 0, 0), ages: (0.0, 1.0), mouse: None, copied: None, anim: None, anim_bg: Vec::new(), phase: 0.4, max_rows: None, flipped: Box::leak(Box::new(std::collections::HashSet::new())), fold_open: false, fold: Vec::new(), order: &[], drag: None, frows: Vec::new() };
                    if kind == 0 {
                        battery_view::draw(&mut ui, &m.data);
                    } else {
                        draw_cpu(&mut ui, &m.data);
                    }
                    let used = ui.y.min(theme.s(560.0));
                    let pad = theme.s(14.0);
                    let mut card = Canvas::new();
                    card.resize(w + pad * 2, used + pad * 2);
                    card.clear();
                    // the system material as in the screenshot, then the style's tint
                    let acrylic = if light { Color::rgb(0xE4, 0xE0, 0xDE) } else { Color::rgb(0x58, 0x56, 0x56) };
                    card.fill_rect(0, 0, w + pad * 2, used + pad * 2, acrylic);
                    if let Some((tint, a)) = style.popup_fill(light) {
                        card.fill_rect(0, 0, w + pad * 2, used + pad * 2, tint.alpha(a));
                    }
                    card.blit_rect(&c, 0, 0, w, used, pad, pad);
                    sheet.push(card);
                }
            }
        }
        // side by side: battery and CPU per style
        let gap = 16;
        let width: i32 = sheet.iter().map(|c| c.w + gap).sum();
        let height = sheet.iter().map(|c| c.h).max().unwrap();
        let mut all = Canvas::new();
        all.resize(width, height);
        all.clear();
        let mut x = 0;
        for c in &sheet {
            all.blit_rect(c, 0, 0, c.w, c.h, x, 0);
            x += c.w + gap;
        }
        all.save_bmp(&out.join("style_sheet.bmp"), Color::rgb(0x6B, 0x14, 0x10)).unwrap();
        crate::i18n::set_language("en");
    }
}
