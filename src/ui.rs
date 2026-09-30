//! egui front end. BLE work runs on a tokio runtime and reports back as `Event`s.

use std::collections::{BTreeMap, HashMap};
use std::f64::consts::TAU;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use btleplug::platform::Adapter;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Key,
    Layout, Margin, Rect, Response, RichText, Sense, Stroke, StrokeKind, TextStyle, TextureHandle,
    Ui, pos2, vec2,
};

use crate::ble::{self, Power, Res, Station};

const BG: Color32 = Color32::from_rgb(0x0e, 0x0f, 0x11);
const PANEL: Color32 = Color32::from_rgb(0x17, 0x19, 0x1c);
const LINE: Color32 = Color32::from_rgb(0x2b, 0x2e, 0x34);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xe3, 0xd8);
const DIM: Color32 = Color32::from_rgb(0x84, 0x80, 0x78);
const LASER: Color32 = Color32::from_rgb(0xff, 0x7a, 0x1a);
const ON: Color32 = Color32::from_rgb(0x3d, 0xe0, 0x5a);
const STANDBY: Color32 = Color32::from_rgb(0x4a, 0xa8, 0xff);
const SLEEP: Color32 = Color32::from_rgb(0x6d, 0x7a, 0x8c);
const BAD: Color32 = Color32::from_rgb(0xf0, 0x33, 0x2a);

pub fn run() -> eframe::Result {
    let icon = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("bundled icon")
        .to_rgba8();
    let icon = egui::IconData {
        width: icon.width(),
        height: icon.height(),
        rgba: icon.into_raw(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(icon)
            .with_title("Lighthouse")
            .with_app_id("lighthouse")
            .with_inner_size([860.0, 720.0])
            .with_min_inner_size([600.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Lighthouse",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// ---------------------------------------------------------------- worker

#[derive(Clone, Copy)]
enum Cmd {
    Refresh,
    Power(Power),
    Channel(u8),
    Identify,
}

enum Event {
    Found(Vec<ble::Seen>),
    Survey(Result<Vec<ble::Seen>, String>),
    ScanFailed(String),
    Power(String, Power),
    Channel(String, u8),
    Info(String, Vec<(&'static str, String)>),
    Done(String, Option<String>),
}

struct Shared {
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
    adapter: tokio::sync::Mutex<Option<Adapter>>,
    /// A station takes one connection at a time: queue work per address.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Shared {
    fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    /// Lazy so the app still opens (and can retry) while bluetooth.service is down.
    async fn adapter(&self) -> Res<Adapter> {
        let mut a = self.adapter.lock().await;
        if a.is_none() {
            *a = Some(ble::adapter().await?);
        }
        Ok(a.clone().unwrap())
    }

    async fn scan(&self) {
        match async { ble::scan(&self.adapter().await?, 6).await }.await {
            Ok(found) => self.send(Event::Found(found)),
            Err(e) => self.send(Event::ScanFailed(e.to_string())),
        }
    }

    async fn survey(&self, skip: Vec<String>) {
        let r = async { ble::survey(&self.adapter().await?, &skip).await }.await;
        self.send(Event::Survey(r.map_err(|e| e.to_string())));
    }

    async fn run(&self, addr: String, cmd: Cmd) {
        let lock = self
            .locks
            .lock()
            .unwrap()
            .entry(addr.clone())
            .or_default()
            .clone();
        let _g = lock.lock().await;
        let r = self.session(&addr, cmd).await;
        self.send(Event::Done(addr, r.err().map(|e| e.to_string())));
    }

    async fn session(&self, addr: &str, cmd: Cmd) -> Res<()> {
        let st = Station::open(&self.adapter().await?, addr).await?;
        let r = self.apply(&st, addr, cmd).await;
        st.close().await;
        r
    }

    async fn apply(&self, st: &Station, addr: &str, cmd: Cmd) -> Res<()> {
        let a = || addr.to_string();
        match cmd {
            Cmd::Refresh => {
                self.send(Event::Power(a(), st.power().await?));
                self.send(Event::Channel(a(), st.channel().await?));
                self.send(Event::Info(a(), st.info().await));
            }
            Cmd::Power(p) => {
                st.set_power(p).await?;
                st.settle(p, |p| self.send(Event::Power(a(), p))).await?;
            }
            Cmd::Channel(c) => {
                st.set_channel(c).await?;
                self.send(Event::Channel(a(), st.channel().await?));
            }
            Cmd::Identify => st.identify().await?,
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- app state

struct Row {
    addr: String,
    name: String,
    power: Option<Power>,
    channel: Option<u8>,
    info: Vec<(&'static str, String)>,
    details: bool,
    busy: u32,
    error: Option<String>,
}

fn row(addr: String, name: String) -> Row {
    Row {
        addr,
        name,
        power: None,
        channel: None,
        info: Vec::new(),
        details: false,
        busy: 0,
        error: None,
    }
}

enum Act {
    Run(Cmd),
    Off,
    Rename(String),
    Details,
    Forget,
}

struct Rename {
    addr: String,
    text: String,
    focus: bool,
}

struct App {
    rt: tokio::runtime::Runtime,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Event>,
    rows: Vec<Row>,
    scanning: bool,
    status: String,
    status_bad: bool,
    rename: Option<Rename>,
    /// What the red button does: sleep or standby.
    off: Power,
    photo: TextureHandle,
    /// Stations heard that aren't ours, once an interference check ran.
    neighbours: Option<Vec<ble::Seen>>,
    surveying: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let img = image::load_from_memory(include_bytes!("../assets/basestation.png"))
            .expect("bundled photo")
            .to_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let photo = cc.egui_ctx.load_texture(
            "basestation",
            egui::ColorImage::from_rgba_unmultiplied(size, &img),
            egui::TextureOptions::LINEAR,
        );
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            tx,
            ctx: cc.egui_ctx.clone(),
            adapter: Default::default(),
            locks: Default::default(),
        });
        let mut app = App {
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
            shared,
            rx,
            rows: ble::load()
                .into_iter()
                .map(|(addr, name)| row(addr, name))
                .collect(),
            scanning: false,
            status: String::new(),
            status_bad: false,
            rename: None,
            off: ble::off_mode(),
            photo,
            neighbours: None,
            surveying: false,
        };
        if app.rows.is_empty() {
            app.scan();
        } else {
            app.say(format!(
                "{} saved station(s). Reading state…",
                app.rows.len()
            ));
            app.send_all(Cmd::Refresh);
        }
        app
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_bad = false;
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_bad = true;
    }

    fn scan(&mut self) {
        self.scanning = true;
        self.say("Scanning for base stations (6 s)…");
        let s = self.shared.clone();
        self.rt.spawn(async move { s.scan().await });
    }

    fn send(&mut self, addr: &str, cmd: Cmd) {
        let Some(r) = self.rows.iter_mut().find(|r| r.addr == addr) else {
            return;
        };
        r.busy += 1;
        r.error = None;
        let (s, addr) = (self.shared.clone(), addr.to_string());
        self.rt.spawn(async move { s.run(addr, cmd).await });
    }

    fn send_all(&mut self, cmd: Cmd) {
        for addr in self.rows.iter().map(|r| r.addr.clone()).collect::<Vec<_>>() {
            self.send(&addr, cmd);
        }
    }

    fn save(&mut self) {
        let map: BTreeMap<_, _> = self
            .rows
            .iter()
            .map(|r| (r.addr.clone(), r.name.clone()))
            .collect();
        if let Err(e) = ble::save(&map) {
            self.warn(format!("Could not save stations: {e}"));
        }
    }

    fn survey(&mut self) {
        self.surveying = true;
        self.say(
            "Checking interference: listening for other stations, then reading their channels…",
        );
        let skip = self.rows.iter().map(|r| r.addr.clone()).collect();
        let s = self.shared.clone();
        self.rt.spawn(async move { s.survey(skip).await });
        self.send_all(Cmd::Refresh);
    }

    fn set_off(&mut self, p: Power) {
        self.off = p;
        if let Err(e) = ble::set_off_mode(p) {
            self.warn(format!("Could not save setting: {e}"));
        }
    }

    fn with(&mut self, addr: &str, f: impl FnOnce(&mut Row)) {
        if let Some(r) = self.rows.iter_mut().find(|r| r.addr == addr) {
            f(r);
        }
    }

    fn on_event(&mut self, e: Event) {
        match e {
            Event::Found(found) => {
                self.scanning = false;
                let n = found.len();
                for s in found {
                    if !self.rows.iter().any(|r| r.addr == s.addr) {
                        self.rows.push(row(s.addr.clone(), s.name));
                    }
                    self.send(&s.addr, Cmd::Refresh);
                }
                self.save();
                if n == 0 {
                    self.warn("No base stations found. Are they plugged in and within ~10 m?");
                } else {
                    self.say(format!("Found {n} station(s)."));
                }
            }
            Event::Survey(Ok(others)) => {
                self.surveying = false;
                self.say(format!(
                    "Interference check done: {} other station(s) in range.",
                    others.len()
                ));
                self.neighbours = Some(others);
            }
            Event::Survey(Err(e)) => {
                self.surveying = false;
                self.warn(format!("Interference check failed: {e}"));
            }
            Event::ScanFailed(e) => {
                self.scanning = false;
                self.warn(format!("Scan failed: {e}. Is bluetooth.service running?"));
            }
            Event::Power(addr, p) => self.with(&addr, |r| r.power = Some(p)),
            Event::Channel(addr, c) => self.with(&addr, |r| r.channel = Some(c)),
            Event::Info(addr, info) => self.with(&addr, |r| r.info = info),
            Event::Done(addr, err) => {
                let mut msg = None;
                self.with(&addr, |r| {
                    r.busy = r.busy.saturating_sub(1);
                    if let Some(e) = err {
                        msg = Some(format!("{}: {e}", r.name));
                        r.error = Some(e);
                    }
                });
                if let Some(m) = msg {
                    self.warn(m);
                } else if !self.status_bad && self.rows.iter().all(|r| r.busy == 0) {
                    self.say("Ready.");
                }
            }
        }
    }

    fn apply(&mut self, addr: String, act: Act) {
        match act {
            Act::Run(cmd) => self.send(&addr, cmd),
            Act::Off => self.send(&addr, Cmd::Power(self.off)),
            Act::Rename(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    self.with(&addr, |r| r.name = text);
                    self.save();
                }
                self.rename = None;
            }
            Act::Details => self.with(&addr, |r| r.details = !r.details),
            Act::Forget => {
                self.rows.retain(|r| r.addr != addr);
                self.save();
                self.say("Station forgotten. Scan to add it back.");
            }
        }
    }
}

// ---------------------------------------------------------------- drawing

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        while let Ok(e) = self.rx.try_recv() {
            self.on_event(e);
        }
        let t = ui.input(|i| i.time);

        egui::Frame::new()
            .inner_margin(Margin::symmetric(28, 22))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.header(ui);
                ui.add_space(18.0);
                if self.rows.is_empty() {
                    self.empty(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        self.all_stations(ui);
                        ui.add_space(16.0);
                        self.channel_map(ui);
                        ui.add_space(16.0);
                        let mut acts = Vec::new();
                        for r in &self.rows {
                            let ctx = CardCtx {
                                photo: &self.photo,
                                off: self.off,
                                t,
                            };
                            if let Some(a) = card(ui, r, &ctx, &mut self.rename) {
                                acts.push((r.addr.clone(), a));
                            }
                            ui.add_space(10.0);
                        }
                        for (addr, a) in acts {
                            self.apply(addr, a);
                        }
                    });
            });

        let moving = self.scanning
            || self
                .rows
                .iter()
                .any(|r| r.busy > 0 || r.power.is_some_and(|p| p != Power::On));
        if moving || self.surveying {
            // 30 fps is plenty for LED pulses and spinners.
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("LIGHTHOUSE")
                        .font(display(34.0))
                        .color(TEXT)
                        .extra_letter_spacing(4.0),
                );
                let color = if self.status_bad { BAD } else { DIM };
                ui.label(RichText::new(&self.status).monospace().color(color));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.scanning { "SCANNING" } else { "SCAN" };
                if ui
                    .add_enabled(!self.scanning, button(label, LASER, BG))
                    .clicked()
                {
                    self.scan();
                }
                if self.scanning {
                    ui.add(egui::Spinner::new().color(LASER));
                }
                if !self.rows.is_empty() && ui.add(button("REFRESH", PANEL, TEXT)).clicked() {
                    self.send_all(Cmd::Refresh);
                }
            });
        });
        let r = ui.available_rect_before_wrap();
        let y = r.top() + 10.0;
        ui.painter().hline(r.x_range(), y, Stroke::new(1.0, LINE));
        ui.painter()
            .hline(r.left()..=r.left() + 56.0, y, Stroke::new(2.0, LASER));
        ui.add_space(12.0);
    }

    fn empty(&mut self, ui: &mut Ui) {
        ui.add_space(70.0);
        ui.vertical_centered(|ui| {
            let title = if self.scanning {
                "LISTENING FOR BASE STATIONS"
            } else {
                "NO BASE STATIONS YET"
            };
            ui.label(
                RichText::new(title)
                    .font(display(26.0))
                    .color(TEXT)
                    .extra_letter_spacing(2.0),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("Plug them in and keep them within ~10 m of this PC.").color(DIM),
            );
            ui.label(
                RichText::new("systemctl enable --now bluetooth")
                    .monospace()
                    .color(DIM),
            );
            ui.add_space(18.0);
            if !self.scanning
                && ui
                    .add(button("SCAN", LASER, BG).min_size(vec2(140.0, 38.0)))
                    .clicked()
            {
                self.scan();
            }
        });
    }

    /// The "all devices" row: twin photo, group power, and what the red button means.
    fn all_stations(&mut self, ui: &mut Ui) {
        let all = |p: Power| !self.rows.is_empty() && self.rows.iter().all(|r| r.power == Some(p));
        let (all_on, all_off) = (all(Power::On), all(self.off));

        egui::Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, LINE))
            .corner_radius(6)
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    twin_photo(ui, &self.photo);
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("ALL STATIONS")
                                .font(display(22.0))
                                .color(LASER)
                                .extra_letter_spacing(1.5),
                        );
                        ui.label(
                            RichText::new(format!("{} saved", self.rows.len()))
                                .monospace()
                                .color(DIM),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if power_button(ui, true, all_on)
                                .on_hover_text("All on")
                                .clicked()
                            {
                                self.send_all(Cmd::Power(Power::On));
                            }
                            ui.add_space(14.0);
                            let tip = format!("All to {}", self.off.label().to_lowercase());
                            if power_button(ui, false, all_off)
                                .on_hover_text(tip)
                                .clicked()
                            {
                                self.send_all(Cmd::Power(self.off));
                            }
                        });
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        ui.vertical(|ui| {
                            ui.label(caps("RED BUTTON PUTS STATIONS IN"));
                            ui.horizontal(|ui| {
                                for p in [Power::Standby, Power::Sleep] {
                                    let lit = self.off == p;
                                    let (fill, fg) =
                                        if lit { (color(Some(p)), BG) } else { (BG, DIM) };
                                    if ui
                                        .add(button(&p.label().to_uppercase(), fill, fg))
                                        .clicked()
                                    {
                                        self.set_off(p);
                                    }
                                }
                            });
                            ui.label(
                                RichText::new(if self.off == Power::Standby {
                                    "Rotor keeps spinning. Wakes in ~2 s."
                                } else {
                                    "Rotor stops. Quiet, wakes in ~8 s."
                                })
                                .color(DIM),
                            );
                        });
                    });
                });
            });
    }

    fn channel_map(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(caps("CHANNEL MAP"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.surveying {
                    "CHECKING"
                } else {
                    "CHECK INTERFERENCE"
                };
                let tip = "Find every base station Bluetooth can hear, including other rooms' and \
                           neighbours', read their channels and suggest free ones for yours.";
                if ui
                    .add_enabled(!self.surveying, button(label, PANEL, LASER))
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.survey();
                }
                if self.surveying {
                    ui.add(egui::Spinner::new().color(LASER));
                }
            });
        });
        let none = Vec::new();
        let others = self.neighbours.as_ref().unwrap_or(&none);
        let w = ui.available_width();
        let gap = 4.0;
        let cell = (w - gap * 15.0) / 16.0;
        let (rect, _) = ui.allocate_exact_size(vec2(w, 46.0), Sense::hover());
        let p = ui.painter().clone();
        for ch in 1..=16u8 {
            let x = rect.left() + f32::from(ch - 1) * (cell + gap);
            let r = Rect::from_min_size(pos2(x, rect.top()), vec2(cell, rect.height()));
            let users: Vec<&Row> = self.rows.iter().filter(|r| r.channel == Some(ch)).collect();
            let theirs: Vec<&ble::Seen> = others.iter().filter(|s| s.channel == Some(ch)).collect();
            let (fill, stroke, fg) = match (users.len(), theirs.len()) {
                (0, 0) => (BG, LINE, DIM),
                (0, _) => (BG, LINE, TEXT),
                (1, 0) => (PANEL, LINE, TEXT),
                (1, _) => (LASER.gamma_multiply(0.15), LASER, LASER),
                _ => (BAD.gamma_multiply(0.18), BAD, BAD),
            };
            p.rect_filled(r, 3, fill);
            p.rect_stroke(r, 3, Stroke::new(1.0, stroke), StrokeKind::Inside);
            p.text(
                pos2(r.center().x, r.top() + 14.0),
                Align2::CENTER_CENTER,
                format!("{ch:02}"),
                FontId::monospace(11.0),
                fg,
            );
            // Ours are filled dots, other stations are rings.
            let n = (users.len() + theirs.len()) as f32;
            let dot = |i: usize| {
                pos2(
                    r.center().x + (i as f32 - (n - 1.0) / 2.0) * 8.0,
                    r.bottom() - 12.0,
                )
            };
            for (i, u) in users.iter().enumerate() {
                p.circle_filled(dot(i), 3.0, color(u.power));
            }
            for (i, _) in theirs.iter().enumerate() {
                p.circle_stroke(dot(users.len() + i), 2.6, Stroke::new(1.2, DIM));
            }
            if !users.is_empty() || !theirs.is_empty() {
                let mut tip: Vec<String> = users
                    .iter()
                    .map(|u| format!("{} (yours)", u.name))
                    .collect();
                tip.extend(
                    theirs
                        .iter()
                        .map(|s| format!("{} (other, {})", s.name, dbm(s.rssi))),
                );
                ui.interact(r, ui.id().with(("ch", ch)), Sense::hover())
                    .on_hover_text(tip.join("\n"));
            }
        }
        self.suggestions(ui);
    }

    /// Channel moves from `ble::plan`: fixes duplicates always, and avoids
    /// other stations' channels once an interference check ran.
    fn suggestions(&mut self, ui: &mut Ui) {
        let none = Vec::new();
        let others = self.neighbours.as_ref().unwrap_or(&none);
        let taken: Vec<(u8, Option<i16>)> = others
            .iter()
            .filter_map(|s| Some((s.channel?, s.rssi)))
            .collect();
        let mine: Vec<Option<u8>> = self.rows.iter().map(|r| r.channel).collect();
        let moves: Vec<(String, u8)> = self
            .rows
            .iter()
            .zip(ble::plan(&mine, &taken))
            .filter_map(|(r, to)| Some((r.addr.clone(), to?)))
            .collect();

        if moves.is_empty() {
            if let Some(others) = &self.neighbours {
                ui.add_space(6.0);
                let unread = others.iter().filter(|s| s.channel.is_none()).count();
                let mut msg = format!(
                    "All clear. {} other station(s) in range, none share your channels.",
                    others.len()
                );
                if unread > 0 {
                    msg += &format!(" {unread} could not be read.");
                }
                ui.label(RichText::new(msg).color(ON));
            }
            return;
        }

        ui.add_space(10.0);
        let mut apply = false;
        egui::Frame::new()
            .fill(LASER.gamma_multiply(0.08))
            .stroke(Stroke::new(1.0, LASER))
            .corner_radius(6)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(caps("SUGGESTED CHANNELS"));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        apply = ui.add(button("APPLY", LASER, BG)).clicked();
                    });
                });
                for (addr, to) in &moves {
                    let Some(r) = self.rows.iter().find(|r| &r.addr == addr) else {
                        continue;
                    };
                    let cur = r.channel.unwrap_or(0);
                    let clash: Vec<String> = self
                        .rows
                        .iter()
                        .filter(|o| o.addr != r.addr && o.channel == r.channel)
                        .map(|o| o.name.clone())
                        .chain(
                            others
                                .iter()
                                .filter(|s| s.channel == r.channel)
                                .map(|s| format!("{} ({})", s.name, dbm(s.rssi))),
                        )
                        .collect();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&r.name).font(display(15.0)).color(TEXT));
                        ui.label(
                            RichText::new(format!("CH {cur:02}  →  CH {to:02}"))
                                .monospace()
                                .color(LASER),
                        );
                        ui.label(
                            RichText::new(format!("shares with {}", clash.join(", "))).color(DIM),
                        );
                    });
                }
            });
        if apply {
            for (addr, ch) in moves {
                self.send(&addr, Cmd::Channel(ch));
            }
        }
    }
}

fn dbm(rssi: Option<i16>) -> String {
    rssi.map_or("? dBm".into(), |r| format!("{r} dBm"))
}

struct CardCtx<'a> {
    photo: &'a TextureHandle,
    off: Power,
    t: f64,
}

fn card(ui: &mut Ui, row: &Row, cx: &CardCtx, rename: &mut Option<Rename>) -> Option<Act> {
    let mut act = None;
    let edge = if row.error.is_some() {
        BAD.gamma_multiply(0.7)
    } else {
        LINE
    };
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(6)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                photo(ui, cx.photo, row, cx.t);
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        match rename {
                            Some(rn) if rn.addr == row.addr => {
                                let edit = egui::TextEdit::singleline(&mut rn.text)
                                    .font(display(20.0))
                                    .desired_width(240.0);
                                let resp = ui.add(edit);
                                if std::mem::take(&mut rn.focus) {
                                    resp.request_focus();
                                }
                                if resp.lost_focus() {
                                    // Escape cancels: an empty name is ignored.
                                    let keep = !ui.input(|i| i.key_pressed(Key::Escape));
                                    let text = if keep { rn.text.clone() } else { String::new() };
                                    act = Some(Act::Rename(text));
                                }
                            }
                            _ => {
                                let name = egui::Label::new(
                                    RichText::new(&row.name).font(display(22.0)).color(LASER),
                                )
                                .sense(Sense::click());
                                if ui.add(name).on_hover_text("Click to rename").clicked() {
                                    *rename = Some(Rename {
                                        addr: row.addr.clone(),
                                        text: row.name.clone(),
                                        focus: true,
                                    });
                                }
                            }
                        }
                        ui.add_space(6.0);
                        let state = row.power.map_or("Unknown".into(), Power::label);
                        ui.label(
                            RichText::new(state.to_uppercase())
                                .font(display(13.0))
                                .color(color(row.power))
                                .extra_letter_spacing(1.5),
                        );
                        if row.busy > 0 {
                            ui.add(egui::Spinner::new().size(14.0).color(LASER));
                        }
                    });
                    let mut meta = row.addr.clone();
                    if let Some((_, fw)) = row.info.iter().find(|(l, _)| *l == "Firmware") {
                        meta += &format!("   fw {fw}");
                    }
                    ui.label(RichText::new(meta).monospace().color(DIM));
                    if let Some(e) = &row.error {
                        ui.label(RichText::new(e).color(BAD));
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        let on = row.power == Some(Power::On);
                        if power_button(ui, true, on).on_hover_text("On").clicked() {
                            act = Some(Act::Run(Cmd::Power(Power::On)));
                        }
                        ui.add_space(8.0);
                        let off = row.power == Some(cx.off);
                        if power_button(ui, false, off)
                            .on_hover_text(cx.off.label())
                            .clicked()
                        {
                            act = Some(Act::Off);
                        }
                        ui.add_space(14.0);
                        let mut ch = row.channel.unwrap_or(0);
                        let shown = row.channel.map_or("CH --".into(), |c| format!("CH {c:02}"));
                        egui::ComboBox::from_id_salt(("ch", &row.addr))
                            .width(76.0)
                            .selected_text(RichText::new(shown).monospace())
                            .show_ui(ui, |ui| {
                                for c in 1..=16 {
                                    ui.selectable_value(&mut ch, c, format!("Channel {c:02}"));
                                }
                            })
                            .response
                            .on_hover_text("Channel (optical sync mode). Every station in a room needs its own.");
                        if ch != 0 && Some(ch) != row.channel {
                            act = Some(Act::Run(Cmd::Channel(ch)));
                        }
                        if ui
                            .add(button("IDENTIFY", BG, LASER))
                            .on_hover_text("Blink this station's LED")
                            .clicked()
                        {
                            act = Some(Act::Run(Cmd::Identify));
                        }
                        let (fill, fg) = if row.details { (TEXT, BG) } else { (BG, DIM) };
                        if ui.add(button("DETAILS", fill, fg)).clicked() {
                            act = Some(Act::Details);
                        }
                        if ui
                            .add(button("READ", BG, DIM))
                            .on_hover_text("Re-read power, channel and device info")
                            .clicked()
                        {
                            act = Some(Act::Run(Cmd::Refresh));
                        }
                        if ui.add(button("FORGET", BG, DIM)).clicked() {
                            act = Some(Act::Forget);
                        }
                    });
                });
            });
            if row.details {
                details(ui, row);
            }
        });
    act
}

fn details(ui: &mut Ui, row: &Row) {
    ui.add_space(12.0);
    let r = ui.available_rect_before_wrap();
    ui.painter()
        .hline(r.x_range(), r.top(), Stroke::new(1.0, LINE));
    ui.add_space(10.0);
    let channel = row.channel.map_or("--".into(), |c| c.to_string());
    let mode = row.power.map_or("--".into(), Power::label);
    let fields = [
        ("Local name", row.name.as_str()),
        ("Address", row.addr.as_str()),
        ("Mode", mode.as_str()),
        ("Channel", channel.as_str()),
    ];
    let info = row.info.iter().map(|(l, v)| (*l, v.as_str()));
    egui::Grid::new(("info", &row.addr))
        .num_columns(2)
        .spacing(vec2(24.0, 6.0))
        .show(ui, |ui| {
            for (label, value) in fields.into_iter().chain(info) {
                ui.label(caps(&label.to_uppercase()));
                ui.label(RichText::new(value).monospace().color(TEXT));
                ui.end_row();
            }
        });
    if row.info.is_empty() {
        ui.label(RichText::new("Press READ to load device info.").color(DIM));
    }
}

/// Where the status LED sits on assets/basestation.png, as a fraction of the image.
const LED: egui::Vec2 = vec2(65.0 / 192.0, 69.0 / 192.0);
const FULL_UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// Photo of the station with its front LED drawn like the real one:
/// green when on, slow blue pulse when asleep or in standby.
fn photo(ui: &mut Ui, tex: &TextureHandle, row: &Row, t: f64) {
    let size = 96.0;
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter();
    let col = color(row.power);
    let awake = matches!(row.power, Some(Power::On | Power::Booting | Power::Standby));

    // Light spill behind the unit so the black casing reads against the panel.
    if awake {
        for (r, a) in [(46.0, 0.05), (36.0, 0.06), (26.0, 0.07)] {
            p.circle_filled(rect.center(), r, col.gamma_multiply(a));
        }
    }
    let tint = if awake {
        Color32::WHITE
    } else {
        Color32::from_gray(150)
    };
    p.image(tex.id(), rect, FULL_UV, tint);

    let led = rect.min + LED * size;
    // Real unit: solid green when on, slow blue pulse when asleep or in standby.
    let breathe = |period: f64| (0.5 - 0.5 * (t * TAU / period).cos()) as f32;
    let (led_col, glow) = match row.power {
        Some(Power::On) => (ON, 1.0),
        Some(Power::Booting) => (ON, breathe(0.6)),
        Some(Power::Standby) => (STANDBY, 0.15 + 0.85 * breathe(2.5)),
        Some(Power::Sleep) => (STANDBY, 0.1 + 0.9 * breathe(4.0)),
        _ => (DIM, 0.0),
    };
    // Cover the LED that is lit in the photo, then draw ours.
    p.circle_filled(led, 2.2, Color32::from_gray(20));
    if glow > 0.0 {
        for (r, a) in [(9.0, 0.10), (6.0, 0.18), (3.5, 0.45), (1.8, 1.0)] {
            p.circle_filled(led, r, led_col.gamma_multiply(a * glow));
        }
    }
}

/// Two stations, one behind the other, for the "all stations" row.
fn twin_photo(ui: &mut Ui, tex: &TextureHandle) {
    let (rect, _) = ui.allocate_exact_size(vec2(96.0, 96.0), Sense::hover());
    let p = ui.painter();
    let back = Rect::from_min_size(rect.min + vec2(2.0, 6.0), vec2(66.0, 66.0));
    let front = Rect::from_min_size(rect.min + vec2(28.0, 28.0), vec2(66.0, 66.0));
    p.image(tex.id(), back, FULL_UV, Color32::from_gray(140));
    p.image(tex.id(), front, FULL_UV, Color32::WHITE);
}

/// Round glowing power button like the phone app: green "I" or red "O".
fn power_button(ui: &mut Ui, on: bool, lit: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
    let p = ui.painter();
    let c = rect.center();
    let col = if on { ON } else { BAD };
    let k = if resp.hovered() {
        1.0
    } else if lit {
        0.85
    } else {
        0.55
    };
    for (r, a) in [(22.0, 0.10), (19.5, 0.22)] {
        p.circle_filled(c, r, col.gamma_multiply(a * k));
    }
    p.circle_stroke(c, 15.0, Stroke::new(6.0, col.gamma_multiply(k)));
    p.circle_filled(c, 12.0, BG);
    let glyph = Stroke::new(2.0, col);
    if on {
        p.line_segment([c - vec2(0.0, 6.5), c + vec2(0.0, 6.5)], glyph);
    } else {
        p.circle_stroke(c, 5.5, glyph);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn color(p: Option<Power>) -> Color32 {
    match p {
        Some(Power::On) => ON,
        Some(Power::Booting) => LASER,
        Some(Power::Standby) => STANDBY,
        Some(Power::Sleep) => SLEEP,
        _ => DIM,
    }
}

// ---------------------------------------------------------------- style

fn display(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("display".into()))
}

fn caps(text: &str) -> RichText {
    RichText::new(text)
        .font(display(12.0))
        .color(DIM)
        .extra_letter_spacing(2.0)
}

fn button(text: &str, fill: Color32, fg: Color32) -> egui::Button<'static> {
    let label = RichText::new(text)
        .font(display(14.0))
        .color(fg)
        .extra_letter_spacing(1.2);
    egui::Button::new(label)
        .fill(fill)
        .min_size(vec2(0.0, 30.0))
}

fn style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let regular = include_bytes!("../assets/FiraSansCondensed-Regular.ttf");
    let bold = include_bytes!("../assets/FiraSansCondensed-ExtraBold.ttf");
    fonts
        .font_data
        .insert("fira".into(), Arc::new(FontData::from_static(regular)));
    fonts
        .font_data
        .insert("fira-bold".into(), Arc::new(FontData::from_static(bold)));
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    prop.insert(0, "fira".into());
    let fallback = prop.clone();
    fonts.families.insert(
        FontFamily::Name("display".into()),
        std::iter::once("fira-bold".to_string())
            .chain(fallback)
            .collect(),
    );
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.text_styles
            .insert(TextStyle::Body, FontId::proportional(15.0));
        s.text_styles
            .insert(TextStyle::Button, FontId::proportional(14.0));
        s.text_styles
            .insert(TextStyle::Monospace, FontId::monospace(12.0));
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 5.0);

        let v = &mut s.visuals;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.extreme_bg_color = BG;
        v.faint_bg_color = PANEL;
        v.selection.bg_fill = LASER.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, LASER);
        let w = &mut v.widgets;
        for state in [
            &mut w.noninteractive,
            &mut w.inactive,
            &mut w.hovered,
            &mut w.active,
            &mut w.open,
        ] {
            state.corner_radius = CornerRadius::same(3);
        }
        w.noninteractive.fg_stroke.color = TEXT;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.inactive.bg_fill = BG;
        w.inactive.weak_bg_fill = BG;
        w.inactive.bg_stroke = Stroke::new(1.0, LINE);
        w.inactive.fg_stroke.color = TEXT;
        w.hovered.bg_fill = PANEL;
        w.hovered.weak_bg_fill = PANEL;
        w.hovered.bg_stroke = Stroke::new(1.0, LASER);
        w.active.bg_stroke = Stroke::new(1.5, LASER);
        w.open.bg_stroke = Stroke::new(1.0, LASER);
    });
}
