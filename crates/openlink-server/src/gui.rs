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
                    ui.label("Panel");
                    ui.label(format!(
                        "{}×{} per eye @ {} Hz",
                        h.info.eye_width, h.info.eye_height, h.info.current_refresh_rate
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

                ui.label("Resolution scale");
                ui.add(egui::Slider::new(&mut d.resolution_scale, 0.5..=1.5).step_by(0.05));
                ui.end_row();

                ui.label("Refresh rate");
                egui::ComboBox::from_id_salt("hz")
                    .selected_text(format!("{} Hz", d.refresh_rate))
                    .show_ui(ui, |ui| {
                        for hz in [72.0, 80.0, 90.0, 120.0] {
                            ui.selectable_value(&mut d.refresh_rate, hz, format!("{hz} Hz"));
                        }
                    });
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.heading("Audio");
        ui.checkbox(&mut d.speakers, "Headset speakers")
            .on_hover_text("Adds an \"OpenLink\" output device; what plays on it is heard in the headset");
        ui.checkbox(&mut d.microphone, "Headset microphone")
            .on_hover_text("Adds an \"OpenLink\" input device carrying the headset's microphone");

        ui.add_space(12.0);
        ui.heading("Connection");
        ui.horizontal(|ui| {
            ui.radio_value(&mut d.connection, ConnectionMode::WiFi, "Wi-Fi");
            ui.radio_value(&mut d.connection, ConnectionMode::Usb, "USB cable");
        });

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
            ui.weak("Changes apply on the next headset connection.");
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
        // Status comes from other threads; poll at a gentle rate.
        ctx.request_repaint_after(Duration::from_millis(250));
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

fn codec_label(c: CodecPreference) -> &'static str {
    match c {
        CodecPreference::Auto => "Auto (best available)",
        CodecPreference::Fixed(Codec::Av1) => "AV1",
        CodecPreference::Fixed(Codec::H265) => "HEVC (H.265)",
        CodecPreference::Fixed(Codec::H264) => "H.264",
    }
}
