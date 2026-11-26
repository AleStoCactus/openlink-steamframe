//! The desktop window, laid out as a dark dashboard: the headset and its stream in the middle,
//! the settings in tabs on the right, live link, stream and timing figures along the bottom.
//!
//! egui's default proportional font has no symbols or emoji (a "●" in a label shows as a box
//! and logs a warning), so status dots, icons and gauges are painted rather than typed.

use crate::config::{CodecPreference, ConnectionMode, Settings};
use crate::encoder::{Backend, BackendPreference};
use crate::session::Status;
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Button, Color32, CornerRadius, FontId, Frame, Layout, Margin, Painter,
    Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui,
};
use openlink_protocol::{Codec, HeadsetInfo};
use std::collections::HashSet;
use std::f32::consts::{PI, TAU};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Colours of the window: a deep navy base, a coral accent for what is switched on, teal for
/// healthy live figures.
mod palette {
    use eframe::egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x18);
    pub const PANEL: Color32 = Color32::from_rgb(0x13, 0x14, 0x24);
    pub const CARD: Color32 = Color32::from_rgb(0x19, 0x1a, 0x2d);
    pub const INSET: Color32 = Color32::from_rgb(0x0f, 0x10, 0x1d);
    pub const CHIP: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x31);
    pub const LINE: Color32 = Color32::from_rgb(0x25, 0x27, 0x3d);
    pub const LINE_STRONG: Color32 = Color32::from_rgb(0x33, 0x36, 0x55);
    pub const TAB_SELECTED: Color32 = Color32::from_rgb(0x24, 0x26, 0x3e);
    pub const TOGGLE_OFF: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x46);
    pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe9, 0xf3);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0x9d, 0xb8);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x63, 0x66, 0x86);
    pub const ACCENT: Color32 = Color32::from_rgb(0xff, 0x4d, 0x6d);
    pub const TEAL: Color32 = Color32::from_rgb(0x2e, 0xd3, 0xb7);
    pub const GREEN: Color32 = Color32::from_rgb(0x3f, 0xc8, 0x6e);
    pub const AMBER: Color32 = Color32::from_rgb(0xf5, 0xb2, 0x3e);
    pub const RED: Color32 = Color32::from_rgb(0xef, 0x4b, 0x52);
    pub const BLUE: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
    pub const VIOLET: Color32 = Color32::from_rgb(0xa0, 0x6b, 0xff);
    pub const PINK: Color32 = Color32::from_rgb(0xff, 0x5c, 0xa8);
    pub const BADGE_BG: Color32 = Color32::from_rgb(0x28, 0x23, 0x4d);
    pub const BADGE_FG: Color32 = Color32::from_rgb(0xbf, 0xae, 0xff);
}
use palette::*;

const RESOLUTION_HELP: &str = "Streamed resolution, relative to the headset's panel. SteamVR's 100% is \
     this size enlarged for the headset's lens, so it matches the stream's sharpness at the center of view \
     (as with other headsets, it is larger than the panel); change the render resolution in SteamVR's \
     settings. Saving a new value while a headset is connected restarts SteamVR, which only picks up a new \
     size when it starts.";
const FOVEATION_HELP: &str = "How much of each eye's width and height, around the center of the lens, is \
     encoded at full resolution. Outside it, resolution falls off smoothly to a quarter at the very edges, \
     where the lens blurs anyway: the encoder has fewer pixels to code and spends the bitrate on the center. \
     Where the GPU supports quantization maps, the encoder also gives fewer bits to a ring around the lens \
     center, falling off toward the edges. Lower saves more. Applies live, without restarting SteamVR.";
const REFRESH_HELP: &str = "Quest 3 on Horizon OS v2.7 or later also runs at 144, 165 and 207 Hz, and at \
     240 Hz in developer mode with display scaling on. The stream uses the closest rate the headset \
     currently accepts.";
const LAYER_HELP: &str = "Draws SteamVR's own layer (dashboard, laser, controller models) over the game. \
     Turn it off where SteamVR leaves textures of that layer undrawn and they cover the game in black; the \
     dashboard opened over a game then does not show in the headset.";

pub fn run(status: Arc<Mutex<Status>>, settings: Arc<Mutex<Settings>>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1000.0, 660.0])
            .with_title("OpenLink"),
        ..Default::default()
    };
    eframe::run_native(
        "OpenLink",
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(App::new(status, settings)))
        }),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Quality,
    Display,
    Audio,
    Connection,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Quality, Tab::Display, Tab::Audio, Tab::Connection];

    fn label(self) -> &'static str {
        match self {
            Tab::Quality => "Quality",
            Tab::Display => "Display",
            Tab::Audio => "Audio",
            Tab::Connection => "Connection",
        }
    }
}

struct App {
    status: Arc<Mutex<Status>>,
    settings: Arc<Mutex<Settings>>,
    /// Edited copy; applied on "Save".
    draft: Settings,
    save_error: Option<String>,
    meters: Meters,
    tab: Tab,
    /// Titles of the setting cards shown expanded.
    open: HashSet<&'static str>,
    /// The foveation band to go back to when its switch is turned on again.
    foveation_band: u32,
}

impl App {
    fn new(status: Arc<Mutex<Status>>, settings: Arc<Mutex<Settings>>) -> Self {
        let draft = settings.lock().unwrap().clone();
        let foveation_band = if draft.foveation_percent < 100 { draft.foveation_percent } else { 50 };
        let open = [
            "Bitrate",
            "Encoded resolution",
            "Foveated encoding",
            "Refresh rate",
            "SteamVR layer over games",
            "Headset speakers",
            "Headset microphone",
            "Connection",
            "Steam Frame adapter",
        ]
        .into_iter()
        .collect();
        Self {
            status,
            settings,
            draft,
            save_error: None,
            meters: Meters::default(),
            tab: Tab::Quality,
            open,
            foveation_band,
        }
    }
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let st = self.status.lock().unwrap().clone();
        self.meters.update(st.audio.levels.take());
        paint_backdrop(ui);

        egui::Panel::top("top_bar")
            .exact_size(56.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin::symmetric(14, 0)))
            .show(ui, |ui| self.top_bar(ui, &st));
        // Panels get transparent frames with inner margins for the gaps, and draw their card
        // inside: margins outside a panel's frame do not keep the panels beside it away. Their
        // contents must fit: egui gives a bottom panel whose contents overflow less room at its
        // top, and the middle then reaches under it.
        let side_width = (ui.available_width() * 0.34).clamp(350.0, 422.0);
        egui::Panel::right("settings")
            .exact_size(side_width)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 0, right: 12, top: 0, bottom: 12 }))
            .show(ui, |ui| {
                panel_frame().inner_margin(Margin::same(12)).show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    self.settings_panel(ui, &st);
                });
            });
        egui::Panel::bottom("figures")
            .exact_size(218.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 12, right: 10, top: 10, bottom: 12 }))
            .show(ui, |ui| figures(ui, &st));
        egui::CentralPanel::default()
            .frame(Frame::new().inner_margin(Margin { left: 12, right: 10, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                panel_frame().show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    hero(ui, &st);
                });
            });

        // Status comes from other threads; poll at a gentle rate, or smoothly enough for the
        // audio meters while they can move.
        let fast = st.audio.speakers || st.audio.microphone;
        ui.ctx().request_repaint_after(Duration::from_millis(if fast { 33 } else { 250 }));
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut Ui, st: &Status) {
        ui.horizontal_centered(|ui| {
            logo(ui);
            ui.label(RichText::new("OpenLink").size(16.0).color(TEXT));
            ui.add_space(10.0);
            let mode = self.draft.connection;
            if chip(ui, connection_icon(mode), connection_label(mode))
                .on_hover_text("Connection settings")
                .clicked()
            {
                self.tab = Tab::Connection;
            }
            let size = match &st.headset {
                Some(h) => format!("{} × {}", h.stream.eye_width, h.stream.eye_height),
                None => format!("{}%", self.draft.resolution_percent),
            };
            badge(ui, &size);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let a = &st.audio;
                status_pill(ui, "Audio", a.speakers || a.microphone);
                status_pill(ui, "Headset", st.headset.is_some());
                status_pill(ui, "SteamVR", st.driver_connected);
            });
        });
    }

    fn settings_panel(&mut self, ui: &mut Ui, st: &Status) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let width = (ui.available_width() - 3.0 * 4.0) / 4.0;
            for tab in Tab::ALL {
                let selected = self.tab == tab;
                let text = RichText::new(tab.label()).size(13.0).color(if selected { TEXT } else { TEXT_DIM });
                let mut button = Button::new(text)
                    .fill(TAB_SELECTED)
                    .frame_when_inactive(selected)
                    .corner_radius(CornerRadius::same(8))
                    .min_size(vec2(width, 30.0));
                if selected {
                    button = button.stroke(Stroke::new(1.0, LINE_STRONG));
                }
                if ui.add(button).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(6.0);
        egui::Panel::bottom("settings_footer")
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().inner_margin(Margin { left: 2, right: 2, top: 10, bottom: 0 }))
            .show(ui, |ui| self.footer(ui));
        egui::CentralPanel::default().frame(Frame::new()).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                match self.tab {
                    Tab::Quality => self.quality_tab(ui, st),
                    Tab::Display => self.display_tab(ui, st),
                    Tab::Audio => self.audio_tab(ui, st),
                    Tab::Connection => self.connection_tab(ui, st),
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut Ui) {
        let dirty = *self.settings.lock().unwrap() != self.draft;
        ui.horizontal(|ui| {
            dot(ui, if dirty { AMBER } else { GREEN });
            let text = if dirty { "Unsaved changes" } else { "All changes saved" };
            ui.label(RichText::new(text).size(12.5).color(TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let save = Button::new(RichText::new("Save").color(Color32::WHITE))
                    .fill(ACCENT)
                    .corner_radius(CornerRadius::same(8))
                    .min_size(vec2(78.0, 30.0));
                if ui.add_enabled(dirty, save).clicked() {
                    *self.settings.lock().unwrap() = self.draft.clone();
                    self.save_error = self.draft.save().err().map(|e| format!("could not save settings: {e}"));
                }
                let revert = Button::new(RichText::new("Revert").color(TEXT_DIM))
                    .frame_when_inactive(false)
                    .corner_radius(CornerRadius::same(8))
                    .min_size(vec2(0.0, 30.0));
                if ui.add_enabled(dirty, revert).clicked() {
                    self.draft = self.settings.lock().unwrap().clone();
                }
            });
        });
        help(
            ui,
            "Resolution, foveation, bitrate, refresh rate and SteamVR's layer apply to a connected headset \
             right away; the rest on its next connection.",
        );
        if let Some(e) = &self.save_error {
            note(ui, RED, e);
        }
    }

    fn quality_tab(&mut self, ui: &mut Ui, st: &Status) {
        let headset = st.headset.as_ref().map(|h| &h.info);
        let streaming = st.headset.as_ref().map(|h| h.stream.codec);
        let Self { draft: d, open, foveation_band, .. } = self;
        pipeline(ui, d, st);

        let subtitle = match streaming {
            Some(c) => format!("{} · streaming {}", codec_label(d.codec), codec_name(c)),
            None => codec_label(d.codec).into(),
        };
        card(open, ui, Head::new("Codec", subtitle, Icon::Film, BLUE), None, |ui| {
            segmented(
                ui,
                &mut d.codec,
                &[
                    (CodecPreference::Auto, "Auto"),
                    (CodecPreference::Fixed(Codec::Av1), "AV1"),
                    (CodecPreference::Fixed(Codec::H265), "HEVC"),
                    (CodecPreference::Fixed(Codec::H264), "H.264"),
                ],
            );
            help(
                ui,
                "Auto picks the best codec both the GPU encoder and the headset's decoder support: AV1, then \
                 HEVC, then H.264. Applies on the next connection.",
            );
        });

        card(open, ui, Head::new("Encoder", encoder_label(d.encoder), Icon::Chip, VIOLET), None, |ui| {
            row(ui, "Backend", |ui| {
                egui::ComboBox::from_id_salt("encoder")
                    .selected_text(encoder_label(d.encoder))
                    .width(190.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.encoder, BackendPreference::Auto, "Auto");
                        for b in Backend::ALL.into_iter().filter(|b| b.supported_on_this_os()) {
                            ui.selectable_value(&mut d.encoder, BackendPreference::Fixed(b), b.name());
                        }
                    });
            });
            help(ui, "Applies on the next connection.");
        });

        let subtitle = format!("{} Mbit/s", d.bitrate_mbps);
        card(open, ui, Head::new("Bitrate", subtitle, Icon::Wave, AMBER), None, |ui| {
            wide_slider(ui, egui::Slider::new(&mut d.bitrate_mbps, 20..=1000).suffix(" Mbps").logarithmic(true));
            help(ui, "Applies to a connected headset right away.");
        });

        let subtitle = match headset {
            Some(info) => {
                let (w, h) = info.encoded_size(d.resolution_percent);
                format!("{}% · {w} × {h} per eye", d.resolution_percent)
            }
            None => format!("{}% of the panel", d.resolution_percent),
        };
        card(open, ui, Head::new("Encoded resolution", subtitle, Icon::Frame, TEAL), None, |ui| {
            wide_slider(ui, egui::Slider::new(&mut d.resolution_percent, 50..=150).step_by(5.0).suffix("%"));
            match headset {
                Some(info) => {
                    let (w, h) = info.encoded_size(d.resolution_percent);
                    let (rw, rh) = info.render_size((w, h));
                    let (pw, ph) = info.panel_size();
                    kv(ui, "Per eye", value(format!("{w} × {h}"), TEXT));
                    kv(ui, "Panel", value(format!("{pw} × {ph}"), TEXT_DIM));
                    kv(ui, "SteamVR 100%", value(format!("{rw} × {rh}"), TEXT));
                }
                None => help(ui, "Connect a headset to see the size."),
            }
            help(ui, RESOLUTION_HELP);
        });

        // The switch stands for "below 100%" and comes back to the band last used.
        if d.foveation_percent < 100 {
            *foveation_band = d.foveation_percent;
        }
        let was_on = d.foveation_percent < 100;
        let mut on = was_on;
        let subtitle = match (was_on, headset) {
            (false, _) => "Off".to_string(),
            (true, Some(info)) => match foveated_size(info, d) {
                Some((w, h, _)) => format!("{}% band · {w} × {h} per eye encoded", d.foveation_percent),
                None => format!("{}% band", d.foveation_percent),
            },
            (true, None) => format!("{}% band", d.foveation_percent),
        };
        card(open, ui, Head::new("Foveated encoding", subtitle, Icon::Eye, PINK), Some(&mut on), |ui| {
            if d.foveation_percent < 100 {
                wide_slider(
                    ui,
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
                );
                if let Some(info) = headset {
                    match foveated_size(info, d) {
                        Some((w, h, pct)) => {
                            kv(ui, "Encoded per eye", value(format!("{w} × {h}"), TEXT));
                            kv(ui, "Pixels encoded", value(format!("{pct:.0}%"), TEAL));
                        }
                        None => help(ui, "Whole picture at full resolution."),
                    }
                }
            }
            help(ui, FOVEATION_HELP);
        });
        if on != was_on {
            d.foveation_percent = if on { *foveation_band } else { 100 };
        }
    }

    fn display_tab(&mut self, ui: &mut Ui, st: &Status) {
        let headset = st.headset.as_ref().map(|h| &h.info);
        let Self { draft: d, open, .. } = self;
        let high = headset.map_or(&[][..], |h| h.high_refresh_rates());
        // The session streams at the closest rate the headset's runtime lists; say so when that
        // is not the one picked (e.g. 240 Hz outside developer mode).
        let closest = headset
            .and_then(|h| {
                h.refresh_rates
                    .iter()
                    .copied()
                    .min_by(|a, b| (a - d.refresh_rate).abs().total_cmp(&(b - d.refresh_rate).abs()))
            })
            .filter(|hz| (hz - d.refresh_rate).abs() > 0.5);
        let subtitle = match closest {
            Some(hz) => format!("{} Hz · streams at {hz} Hz", d.refresh_rate),
            None => format!("{} Hz", d.refresh_rate),
        };
        card(open, ui, Head::new("Refresh rate", subtitle, Icon::Clock, AMBER), None, |ui| {
            let rates: Vec<(f32, String)> =
                [72.0, 80.0, 90.0, 120.0].iter().chain(high).map(|&hz| (hz, format!("{hz} Hz"))).collect();
            segmented(ui, &mut d.refresh_rate, &rates);
            if let Some(hz) = closest {
                note(ui, AMBER, &format!("The headset does not offer it now; the stream runs at {hz} Hz."));
            }
            help(ui, REFRESH_HELP);
        });

        let subtitle = if d.steamvr_layer_over_games { "On" } else { "Off" };
        card(
            open,
            ui,
            Head::new("SteamVR layer over games", subtitle.into(), Icon::Layers, VIOLET),
            Some(&mut d.steamvr_layer_over_games),
            |ui| help(ui, LAYER_HELP),
        );
    }

    fn audio_tab(&mut self, ui: &mut Ui, st: &Status) {
        let Self { draft: d, open, meters, .. } = self;
        let a = &st.audio;
        let subtitle = |exists: bool, on: bool, ready: &str| match (exists, on) {
            (true, _) => ready.to_string(),
            (false, true) => "On".to_string(),
            (false, false) => "Off".to_string(),
        };

        let s = subtitle(a.speakers, d.speakers, "\"OpenLink\" output device ready");
        card(open, ui, Head::new("Headset speakers", s, Icon::Speaker, TEAL), Some(&mut d.speakers), |ui| {
            meter(ui, meters.speakers, a.speakers).on_hover_text("Sound being sent to the headset");
            help(
                ui,
                "Adds an \"OpenLink\" output device; what plays on it is heard in the headset. Applies on the \
                 next connection.",
            );
        });

        let s = subtitle(a.microphone, d.microphone, "\"OpenLink\" microphone ready");
        card(open, ui, Head::new("Headset microphone", s, Icon::Mic, BLUE), Some(&mut d.microphone), |ui| {
            meter(ui, meters.microphone, a.microphone).on_hover_text("Sound arriving from the headset's microphone");
            help(
                ui,
                "Adds an \"OpenLink\" input device carrying the headset's microphone. Applies on the next \
                 connection.",
            );
        });

        if let Some(e) = &a.error {
            note(ui, RED, e);
        }
    }

    fn connection_tab(&mut self, ui: &mut Ui, st: &Status) {
        let Self { draft: d, open, .. } = self;
        let mode = d.connection;
        card(open, ui, Head::new("Connection", connection_label(mode).into(), connection_icon(mode), BLUE), None, |ui| {
            segmented(
                ui,
                &mut d.connection,
                &[
                    (ConnectionMode::WiFi, "Wi-Fi"),
                    (ConnectionMode::Usb, "USB cable"),
                    (ConnectionMode::SteamFrameAdapter, "Steam Frame adapter"),
                ],
            );
            help(
                ui,
                match d.connection {
                    ConnectionMode::WiFi => "The headset finds this PC by LAN broadcast.",
                    ConnectionMode::Usb => "Planned: nothing acts on this setting yet.",
                    ConnectionMode::SteamFrameAdapter => {
                        "Direct 6 GHz link through the Steam Frame's USB wireless adapter. OpenLink joins the \
                         headset's network with the profile Steam made when pairing it."
                    }
                },
            );
            help(ui, "Applies on the next connection.");
        });

        let subtitle = match &st.adapter {
            None => "Not plugged in".to_string(),
            Some(a) => match (a.ipv4, &a.ssid) {
                (Some(_), Some(ssid)) => format!("Connected · {ssid}"),
                (Some(_), None) => "Connected".to_string(),
                (None, _) => a.problem.clone().unwrap_or_else(|| "Plugged in, not connected to the headset".into()),
            },
        };
        card(open, ui, Head::new("Steam Frame adapter", subtitle, Icon::Usb, VIOLET), None, |ui| match &st.adapter {
            None => help(ui, "The Steam Frame's USB wireless adapter is not plugged into this PC."),
            Some(a) => {
                kv(ui, "Interface", value(&a.interface, TEXT));
                let address = a.ipv4.map_or("—".into(), |(ip, prefix)| format!("{ip}/{prefix}"));
                kv(ui, "Address", value(address, if a.ipv4.is_some() { TEXT } else { TEXT_FAINT }));
                kv(ui, "Network", value(a.ssid.as_deref().unwrap_or("—"), TEXT));
                kv(ui, "Link rate", value(a.tx_mbps.map_or("—".into(), |tx| format!("{tx:.0} Mbit/s")), TEAL));
                kv(ui, "Signal", value(a.signal_dbm.map_or("—".into(), |dbm| format!("{dbm} dBm")), TEAL));
                if let Some(p) = &a.problem {
                    note(ui, AMBER, p);
                }
            }
        });
    }
}

/// Per-eye size encoded at the draft's resolution and foveation, with the share of the pixels
/// it keeps; `None` when the band covers the whole picture.
fn foveated_size(info: &HeadsetInfo, d: &Settings) -> Option<(u32, u32, f32)> {
    let full = info.encoded_size(d.resolution_percent);
    let fraction = d.foveation_percent as f32 / 100.0;
    let f = openlink_protocol::foveation::Foveation::new(fraction, &info.fov, full)?;
    let (w, h) = f.coded_eye_size();
    Some((w, h, 100.0 * (w * h) as f32 / (full.0 * full.1) as f32))
}

// ------------------------------------------------------------------------------------------------
// Middle and bottom
// ------------------------------------------------------------------------------------------------

/// The headset and what it streams, centered in the main area.
fn hero(ui: &mut Ui, st: &Status) {
    let errors: Vec<&String> = [&st.last_error, &st.audio.error].into_iter().flatten().collect();
    let height = 200.0 + 22.0 * errors.len() as f32;
    ui.vertical_centered(|ui| {
        ui.add_space(((ui.available_height() - height) / 2.0).max(0.0));
        headset_glyph(ui, st.headset.is_some());
        ui.add_space(16.0);
        match &st.headset {
            Some(h) => {
                ui.label(RichText::new(&h.info.model).size(26.0).color(TEXT));
                let via = if st.adapter.as_ref().is_some_and(|a| a.reaches(h.addr.ip())) {
                    "the Steam Frame adapter"
                } else {
                    "the network"
                };
                ui.label(RichText::new(format!("Streaming to {} over {via}", h.addr.ip())).size(13.5).color(TEXT_DIM));
                ui.add_space(14.0);
                let s = &h.stream;
                let mut chips = vec![format!("{} × {} per eye", s.eye_width, s.eye_height)];
                if s.foveation.is_some() {
                    let (w, h) = s.coded_eye_size();
                    chips.push(format!("foveated to {w} × {h}"));
                }
                chips.push(format!("{} Hz", s.refresh_rate));
                chips.push(codec_name(s.codec).into());
                chips.push(format!("{} Mbit/s", s.bitrate_bps / 1_000_000));
                chip_row(ui, &chips);
            }
            None => {
                ui.label(RichText::new("Waiting for a headset…").size(24.0).color(TEXT_DIM));
                let driver = if st.driver_connected {
                    "SteamVR driver connected"
                } else {
                    "SteamVR driver not running"
                };
                ui.label(RichText::new(driver).size(13.5).color(TEXT_FAINT));
            }
        }
        for e in errors {
            ui.add_space(6.0);
            ui.label(RichText::new(e).size(12.5).color(RED));
        }
    });
}

/// The three cards along the bottom.
fn figures(ui: &mut Ui, st: &Status) {
    let gap = 10.0;
    let height = ui.available_height();
    let width = ui.available_width() - 2.0 * gap;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        type Card = fn(&mut Ui, &Status);
        let cards: [(f32, Card); 3] = [(0.34, link_card), (0.30, stream_card), (0.36, timing_card)];
        for (share, card) in cards {
            let w = width * share;
            ui.allocate_ui_with_layout(vec2(w, height), Layout::top_down(Align::Min), |ui| {
                ui.set_min_size(vec2(w, height));
                card(ui, st);
            });
        }
    });
}

fn figure_card(ui: &mut Ui, title: &str, note: &str, add: impl FnOnce(&mut Ui)) {
    panel_frame().inner_margin(Margin::symmetric(14, 12)).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 5.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(12.5).color(TEXT_DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(note).size(11.0).color(TEXT_FAINT));
            });
        });
        ui.add_space(2.0);
        add(ui);
    });
}

fn link_card(ui: &mut Ui, st: &Status) {
    figure_card(ui, "Link", "", |ui| {
        let driver = if st.driver_connected { value("connected", TEAL) } else { value("not running", TEXT_FAINT) };
        kv(ui, "SteamVR driver", driver);
        match &st.headset {
            Some(h) => {
                kv(ui, "Headset", value(&h.info.model, TEAL));
                kv(ui, "Address", value(h.addr.ip().to_string(), TEXT));
            }
            None => {
                kv(ui, "Headset", value("waiting", TEXT_FAINT));
                kv(ui, "Address", value("—", TEXT_FAINT));
            }
        }
        match &st.adapter {
            None => kv(ui, "Frame adapter", value("not plugged in", TEXT_FAINT)),
            Some(a) if a.ipv4.is_some() => {
                kv(ui, "Frame adapter", value(a.ssid.as_deref().unwrap_or(&a.interface), TEAL));
                let link = match (a.tx_mbps, a.signal_dbm) {
                    (Some(tx), Some(dbm)) => format!("{tx:.0} Mbit/s · {dbm} dBm"),
                    (Some(tx), None) => format!("{tx:.0} Mbit/s"),
                    (None, Some(dbm)) => format!("{dbm} dBm"),
                    (None, None) => "—".into(),
                };
                kv(ui, "Adapter link", value(link, TEXT));
            }
            Some(a) => {
                let problem = a.problem.as_deref().unwrap_or("not connected to the headset");
                kv(ui, "Frame adapter", value(problem, AMBER));
            }
        }
        let a = &st.audio;
        let audio = match (a.speakers, a.microphone) {
            (true, true) => value("output + microphone", TEAL),
            (true, false) => value("output", TEAL),
            (false, true) => value("microphone", TEAL),
            (false, false) => value("off", TEXT_FAINT),
        };
        kv(ui, "Audio devices", audio);
    });
}

fn stream_card(ui: &mut Ui, st: &Status) {
    let note = if st.headset.is_some() { "this session" } else { "" };
    figure_card(ui, "Video stream", note, |ui| {
        let Some(h) = &st.headset else {
            ui.add_space(ui.available_height() / 2.0 - 14.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No headset connected").size(12.5).color(TEXT_FAINT));
            });
            return;
        };
        let s = &h.stream;
        let (cw, ch) = s.coded_eye_size();
        kv(ui, "Codec", value(codec_name(s.codec), TEXT));
        kv(ui, "Per eye", value(format!("{} × {}", s.eye_width, s.eye_height), TEXT));
        kv(ui, "Encoded", value(format!("{cw} × {ch}"), TEXT));
        kv(ui, "Refresh", value(format!("{} Hz", s.refresh_rate), TEXT));
        kv(ui, "Bitrate", value(format!("{} Mbit/s", s.bitrate_bps / 1_000_000), TEXT));
        let share = (cw * ch) as f32 / (s.eye_width * s.eye_height).max(1) as f32;
        bar(ui, "Pixels encoded", share, format!("{:.0}%", 100.0 * share));
    });
}

fn timing_card(ui: &mut Ui, st: &Status) {
    figure_card(ui, "Tracking and latency", "", |ui| {
        let h = st.headset.as_ref();
        let photon = h.and_then(|h| h.photon_latency_ms);
        let rtt = h.and_then(|h| h.min_rtt_ns).map(|ns| ns as f32 * 1e-6);
        let hz = h.map(|h| h.tracking_hz);
        let age = h.map(|h| h.sample_age_ms);
        // The big latency figure goes first when the card is too narrow for it and the gauges.
        let wide = ui.available_width() >= 250.0;
        ui.horizontal(|ui| {
            if wide {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add_space(8.0);
                    let (text, color) = match photon {
                        Some(ms) => (format!("{ms:.1}"), grade(ms, 40.0, 60.0)),
                        None => ("—".to_string(), TEXT_FAINT),
                    };
                    ui.label(RichText::new(text).monospace().size(28.0).color(color));
                    ui.label(RichText::new("ms present\nto photon").size(11.0).color(TEXT_DIM));
                });
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                gauge(ui, age.map(|ms| ms / 10.0), age.map(|ms| grade(ms, 5.0, 10.0)), age.map(|ms| format!("{ms:.1}")), "Age ms");
                gauge(ui, rtt.map(|ms| ms / 10.0), rtt.map(|ms| grade(ms, 5.0, 10.0)), rtt.map(|ms| format!("{ms:.1}")), "RTT ms");
                gauge(ui, hz.map(|hz| hz / 500.0), hz.map(|hz| grade(-hz, -450.0, -250.0)), hz.map(|hz| format!("{hz:.0}")), "Tracking");
            });
        });
        let sync = match h.map(|h| (h.clock_offset_ns, h.min_rtt_ns)) {
            Some((Some(_), Some(_))) => value("locked", TEAL),
            Some(_) => value("syncing…", AMBER),
            None => value("—", TEXT_FAINT),
        };
        kv(ui, "Clock sync", sync);
        if !wide {
            let photon = photon.map_or(value("N/A", TEXT_FAINT), |ms| value(format!("{ms:.1} ms"), TEXT));
            kv(ui, "Present to photon", photon);
        }
    });
}

/// Teal up to `good`, amber up to `poor`, red beyond.
fn grade(v: f32, good: f32, poor: f32) -> Color32 {
    if v <= good {
        TEAL
    } else if v <= poor {
        AMBER
    } else {
        RED
    }
}

// ------------------------------------------------------------------------------------------------
// Widgets
// ------------------------------------------------------------------------------------------------

fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        use egui::TextStyle;
        style.text_styles = [
            (TextStyle::Heading, FontId::proportional(20.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
        style.spacing.item_spacing = vec2(8.0, 6.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size.y = 24.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = CARD;
        v.window_stroke = Stroke::new(1.0, LINE_STRONG);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(8);
        v.extreme_bg_color = INSET;
        v.faint_bg_color = CARD;
        v.weak_text_color = Some(TEXT_DIM);
        v.hyperlink_color = TEAL;
        v.warn_fg_color = AMBER;
        v.error_fg_color = RED;
        v.selection.bg_fill = ACCENT;
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.slider_trailing_fill = true;
        let set = |w: &mut egui::style::WidgetVisuals, bg: Color32, stroke: Color32, fg: Color32| {
            w.bg_fill = bg;
            w.weak_bg_fill = bg;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.fg_stroke = Stroke::new(1.0, fg);
            w.corner_radius = CornerRadius::same(7);
            w.expansion = 0.0;
        };
        set(&mut v.widgets.noninteractive, CARD, LINE, TEXT);
        set(&mut v.widgets.inactive, Color32::from_rgb(0x23, 0x25, 0x3b), LINE_STRONG, TEXT);
        set(&mut v.widgets.hovered, Color32::from_rgb(0x2c, 0x2f, 0x4a), Color32::from_rgb(0x46, 0x49, 0x6e), Color32::WHITE);
        set(&mut v.widgets.active, Color32::from_rgb(0x35, 0x38, 0x58), ACCENT, Color32::WHITE);
        set(&mut v.widgets.open, Color32::from_rgb(0x35, 0x38, 0x58), LINE_STRONG, Color32::WHITE);
    });
}

/// The window's base colour, with a warm glow rising from the bottom edge.
fn paint_backdrop(ui: &Ui) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, BG);
    let top = rect.bottom() - rect.height() * 0.5;
    let glow = Color32::from_rgba_unmultiplied(0x8a, 0x1e, 0x4c, 60);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(pos2(rect.left(), top), Color32::TRANSPARENT);
    mesh.colored_vertex(pos2(rect.right(), top), Color32::TRANSPARENT);
    mesh.colored_vertex(rect.right_bottom(), glow);
    mesh.colored_vertex(rect.left_bottom(), glow);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}

fn panel_frame() -> Frame {
    Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(12))
}

fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 7.0, ACCENT);
    for dx in [-5.0, 5.0] {
        p.circle_filled(rect.center() + vec2(dx, 0.5), 3.8, BG);
    }
}

/// A clickable chip with an icon and a small chevron, like a drop-down.
fn chip(ui: &mut Ui, icon: Icon, text: &str) -> egui::Response {
    Frame::new()
        .fill(CHIP)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                paint_icon(ui.painter(), icon, rect, TEXT_DIM);
                ui.label(RichText::new(text).size(13.0).color(TEXT));
                let (rect, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                chevron(ui.painter(), rect.center(), true, TEXT_FAINT);
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn badge(ui: &mut Ui, text: &str) {
    Frame::new()
        .fill(BADGE_BG)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| ui.label(RichText::new(text).monospace().size(12.0).color(BADGE_FG)));
        });
}

fn status_pill(ui: &mut Ui, label: &str, ok: bool) {
    Frame::new()
        .fill(CHIP)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                dot(ui, if ok { GREEN } else { TEXT_FAINT });
                ui.label(RichText::new(label).size(12.5).color(if ok { TEXT } else { TEXT_DIM }));
            });
        });
}

fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 9.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// Chips side by side (each sized to its text), in as many rows as the width needs; in a
/// centered layout each row is centered.
fn chip_row(ui: &mut Ui, items: &[String]) {
    let (pad, gap, height) = (12.0, 6.0, 26.0);
    let max_width = ui.available_width();
    let mut rows: Vec<(Vec<Arc<egui::Galley>>, f32)> = Vec::new();
    for text in items {
        let g = ui.painter().layout_no_wrap(text.clone(), FontId::proportional(12.5), TEXT);
        let w = g.size().x + 2.0 * pad;
        match rows.last_mut() {
            Some((row, row_width)) if *row_width + gap + w <= max_width => {
                *row_width += gap + w;
                row.push(g);
            }
            _ => rows.push((vec![g], w)),
        }
    }
    for (row, row_width) in rows {
        let (rect, _) = ui.allocate_exact_size(vec2(row_width, height), Sense::hover());
        let p = ui.painter();
        let mut x = rect.left();
        for g in row {
            let r = Rect::from_min_size(pos2(x, rect.top()), vec2(g.size().x + 2.0 * pad, height));
            p.rect_filled(r, 7.0, CHIP);
            p.rect_stroke(r, 7.0, Stroke::new(1.0, LINE), StrokeKind::Inside);
            p.galley(pos2(r.left() + pad, r.center().y - g.size().y / 2.0), g, TEXT);
            x = r.right() + gap;
        }
    }
}

/// A stylised headset: visor, lenses and strap, lit teal while one is connected.
fn headset_glyph(ui: &mut Ui, connected: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(170.0, 96.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let tint = if connected { TEAL } else { TEXT_FAINT };
    if connected {
        p.circle_filled(c, 64.0, TEAL.gamma_multiply(0.05));
        p.circle_filled(c, 48.0, TEAL.gamma_multiply(0.06));
    }
    let strap = Stroke::new(5.0, LINE_STRONG);
    p.line_segment([pos2(c.x - 78.0, c.y - 6.0), pos2(c.x - 58.0, c.y - 6.0)], strap);
    p.line_segment([pos2(c.x + 58.0, c.y - 6.0), pos2(c.x + 78.0, c.y - 6.0)], strap);
    let visor = Rect::from_center_size(c, vec2(120.0, 62.0));
    p.rect_filled(visor, 24.0, CARD);
    p.rect_stroke(visor, 24.0, Stroke::new(1.5, LINE_STRONG), StrokeKind::Inside);
    for dx in [-27.0, 27.0] {
        let lens = pos2(c.x + dx, c.y + 2.0);
        p.circle_filled(lens, 16.0, INSET);
        p.circle_stroke(lens, 16.0, Stroke::new(1.5, tint));
    }
}

/// A setting card's header: icon, title and a one-line summary of the current value.
struct Head {
    title: &'static str,
    subtitle: String,
    icon: Icon,
    hue: Color32,
}

impl Head {
    fn new(title: &'static str, subtitle: String, icon: Icon, hue: Color32) -> Self {
        Self { title, subtitle, icon, hue }
    }
}

/// A setting card: a header that expands or collapses it when clicked, with an optional switch,
/// and `body` below while expanded.
fn card(open: &mut HashSet<&'static str>, ui: &mut Ui, head: Head, switch: Option<&mut bool>, body: impl FnOnce(&mut Ui)) {
    let expanded = open.contains(head.title);
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                if expanded {
                    open.remove(head.title);
                } else {
                    open.insert(head.title);
                }
            }
            let p = ui.painter();
            let icon = Rect::from_center_size(pos2(rect.left() + 15.0, rect.center().y), vec2(30.0, 30.0));
            p.rect_filled(icon, 8.0, head.hue.gamma_multiply(0.16));
            paint_icon(p, head.icon, icon.shrink(8.0), head.hue);
            let text_right = rect.right() - if switch.is_some() { 76.0 } else { 24.0 };
            let clipped = p.with_clip_rect(Rect::from_min_max(pos2(icon.right(), rect.top()), pos2(text_right, rect.bottom())));
            let x = icon.right() + 10.0;
            let title = if response.hovered() { Color32::WHITE } else { TEXT };
            clipped.text(pos2(x, rect.center().y - 8.0), Align2::LEFT_CENTER, head.title, FontId::proportional(14.0), title);
            clipped.text(pos2(x, rect.center().y + 9.0), Align2::LEFT_CENTER, &head.subtitle, FontId::proportional(11.5), TEXT_FAINT);
            chevron(p, pos2(rect.right() - 7.0, rect.center().y), expanded, TEXT_FAINT);
            if let Some(on) = switch {
                let at = Rect::from_center_size(pos2(rect.right() - 46.0, rect.center().y), vec2(36.0, 20.0));
                ui.put(at, toggle(on));
            }
            if expanded {
                ui.add_space(8.0);
                body(ui);
            }
        });
}

/// An on/off switch.
fn toggle(on: &mut bool) -> impl egui::Widget + '_ {
    move |ui: &mut Ui| {
        let (rect, mut response) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
        if response.clicked() {
            *on = !*on;
            response.mark_changed();
        }
        let t = ui.ctx().animate_bool(response.id, *on);
        let p = ui.painter();
        p.rect_filled(rect, 10.0, TOGGLE_OFF.lerp_to_gamma(ACCENT, t));
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        p.circle_filled(pos2(x, rect.center().y), 7.0, Color32::WHITE);
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

/// One of a few values, as a row of buttons in a well.
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, options: &[(T, impl AsRef<str>)]) {
    Frame::new()
        .fill(INSET)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(2.0, 2.0);
                for (option, label) in options {
                    let selected = *current == *option;
                    let text = RichText::new(label.as_ref()).size(12.5).color(if selected { TEXT } else { TEXT_DIM });
                    let button = Button::new(text)
                        .fill(TAB_SELECTED)
                        .frame_when_inactive(selected)
                        .corner_radius(CornerRadius::same(6))
                        .min_size(vec2(0.0, 26.0));
                    if ui.add(button).clicked() {
                        *current = *option;
                    }
                }
            });
        });
}

/// A slider across the card, its value box at the end.
fn wide_slider(ui: &mut Ui, slider: egui::Slider) -> egui::Response {
    ui.spacing_mut().slider_width = (ui.available_width() - 84.0).max(80.0);
    ui.add(slider)
}

/// A label on the left and a control on the right.
fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(12.5).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), add);
    });
}

/// A row of text only: as tall as its text rather than a button.
fn text_row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 18.0;
        ui.horizontal(add).inner
    })
    .inner
}

/// A name on the left and its value on the right.
fn kv(ui: &mut Ui, key: &str, value: RichText) {
    text_row(ui, |ui| {
        ui.label(RichText::new(key).size(12.5).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Label::new(value).truncate());
        });
    });
}

fn value(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text.into()).monospace().size(12.0).color(color)
}

fn help(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(11.5).color(TEXT_FAINT));
}

fn note(ui: &mut Ui, color: Color32, text: &str) {
    ui.label(RichText::new(text).size(12.0).color(color));
}

/// A labelled bar for a share (0..1) with its value on the right.
fn bar(ui: &mut Ui, label: &str, share: f32, text: String) {
    text_row(ui, |ui| {
        ui.label(RichText::new(label).size(12.5).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(value(text, TEXT));
            let (rect, _) = ui.allocate_exact_size(vec2((ui.available_width() - 6.0).max(20.0), 6.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(rect, 3.0, INSET);
            let mut fill = rect;
            fill.set_width(rect.width() * share.clamp(0.0, 1.0));
            p.rect_filled(fill, 3.0, TEAL);
        });
    });
}

/// A ring filled to `share` (0..1) with `text` inside and `label` below; empty with a dash
/// when there is nothing to show.
fn gauge(ui: &mut Ui, share: Option<f32>, color: Option<Color32>, text: Option<String>, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(58.0, 76.0), Sense::hover());
    let p = ui.painter();
    let c = pos2(rect.center().x, rect.top() + 28.0);
    let r = 21.0;
    p.circle_stroke(c, r, Stroke::new(5.0, TOGGLE_OFF));
    if let (Some(share), Some(color)) = (share, color) {
        let end = -PI / 2.0 + TAU * share.clamp(0.0, 1.0);
        p.add(Shape::line(arc(c, r, -PI / 2.0, end), Stroke::new(5.0, color)));
    }
    let text = text.unwrap_or_else(|| "–".into());
    p.text(c, Align2::CENTER_CENTER, text, FontId::monospace(11.5), TEXT);
    p.text(pos2(c.x, rect.bottom() - 8.0), Align2::CENTER_CENTER, label, FontId::proportional(11.0), TEXT_DIM);
}

/// A horizontal level meter for a peak `level` (0..1), on a -60..0 dBFS scale, across the
/// available width. Empty when the device does not exist (`active` false).
fn meter(ui: &mut Ui, level: f32, active: bool) -> egui::Response {
    const FLOOR_DB: f32 = -60.0;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, INSET);
    if active && level > 0.0 {
        let db = 20.0 * level.log10();
        let frac = ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
        let color = if db > -1.0 {
            RED
        } else if db > -12.0 {
            AMBER
        } else {
            TEAL
        };
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        painter.rect_filled(fill, 4.0, color);
    }
    response
}

/// Points along a circular arc from angle `from` to `to` (radians, clockwise on screen).
fn arc(c: Pos2, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let n = ((to - from).abs() / TAU * 72.0).ceil().max(2.0) as usize;
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * i as f32 / n as f32;
            c + r * vec2(a.cos(), a.sin())
        })
        .collect()
}

fn chevron(p: &Painter, c: Pos2, down: bool, color: Color32) {
    let s = 3.5;
    let points = if down {
        vec![c + vec2(-s, -s / 2.0), c + vec2(0.0, s / 2.0), c + vec2(s, -s / 2.0)]
    } else {
        vec![c + vec2(-s / 2.0, -s), c + vec2(s / 2.0, 0.0), c + vec2(-s / 2.0, s)]
    };
    p.add(Shape::line(points, Stroke::new(1.5, color)));
}

#[derive(Debug, Clone, Copy)]
enum Icon {
    Film,
    Chip,
    Wave,
    Frame,
    Eye,
    Clock,
    Layers,
    Speaker,
    Mic,
    Wifi,
    Usb,
}

/// Line-drawn icons, fitted to `r`.
fn paint_icon(p: &Painter, icon: Icon, r: Rect, color: Color32) {
    let s = Stroke::new(1.5, color);
    let (w, h) = (r.width(), r.height());
    let at = |x: f32, y: f32| pos2(r.left() + x * w, r.top() + y * h);
    match icon {
        Icon::Film => {
            p.rect_stroke(Rect::from_min_max(at(0.0, 0.12), at(1.0, 0.88)), 2.0, s, StrokeKind::Middle);
            for x in [0.32, 0.68] {
                p.line_segment([at(x, 0.12), at(x, 0.88)], s);
            }
        }
        Icon::Chip => {
            p.rect_stroke(Rect::from_min_max(at(0.2, 0.2), at(0.8, 0.8)), 2.0, s, StrokeKind::Middle);
            for t in [0.4, 0.6] {
                p.line_segment([at(t, 0.0), at(t, 0.2)], s);
                p.line_segment([at(t, 0.8), at(t, 1.0)], s);
                p.line_segment([at(0.0, t), at(0.2, t)], s);
                p.line_segment([at(0.8, t), at(1.0, t)], s);
            }
        }
        Icon::Wave => {
            let points = [(0.0, 0.6), (0.2, 0.6), (0.35, 0.15), (0.55, 0.85), (0.7, 0.4), (0.82, 0.6), (1.0, 0.6)];
            p.add(Shape::line(points.iter().map(|&(x, y)| at(x, y)).collect(), s));
        }
        Icon::Frame => {
            for (x, y, dx, dy) in [(0.0, 0.0, 1.0, 1.0), (1.0, 0.0, -1.0, 1.0), (0.0, 1.0, 1.0, -1.0), (1.0, 1.0, -1.0, -1.0)] {
                let corner = at(x, y);
                p.add(Shape::line(vec![corner + vec2(0.0, dy * 0.35 * h), corner, corner + vec2(dx * 0.35 * w, 0.0)], s));
            }
        }
        Icon::Eye => {
            // Two lids: arcs of one circle each, meeting at the corners of the eye.
            let c = r.center();
            let (half, apex) = (w * 0.5, h * 0.32);
            let radius = (half * half + apex * apex) / (2.0 * apex);
            let k = radius - apex;
            let (left, right) = ((-k).atan2(-half), (-k).atan2(half));
            p.add(Shape::line(arc(pos2(c.x, c.y + k), radius, left, right), s));
            p.add(Shape::line(arc(pos2(c.x, c.y - k), radius, -right, -left), s));
            p.circle_filled(c, h * 0.16, color);
        }
        Icon::Clock => {
            let c = r.center();
            p.circle_stroke(c, w * 0.48, s);
            p.line_segment([c, c + vec2(0.0, -h * 0.28)], s);
            p.line_segment([c, c + vec2(w * 0.22, 0.0)], s);
        }
        Icon::Layers => {
            p.rect_stroke(Rect::from_min_max(at(0.0, 0.0), at(0.68, 0.68)), 2.0, s, StrokeKind::Middle);
            let front = Rect::from_min_max(at(0.32, 0.32), at(1.0, 1.0));
            p.rect_filled(front, 2.0, color.gamma_multiply(0.35));
            p.rect_stroke(front, 2.0, s, StrokeKind::Middle);
        }
        Icon::Speaker => {
            let body = [(0.0, 0.35), (0.25, 0.35), (0.5, 0.1), (0.5, 0.9), (0.25, 0.65), (0.0, 0.65)];
            p.add(Shape::closed_line(body.iter().map(|&(x, y)| at(x, y)).collect(), s));
            p.add(Shape::line(arc(at(0.5, 0.5), w * 0.24, -0.8, 0.8), s));
            p.add(Shape::line(arc(at(0.5, 0.5), w * 0.46, -0.8, 0.8), s));
        }
        Icon::Mic => {
            p.rect_stroke(Rect::from_min_max(at(0.32, 0.0), at(0.68, 0.62)), w * 0.18, s, StrokeKind::Middle);
            p.add(Shape::line(arc(at(0.5, 0.42), w * 0.32, 0.15, PI - 0.15), s));
            p.line_segment([at(0.5, 0.74), at(0.5, 1.0)], s);
        }
        Icon::Wifi => {
            let base = at(0.5, 0.88);
            for radius in [0.3, 0.55, 0.8] {
                p.add(Shape::line(arc(base, h * radius, -2.36, -0.79), s));
            }
            p.circle_filled(base, 1.6, color);
        }
        Icon::Usb => {
            p.rect_stroke(Rect::from_min_max(at(0.25, 0.3), at(0.75, 0.7)), 2.0, s, StrokeKind::Middle);
            p.line_segment([at(0.4, 0.3), at(0.4, 0.05)], s);
            p.line_segment([at(0.6, 0.3), at(0.6, 0.05)], s);
            p.line_segment([at(0.5, 0.7), at(0.5, 1.0)], s);
        }
    }
}

fn connection_label(mode: ConnectionMode) -> &'static str {
    match mode {
        ConnectionMode::WiFi => "Wi-Fi",
        ConnectionMode::Usb => "USB cable",
        ConnectionMode::SteamFrameAdapter => "Steam Frame adapter",
    }
}

fn connection_icon(mode: ConnectionMode) -> Icon {
    match mode {
        ConnectionMode::WiFi | ConnectionMode::SteamFrameAdapter => Icon::Wifi,
        ConnectionMode::Usb => Icon::Usb,
    }
}

fn encoder_label(p: BackendPreference) -> String {
    match p {
        BackendPreference::Auto => "Auto".to_string(),
        BackendPreference::Fixed(b) => b.name().to_string(),
    }
}

fn codec_label(c: CodecPreference) -> &'static str {
    match c {
        CodecPreference::Auto => "Auto (best available)",
        CodecPreference::Fixed(c) => codec_name(c),
    }
}

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::Av1 => "AV1",
        Codec::H265 => "HEVC (H.265)",
        Codec::H264 => "H.264",
    }
}

/// The pipeline a frame goes through with the draft settings, as chips; steps switched off are
/// struck through.
fn pipeline(ui: &mut Ui, d: &Settings, st: &Status) {
    Frame::new()
        .fill(INSET)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Pipeline").size(12.0).color(TEXT_DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("struck through: off").size(11.0).color(TEXT_FAINT));
                });
            });
            let codec = match (&st.headset, d.codec) {
                (Some(h), _) => short_codec(h.stream.codec),
                (None, CodecPreference::Fixed(c)) => short_codec(c),
                (None, CodecPreference::Auto) => "Auto",
            };
            let headset = st.headset.as_ref().map_or("Headset".to_string(), |h| h.info.model.clone());
            let steps = [
                ("SteamVR".to_string(), true),
                ("SteamVR layer".to_string(), d.steamvr_layer_over_games),
                ("Foveation".to_string(), d.foveation_percent < 100),
                ("NV12".to_string(), true),
                (format!("{codec} encode"), true),
                (connection_label(d.connection).to_string(), true),
                (headset, true),
            ];
            ui.add_space(2.0);
            flow(ui, &steps);
        });
}

/// Chips joined by arrows, wrapping onto new rows (each row after the first starts with an
/// arrow). Laid out by hand: egui's wrapping layout squeezes a framed label that no longer fits
/// into one letter per line instead of moving it to the next row.
fn flow(ui: &mut Ui, steps: &[(String, bool)]) {
    let (pad, height, row_gap, arrow) = (7.0, 22.0, 6.0, 16.0);
    let width = ui.available_width();
    let galleys: Vec<_> = steps
        .iter()
        .map(|(text, on)| {
            ui.painter().layout_no_wrap(text.clone(), FontId::proportional(12.0), if *on { TEXT } else { TEXT_FAINT })
        })
        .collect();
    // Where each chip goes: (row, x of its arrow or None, x of the chip).
    let mut places = Vec::with_capacity(galleys.len());
    let (mut row, mut x) = (0usize, 0.0f32);
    for (i, g) in galleys.iter().enumerate() {
        let chip = g.size().x + 2.0 * pad;
        let lead = if i == 0 { 0.0 } else { arrow };
        if i > 0 && x + lead + chip > width {
            row += 1;
            x = 0.0;
        }
        places.push((row, (i > 0).then_some(x), x + lead));
        x += lead + chip + 2.0;
    }
    let rows = row + 1;
    let total = rows as f32 * height + (rows - 1) as f32 * row_gap;
    let (rect, _) = ui.allocate_exact_size(vec2(width, total), Sense::hover());
    let p = ui.painter();
    for ((g, (_, on)), (row, arrow_x, chip_x)) in galleys.into_iter().zip(steps).zip(places) {
        let top = rect.top() + row as f32 * (height + row_gap);
        if let Some(ax) = arrow_x {
            let at = pos2(rect.left() + ax + arrow / 2.0, top + height / 2.0);
            p.text(at, Align2::CENTER_CENTER, "→", FontId::monospace(11.0), TEXT_FAINT);
        }
        let r = Rect::from_min_size(pos2(rect.left() + chip_x, top), vec2(g.size().x + 2.0 * pad, height));
        p.rect_filled(r, 6.0, CARD);
        p.rect_stroke(r, 6.0, Stroke::new(1.0, LINE), StrokeKind::Inside);
        let text_at = pos2(r.left() + pad, r.center().y - g.size().y / 2.0);
        let text_width = g.size().x;
        p.galley(text_at, g, TEXT);
        if !on {
            let y = r.center().y + 0.5;
            p.line_segment([pos2(text_at.x, y), pos2(text_at.x + text_width, y)], Stroke::new(1.0, TEXT_FAINT));
        }
    }
}

fn short_codec(c: Codec) -> &'static str {
    match c {
        Codec::Av1 => "AV1",
        Codec::H265 => "HEVC",
        Codec::H264 => "H.264",
    }
}
