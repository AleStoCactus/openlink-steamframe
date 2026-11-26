//! The desktop window.

use crate::config::{CodecPreference, ConnectionMode, Settings};
use crate::encoder::{Backend, BackendPreference};
use crate::session::Status;
use eframe::egui;
use openlink_protocol::Codec;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn run(status: Arc<Mutex<Status>>, settings: Arc<Mutex<Settings>>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 620.0])
            .with_title("OpenLink"),
        ..Default::default()
    };
    eframe::run_native(
        "OpenLink",
        options,
        Box::new(move |_cc| {
            let draft = settings.lock().unwrap().clone();
            Ok(Box::new(App {
                status,
                settings,
                draft,
                save_error: None,
                meters: Meters::default(),
            }))
        }),
    )
}

struct App {
    status: Arc<Mutex<Status>>,
    settings: Arc<Mutex<Settings>>,
    /// Edited copy; applied on "Save".
    draft: Settings,
    save_error: Option<String>,
    meters: Meters,
}

/// What the audio meters show: peaks that fall back slowly, like a hardware meter, so a short
/// sound stays visible.
#[derive(Default)]
struct Meters {
    speakers: f32,
    microphone: f32,
    last: Option<std::time::Instant>,
}

impl Meters {
    /// Folds in the peaks reported since the last frame.
    fn update(&mut self, (speakers, microphone): (f32, f32)) {
        let now = std::time::Instant::now();
        let dt = self.last.map_or(0.0, |t| (now - t).as_secs_f32());
        self.last = Some(now);
        // Fall about 30 dB per second.
        let fall = 10f32.powf(-1.5 * dt);
        self.speakers = speakers.max(self.speakers * fall);
        self.microphone = microphone.max(self.microphone * fall);
    }
}

impl App {
    fn status_ui(&self, ui: &mut egui::Ui) {
        let st = self.status.lock().unwrap().clone();
        ui.heading("Status");
        egui::Grid::new("status")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label("SteamVR driver");
                dot(
                    ui,
                    st.driver_connected,
                    if st.driver_connected {
                        "connected"
                    } else {
                        "not running"
                    },
                );
                ui.end_row();

                ui.label("Headset");
                match &st.headset {
                    Some(h) => dot(ui, true, &format!("{} ({})", h.info.model, h.addr.ip())),
                    None => dot(ui, false, "waiting for headset"),
                }
                ui.end_row();

                if let Some(a) = &st.adapter {
                    ui.label("Steam Frame adapter");
                    match a.ipv4 {
                        Some((ip, prefix)) => {
                            let mut text = format!("{} {ip}/{prefix}", a.interface);
                            if let Some(ssid) = &a.ssid {
                                text += &format!(", {ssid}");
                            }
                            if let Some(tx) = a.tx_mbps {
                                text += &format!(", {tx:.0} Mbit/s");
                            }
                            if let Some(dbm) = a.signal_dbm {
                                text += &format!(", {dbm} dBm");
                            }
                            dot(ui, true, &text)
                        }
                        None => dot(
                            ui,
                            false,
                            a.problem.as_deref().unwrap_or("plugged in, not connected to the headset"),
                        ),
                    }
                    ui.end_row();
                }

                if let Some(h) = &st.headset {
                    ui.label("Clock sync");
                    ui.label(match (h.clock_offset_ns, h.min_rtt_ns) {
                        (Some(_), Some(rtt)) => {
                            format!("locked, min RTT {:.2} ms", rtt as f64 * 1e-6)
                        }
                        _ => "syncing…".into(),
                    });
                    ui.end_row();
                    ui.label("Tracking");
                    ui.label(format!(
                        "{:.0} Hz, {:.1} ms old on arrival",
                        h.tracking_hz, h.sample_age_ms
                    ));
                    ui.end_row();
                    ui.label("Video");
                    let (pw, ph) = h.info.panel_size();
                    let (rw, rh) = h.info.render_size((h.stream.eye_width, h.stream.eye_height));
                    ui.label(format!(
                        "{}×{} per eye{} @ {} Hz (panel {pw}×{ph}, SteamVR 100% {rw}×{rh})",
                        h.stream.eye_width,
                        h.stream.eye_height,
                        match h.stream.foveation {
                            Some(_) => {
                                let (cw, ch) = h.stream.coded_eye_size();
                                format!(", foveated to {cw}×{ch}")
                            }
                            None => " encoded".into(),
                        },
                        h.stream.refresh_rate,
                    ));
                    ui.end_row();
                    ui.label("Audio");
                    let a = &st.audio;
                    let text = match (a.speakers, a.microphone) {
                        (true, true) => "OpenLink output and microphone ready",
                        (true, false) => "OpenLink output ready",
                        (false, true) => "OpenLink microphone ready",
                        (false, false) => "off",
                    };
                    dot(ui, a.speakers || a.microphone, text);
                    ui.end_row();
                }
            });
        for e in [&st.last_error, &st.audio.error].into_iter().flatten() {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let (audio, headset) = {
            let st = self.status.lock().unwrap();
            (st.audio.clone(), st.headset.as_ref().map(|h| h.info.clone()))
        };
        self.meters.update(audio.levels.take());
        let d = &mut self.draft;
        ui.heading("Video");
        egui::Grid::new("video")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label("Codec");
                egui::ComboBox::from_id_salt("codec")
                    .selected_text(codec_label(d.codec))
                    .show_ui(ui, |ui| {
                        for c in [
                            CodecPreference::Auto,
                            CodecPreference::Fixed(Codec::Av1),
                            CodecPreference::Fixed(Codec::H265),
                            CodecPreference::Fixed(Codec::H264),
                        ] {
                            ui.selectable_value(&mut d.codec, c, codec_label(c));
                        }
                    });
                ui.end_row();

                ui.label("Encoder");
                let enc_label = |p: BackendPreference| match p {
                    BackendPreference::Auto => "Auto".to_string(),
                    BackendPreference::Fixed(b) => b.name().to_string(),
                };
                egui::ComboBox::from_id_salt("encoder")
                    .selected_text(enc_label(d.encoder))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.encoder, BackendPreference::Auto, "Auto");
                        for b in Backend::ALL
                            .into_iter()
                            .filter(|b| b.supported_on_this_os())
                        {
                            ui.selectable_value(
                                &mut d.encoder,
                                BackendPreference::Fixed(b),
                                b.name(),
                            );
                        }
                    });
                ui.end_row();

                ui.label("Bitrate");
                ui.add(
                    egui::Slider::new(&mut d.bitrate_mbps, 20..=1000)
                        .suffix(" Mbps")
                        .logarithmic(true),
                );
                ui.end_row();

                ui.label("Encoded Resolution");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut d.resolution_percent, 50..=150)
                            .step_by(5.0)
                            .suffix("%"),
                    )
                    .on_hover_text(
                        "Streamed resolution, relative to the headset's panel. SteamVR's 100% \
                         is this size enlarged for the headset's lens, so it matches the stream's \
                         sharpness at the center of view (as with other headsets, it is larger \
                         than the panel); change the render resolution in SteamVR's settings. \
                         Saving a new value while a headset is connected restarts \
                         SteamVR, which only picks up a new size when it starts.",
                    );
                    match &headset {
                        Some(info) => {
                            let (w, h) = info.encoded_size(d.resolution_percent);
                            let (rw, rh) = info.render_size((w, h));
                            ui.label(format!("{w} × {h} per eye (SteamVR 100%: {rw} × {rh})"));
                        }
                        None => {
                            ui.weak("connect a headset to see the size");
                        }
                    }
                });
                ui.end_row();

                ui.label("Foveated encoding");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut d.foveation_percent, 20..=100)
                            .step_by(5.0)
                            // The suffix would also follow "off", so the formatter adds the %.
                            .custom_formatter(|v, _| if v >= 100.0 { "off".into() } else { format!("{v:.0}%") })
                            .custom_parser(|s| {
                                let s = s.trim();
                                if s.eq_ignore_ascii_case("off") {
                                    Some(100.0)
                                } else {
                                    s.trim_end_matches('%').trim().parse().ok()
                                }
                            }),
                    )
                    .on_hover_text(
                        "How much of each eye's width and height, around the center of the lens, \
                         is encoded at full resolution. Outside it, resolution falls off smoothly \
                         to a quarter at the very edges, where the lens blurs anyway: the encoder \
                         has fewer pixels to code and spends the bitrate on the center. Where the \
                         GPU supports quantization maps, the encoder also gives fewer bits to a \
                         ring around the lens center, falling off toward the edges. Lower saves \
                         more. 100% turns it off. Applies live, without restarting SteamVR.",
                    );
                    if let Some(info) = &headset {
                        let full = info.encoded_size(d.resolution_percent);
                        let fraction = d.foveation_percent as f32 / 100.0;
                        match openlink_protocol::foveation::Foveation::new(fraction, &info.fov, full) {
                            Some(f) => {
                                let (w, h) = f.coded_eye_size();
                                let pct = 100.0 * (w * h) as f32 / (full.0 * full.1) as f32;
                                ui.label(format!("{w} × {h} per eye encoded ({pct:.0}% of the pixels)"));
                            }
                            None => {
                                ui.weak("whole picture at full resolution");
                            }
                        }
                    }
                });
                ui.end_row();

                ui.label("Refresh rate");
                ui.horizontal(|ui| {
                    let high = headset.as_ref().map_or(&[][..], |h| h.high_refresh_rates());
                    egui::ComboBox::from_id_salt("hz")
                        .selected_text(format!("{} Hz", d.refresh_rate))
                        .show_ui(ui, |ui| {
                            for &hz in [72.0, 80.0, 90.0, 120.0].iter().chain(high) {
                                ui.selectable_value(&mut d.refresh_rate, hz, format!("{hz} Hz"));
                            }
                        })
                        .response
                        .on_hover_text(
                            "Quest 3 on Horizon OS v2.7 or later also runs at 144, 165 and 207 Hz, \
                             and at 240 Hz in developer mode with display scaling on. The stream \
                             uses the closest rate the headset currently accepts.",
                        );
                    // The session streams at the closest rate the headset's runtime lists; say so
                    // when that is not the one picked (e.g. 240 Hz outside developer mode).
                    if let Some(h) = &headset {
                        let closest = h
                            .refresh_rates
                            .iter()
                            .copied()
                            .min_by(|a, b| (a - d.refresh_rate).abs().total_cmp(&(b - d.refresh_rate).abs()));
                        if let Some(hz) = closest.filter(|hz| (hz - d.refresh_rate).abs() > 0.5) {
                            ui.weak(format!("headset does not offer it now; streams at {hz} Hz"));
                        }
                    }
                });
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.heading("Audio");
        egui::Grid::new("audio")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.checkbox(&mut d.speakers, "Headset speakers").on_hover_text(
                    "Adds an \"OpenLink\" output device; what plays on it is heard in the headset",
                );
                meter(ui, self.meters.speakers, audio.speakers)
                    .on_hover_text("Sound being sent to the headset");
                ui.end_row();
                ui.checkbox(&mut d.microphone, "Headset microphone").on_hover_text(
                    "Adds an \"OpenLink\" input device carrying the headset's microphone",
                );
                meter(ui, self.meters.microphone, audio.microphone)
                    .on_hover_text("Sound arriving from the headset's microphone");
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.heading("Connection");
        ui.horizontal(|ui| {
            ui.radio_value(&mut d.connection, ConnectionMode::WiFi, "Wi-Fi");
            ui.radio_value(&mut d.connection, ConnectionMode::Usb, "USB cable");
            ui.radio_value(&mut d.connection, ConnectionMode::SteamFrameAdapter, "Steam Frame adapter");
        });
        if d.connection == ConnectionMode::SteamFrameAdapter {
            ui.weak("Direct 6 GHz link through the Steam Frame's USB wireless adapter. OpenLink joins the headset's network with the profile Steam made when pairing it.");
        }

        ui.add_space(12.0);
        let dirty = *self.settings.lock().unwrap() != self.draft;
        ui.horizontal(|ui| {
            if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                *self.settings.lock().unwrap() = self.draft.clone();
                self.save_error = self
                    .draft
                    .save()
                    .err()
                    .map(|e| format!("could not save settings: {e}"));
            }
            if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                self.draft = self.settings.lock().unwrap().clone();
            }
            ui.weak(
                "Resolution, foveation, bitrate and refresh rate apply to a connected headset right away; \
                 the rest on its next connection.",
            );
        });
        if let Some(e) = &self.save_error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            self.status_ui(ui);
            ui.separator();
            self.settings_ui(ui);
        });
        // Status comes from other threads; poll at a gentle rate, or smoothly enough for the
        // audio meters while they can move.
        let audio = self.status.lock().unwrap().audio.clone();
        let fast = audio.speakers || audio.microphone;
        ctx.request_repaint_after(Duration::from_millis(if fast { 33 } else { 250 }));
    }
}

fn dot(ui: &mut egui::Ui, ok: bool, text: &str) {
    let color = if ok {
        egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
    } else {
        ui.visuals().weak_text_color()
    };
    ui.horizontal(|ui| {
        ui.colored_label(color, "●");
        ui.label(text);
    });
}

/// A horizontal level meter for a peak `level` (0..1), on a -60..0 dBFS scale. Empty when the
/// device does not exist (`active` false).
fn meter(ui: &mut egui::Ui, level: f32, active: bool) -> egui::Response {
    const FLOOR_DB: f32 = -60.0;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(160.0, 10.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    if active && level > 0.0 {
        let db = 20.0 * level.log10();
        let frac = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
        let color = if db > -1.0 {
            egui::Color32::from_rgb(0xe5, 0x48, 0x4d)
        } else if db > -12.0 {
            egui::Color32::from_rgb(0xe3, 0xb3, 0x41)
        } else {
            egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
        };
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        painter.rect_filled(fill, 2.0, color);
    }
    response
}

fn codec_label(c: CodecPreference) -> &'static str {
    match c {
        CodecPreference::Auto => "Auto (best available)",
        CodecPreference::Fixed(Codec::Av1) => "AV1",
        CodecPreference::Fixed(Codec::H265) => "HEVC (H.265)",
        CodecPreference::Fixed(Codec::H264) => "H.264",
    }
}
