#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use rand::Rng; 
use reqwest::blocking::Client;
use lazy_static::lazy_static; 
use serde_json; 
use std::time::Instant;
use rodio::{OutputStream, OutputStreamHandle};
use rodio::buffer::SamplesBuffer;

const ANIMATION_MIN_TIME: f32 = 10.0; 
const ANIMATION_MAX_TIME: f32 = 15.0;
const TARGET_SCROLL_ROWS: usize = 100; 
const BRAKING_POWER: i32 = 7; 
const ROW_HEIGHT: f32 = 80.0;           

const EU_COUNTRIES_LIST: &str = "DE,FR,PL,GB,UA,NL,CZ,SK,IT,ES,AT,BE,DK,SE,NO,FI,IE,TR"; 

lazy_static! {
    static ref EU_SET: std::collections::HashSet<String> = {
        EU_COUNTRIES_LIST.split(',').map(|s| s.to_string()).collect()
    };
}

#[derive(Deserialize, Debug, Clone)]
struct ApiAttributes {
    name: String,
    players: u32,
    #[serde(rename = "maxPlayers")]
    max_players: u32,
    details: ApiDetails,
    country: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct ApiDetails {
    map: Option<String>,
    #[serde(rename = "gameMode")]
    game_mode: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct ApiServerData {
    attributes: ApiAttributes,
}

#[derive(Deserialize, Debug, Clone)]
struct ApiLinks {
    next: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct ApiResponse {
    data: Vec<ApiServerData>,
    links: Option<ApiLinks>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerItem {
    name: String,
    players: u32,
    max_players: u32,
    map: String,
    mode: String,
    country: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RouletteState {
    Ready,
    Loading,
    Spinning,
    Finished,
}

fn fetch_roulette_servers(
    tx: Sender<Vec<ServerItem>>, 
    min_p: u32, 
    max_p: u32, 
) {
    let client = Client::new();
    let mut all_servers = Vec::new();
    let base_url = "https://api.battlemetrics.com/servers";
    let mut next_url = base_url.to_string();
    
    let mut pages_fetched = 0;
    const MAX_PAGES: u32 = 5; 

    let filters = [
        ("filter[game]", "squad"),
        ("filter[status]", "online"),
        ("page[size]", "100"),
        ("sort", "-players"),
    ];

    while !next_url.is_empty() && pages_fetched < MAX_PAGES {
        pages_fetched += 1;
        let mut request = client.get(&next_url);
        
        if next_url == base_url {
            request = request
                .query(&filters)
                .query(&[("filter[players][min]", min_p.to_string().as_str())])
                .query(&[("filter[players][max]", max_p.to_string().as_str())]);
        }

        match request.send() {
            Ok(resp) => { 
                if resp.status().is_success() {
                    let body_text = resp.text().unwrap_or_default();
                    if let Ok(json) = serde_json::from_str::<ApiResponse>(&body_text) {
                        next_url = json.links.as_ref().and_then(|l| l.next.clone()).unwrap_or_default();
                        for server_data in json.data {
                            let attr = server_data.attributes;
                            let country = attr.country.unwrap_or("??".to_string());
                            if !EU_SET.contains(&country) { continue; }
                            
                            all_servers.push(ServerItem {
                                name: attr.name,
                                players: attr.players,
                                max_players: attr.max_players,
                                map: attr.details.map.unwrap_or("Unknown".to_string()),
                                mode: attr.details.game_mode.unwrap_or("Unknown".to_string()),
                                country,
                            });
                        }
                    } else { next_url = String::new(); }
                } else { next_url = String::new(); }
            },
            Err(_) => { next_url = String::new(); }
        }
    }
    let _ = tx.send(all_servers);
}

struct RouletteApp {
    pub min_players: u32,
    pub max_players: u32,
    pub roulette_servers: Vec<ServerItem>,
    pub selected_server: Option<ServerItem>,
    pub roulette_state: RouletteState,
    pub roulette_rx: Option<Receiver<Vec<ServerItem>>>,
    pub spin_start_time: Option<Instant>, 
    pub current_scroll: f32,
    pub start_scroll: f32,
    pub target_scroll: f32,
    pub current_animation_duration: f32,
    pub _audio_stream: Option<OutputStream>, 
    pub audio_handle: Option<OutputStreamHandle>,
    pub click_samples: Vec<f32>, 
    pub last_sound_index: i32,
    pub needs_update: bool,
}

impl Default for RouletteApp {
    fn default() -> Self {
        let (_stream, audio_handle) = match OutputStream::try_default() {
            Ok((s, h)) => (Some(s), Some(h)),
            Err(e) => {
                println!("{}", e);
                (None, None)
            }
        };

        let sample_rate = 44100;
        let duration_ms = 20; 
        let num_samples = (sample_rate * duration_ms) / 1000;
        
        let mut click_samples = Vec::with_capacity(num_samples as usize);
        let mut rng = rand::thread_rng();
        let mut last_sample = 0.0; 

        for i in 0..num_samples {
            let raw_noise: f32 = rng.gen_range(-1.0..1.0);
            let filtered_noise = last_sample * 0.85 + raw_noise * 0.15;
            last_sample = filtered_noise;
            let decay = 1.0 - (i as f32 / num_samples as f32);
            let punchy_decay = decay.powf(2.0); 
            click_samples.push(filtered_noise * punchy_decay * 3.0);
        }

        Self {
            min_players: 60,
            max_players: 100,
            roulette_servers: Vec::new(),
            selected_server: None,
            roulette_state: RouletteState::Ready,
            roulette_rx: None,
            spin_start_time: None,
            current_scroll: 0.0,
            start_scroll: 0.0,
            target_scroll: 0.0,
            current_animation_duration: 10.0, 
            _audio_stream: _stream,
            audio_handle,
            click_samples, 
            last_sound_index: -1,
            needs_update: true,
        }
    }
}

impl RouletteApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(10.0, 15.0);
        cc.egui_ctx.set_style(style);
        Default::default()
    }
    
    fn start_fetch(&mut self, ctx: egui::Context) {
        if self.roulette_state == RouletteState::Loading { return; }
        self.roulette_servers.clear();
        self.selected_server = None;
        self.roulette_state = RouletteState::Loading;
        self.needs_update = false;

        let (tx, rx) = channel();
        self.roulette_rx = Some(rx);
        let (min, max) = (self.min_players, self.max_players);

        thread::spawn(move || {
            fetch_roulette_servers(tx, min, max);
            ctx.request_repaint();
        });
    }

    fn start_spin(&mut self) {
        if self.roulette_servers.is_empty() { return; }
        let mut rng = rand::thread_rng();
        
        let winner_idx = rng.gen_range(0..self.roulette_servers.len());
        self.selected_server = Some(self.roulette_servers[winner_idx].clone());
        
        self.current_animation_duration = rng.gen_range(ANIMATION_MIN_TIME..ANIMATION_MAX_TIME);
        
        let server_count = self.roulette_servers.len();
        let loops = (TARGET_SCROLL_ROWS / server_count).max(3);
        
        let offset: f32 = rng.gen_range(-30.0..30.0);

        let target_index_virtual = (loops * server_count) + winner_idx;
        
        self.target_scroll = (target_index_virtual as f32 * ROW_HEIGHT) + offset;
        self.start_scroll = 0.0;
        self.current_scroll = 0.0;
        
        self.last_sound_index = -1;

        self.spin_start_time = Some(Instant::now());
        self.roulette_state = RouletteState::Spinning;
    }

    fn ease_out_custom(&self, t: f32) -> f32 {
        if t >= 1.0 { return 1.0; }
        1.0 - (1.0 - t).powi(BRAKING_POWER)
    }

    fn play_click(&self) {
        if let Some(handle) = &self.audio_handle {
            let buffer = SamplesBuffer::new(1, 44100, self.click_samples.clone());
            let _ = handle.play_raw(buffer);
        }
    }

    fn roulette_ui(&mut self, ctx: &egui::Context) {
        ctx.set_visuals(egui::Visuals::dark());

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading(egui::RichText::new("🎰 SQUAD EU ROULETTE").size(28.0).strong().color(egui::Color32::GOLD));
            });
            ui.add_space(10.0);

            ui.group(|ui| {
                ui.style_mut().spacing.slider_width = 250.0; 
                ui.style_mut().spacing.interact_size.y = 30.0; 
                
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Гравці:").size(18.0));
                    if ui.add(egui::Slider::new(&mut self.min_players, 0..=100).text("мін")).changed() { self.needs_update = true; }
                    if ui.add(egui::Slider::new(&mut self.max_players, 0..=100).text("макс")).changed() { self.needs_update = true; }
                });
                
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    if ui.button("🔄 Оновити").clicked() { self.start_fetch(ctx.clone()); }
                    if self.needs_update { ui.colored_label(egui::Color32::YELLOW, "Дані застаріли!"); } 
                    else { ui.colored_label(egui::Color32::GREEN, format!("Серверів: {}", self.roulette_servers.len())); }
                });
            });
            
            ui.add_space(20.0);

            let btn_text = match self.roulette_state {
                RouletteState::Ready => "🎰 КРУТИТИ!",
                RouletteState::Loading => "⏳ ...",
                RouletteState::Spinning => "🌀 ...",
                RouletteState::Finished => "🎰 ЩЕ РАЗ!",
            };
            let can_spin = !self.needs_update && !matches!(self.roulette_state, RouletteState::Loading | RouletteState::Spinning) && !self.roulette_servers.is_empty();

            ui.vertical_centered(|ui| {
                if ui.add_enabled(can_spin, egui::Button::new(egui::RichText::new(btn_text).size(24.0).strong()).min_size(egui::vec2(250.0, 60.0))).clicked() {
                    self.start_spin();
                }
            });

            ui.add_space(20.0);
            
            let scroll_height = 320.0; 
            
            egui::Frame::canvas(ui.style()).fill(egui::Color32::from_black_alpha(230)).stroke(egui::Stroke::new(1.0, egui::Color32::DARK_GRAY)).inner_margin(0.0).show(ui, |ui| {
                let center_y = scroll_height / 2.0 - ROW_HEIGHT / 2.0;

                egui::ScrollArea::vertical()
                    .max_height(scroll_height)
                    .enable_scrolling(false)
                    .vertical_scroll_offset(self.current_scroll - center_y) 
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.style_mut().spacing.item_spacing.y = 0.0; 

                        if self.roulette_servers.is_empty() {
                            ui.allocate_space(egui::vec2(ui.available_width(), 320.0));
                            ui.centered_and_justified(|ui| { ui.label("Список порожній. Онови сервери!"); });
                        } else {
                            let server_count = self.roulette_servers.len();
                            let needed_rows = TARGET_SCROLL_ROWS + 10;
                            let repetitions = (needed_rows as f32 / server_count as f32).ceil() as usize + 2;

                            for _ in 0..repetitions {
                                for server in &self.roulette_servers {
                                    ui.allocate_ui(egui::vec2(ui.available_width(), ROW_HEIGHT), |ui| {
                                        ui.vertical_centered(|ui| {
                                            ui.add_space(4.0); 
                                            ui.group(|ui| {
                                                ui.set_width(ui.available_width() - 10.0);
                                                ui.vertical_centered(|ui| {
                                                    ui.add_space(2.0); 
                                                    ui.label(egui::RichText::new(&server.name).size(20.0).strong().color(egui::Color32::LIGHT_BLUE));
                                                    ui.horizontal_centered(|ui| {
                                                        ui.label(format!("🗺️ {}", server.map));
                                                        ui.add_space(10.0);
                                                        ui.label(egui::RichText::new(format!("👥 {}/{}", server.players, server.max_players)).color(egui::Color32::YELLOW));
                                                    });
                                                });
                                            });
                                        });
                                    });
                                }
                            }
                        }
                    });

                let rect = ui.min_rect();
                let line_y = rect.top() + scroll_height / 2.0;
                let painter = ui.painter();
                painter.line_segment([egui::pos2(rect.left(), line_y), egui::pos2(rect.right(), line_y)], egui::Stroke::new(3.0, egui::Color32::RED));
                painter.text(egui::pos2(rect.right() - 10.0, line_y), egui::Align2::RIGHT_CENTER, "◄", egui::FontId::proportional(30.0), egui::Color32::RED);
            });

            if self.roulette_state == RouletteState::Finished {
                if let Some(winner) = &self.selected_server {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.group(|ui| {
                            ui.set_min_width(300.0); 
                            ui.label(egui::RichText::new("🎉 ПЕРЕМОЖЕЦЬ:").size(16.0));
                            ui.add_space(5.0);
                            ui.label(egui::RichText::new(&winner.name).size(24.0).color(egui::Color32::GREEN).strong());
                            ui.add_space(5.0);
                            ui.label(egui::RichText::new(format!("Карта: {}", winner.map)).size(18.0).italics()); 
                            ui.add_space(10.0);
                            if ui.button("📋 Скопіювати назву").clicked() { ctx.output_mut(|o| o.copied_text = winner.name.clone()); }
                        });
                    });
                }
            }
        });
    }
}

impl eframe::App for RouletteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.roulette_rx {
            if let Ok(servers) = rx.try_recv() {
                self.roulette_servers = servers;
                if self.roulette_state == RouletteState::Loading {
                     self.roulette_state = if self.roulette_servers.is_empty() { RouletteState::Finished } else { RouletteState::Ready };
                }
                self.roulette_rx = None;
            }
        }
        
        if self.roulette_state == RouletteState::Spinning {
            if let Some(start) = self.spin_start_time {
                let elapsed = start.elapsed().as_secs_f32();
                if elapsed < self.current_animation_duration {
                    let t = elapsed / self.current_animation_duration;
                    let ease_t = self.ease_out_custom(t); 
                    
                    let new_scroll = self.start_scroll + (self.target_scroll - self.start_scroll) * ease_t;
                    
                    if (self.target_scroll - new_scroll).abs() < 0.5 {
                        self.current_scroll = self.target_scroll;
                        self.roulette_state = RouletteState::Finished;
                    } else {
                        self.current_scroll = new_scroll;
                        
                        let scroll_offset_for_sound = self.current_scroll + ROW_HEIGHT * 0.5;
                        let current_idx = (scroll_offset_for_sound / ROW_HEIGHT).floor() as i32;

                        if current_idx > self.last_sound_index {
                            self.play_click();
                            self.last_sound_index = current_idx;
                        }
                    }

                    ctx.request_repaint();
                } else {
                    self.current_scroll = self.target_scroll;
                    self.roulette_state = RouletteState::Finished;
                }
            }
        }
        self.roulette_ui(ctx);
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 950.0])
            .with_min_inner_size([600.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Squad EU Roulette",
        options,
        Box::new(|cc| Ok(Box::new(RouletteApp::new(cc)))),
    )
}