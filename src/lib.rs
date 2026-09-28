use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget},
};

pub use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, SystemTime};

// how often we poll for keyboard input / redraw. 16ms ≈ 60Hz.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);
/// how often the background "jiggle" thread perturbs wavelength/amplitude.
const JIGGLE_INTERVAL: Duration = Duration::from_millis(150);
const JIGGLE_WAVELENGTH_MIN: f32 = 20.0;
const JIGGLE_WAVELENGTH_MAX: f32 = 80.0;
const JIGGLE_AMPLITUDE_BASE: f32 = 6.0;
const JIGGLE_AMPLITUDE_SWING: f32 = 3.0;
/// hard cap on rendered thickness so a bad config can't turn the wave into a
/// wall of characters or waste time drawing off screen rows.
const MAX_THICKNESS: u16 = 20;
/// smallest wavelength we'll accept => anything <= 0 would divide-by-zero.
const MIN_WAVELENGTH: f32 = 1.0;

const HIGH_INTENSITY_THRESHOLD: f32 = 0.7;
const MED_INTENSITY_THRESHOLD: f32 = 0.3;
const BOLD_SYMBOL_THRESHOLD: f32 = 0.8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)] // missing fields in config.json fall back to default instead of failing the whole parse
struct Sinewave {
    wavelength: f32,
    amplitude: f32,
    speed: f32,
    thickness: u16,

    theme: Theme,

    #[serde(skip)]
    time: f32,
}

impl Default for Sinewave {
    fn default() -> Self {
        Self {
            wavelength: 40.0,
            amplitude: 10.0,
            speed: 0.01,
            time: 0.0,
            thickness: 0,
            theme: Theme {
                high_intensity: [0, 0, 0],
                med_intensity: [0, 0, 0],
                low_intensity: [0, 0, 0],
            },
        }
    }
}

impl Sinewave {
    // if this running for days, time could theoretically degrade the performance of the wave so,
    // we're using this cool method
    fn advance_time(&mut self) {
        self.time = (self.speed + self.time).rem_euclid(1.);
    }

    /// clamp user/config-supplied values into ranges that can't crash or
    /// degenerate the render (e.g. division by zero).
    fn sanitize(&mut self) {
        if !self.wavelength.is_finite() || self.wavelength < MIN_WAVELENGTH {
            self.wavelength = MIN_WAVELENGTH;
        }
        if !self.amplitude.is_finite() {
            self.amplitude = 0.0;
        }
        self.thickness = self.thickness.min(MAX_THICKNESS);
    }

    fn get_y_offset(&self, x: f32) -> f32 {
        let space_factor = x / self.wavelength;
        let angle = 2.0 * std::f32::consts::PI * (space_factor - self.time);
        self.amplitude * angle.sin()
    }
}

/// the app name used for the config directory: `~/.config/wavesaver/config.json`.
const APP_NAME: &str = "wavesaver";

/// resolves the config file path the XDG way: `$XDG_CONFIG_HOME/wavesaver/config.json`
/// if `XDG_CONFIG_HOME` is set (common under nix-darwin/home-manager), otherwise
/// `~/.config/wavesaver/config.json`. falls back to a CWD-relative path only if
/// we can't figure out a home directory at all
fn config_path() -> PathBuf {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    match config_home {
        Some(dir) => dir.join(APP_NAME).join("config.json"),
        None => PathBuf::from("config.json"),
    }
}

/// watches `config.json` and only re-reads/parses it when its mtime changes,
/// instead of doing file I/O + JSON parsing on every single frame.
struct ConfigWatcher {
    path: PathBuf,
    last_modified: Option<SystemTime>,
}

enum ConfigPoll {
    Unchanged,
    Loaded(Sinewave),
    ParseError(String),
}

impl ConfigWatcher {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            last_modified: None,
        }
    }

    fn poll(&mut self) -> ConfigPoll {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            // no config file present is a normal state, not an error.
            return ConfigPoll::Unchanged;
        };
        let Ok(modified) = metadata.modified() else {
            return ConfigPoll::Unchanged;
        };
        if self.last_modified == Some(modified) {
            return ConfigPoll::Unchanged;
        }
        self.last_modified = Some(modified);

        match File::open(&self.path) {
            Ok(file) => match serde_json::from_reader::<_, Sinewave>(BufReader::new(file)) {
                Ok(mut config) => {
                    config.sanitize();
                    ConfigPoll::Loaded(config)
                }
                Err(err) => ConfigPoll::ParseError(err.to_string()),
            },
            Err(err) => ConfigPoll::ParseError(err.to_string()),
        }
    }
}

struct WaveWidget<'a> {
    wave: &'a Sinewave,
}

impl<'a> Widget for WaveWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let middle_y = area.y + (area.height / 2);

        for x_cell in area.x..area.right() {
            let relative_x = (x_cell - area.x) as f32;
            let y_offset = self.wave.get_y_offset(relative_x);
            let target_y = (middle_y as f32 - y_offset.round()) as i32;

            if target_y < area.y as i32 || target_y >= area.bottom() as i32 {
                continue;
            }

            let cell_idx_y = target_y as u16;
            let signal_intensity = if self.wave.amplitude.abs() < f32::EPSILON {
                0.0
            } else {
                (y_offset.abs() / self.wave.amplitude.abs()).clamp(0.0, 1.0)
            };

            let wave_color = if signal_intensity > HIGH_INTENSITY_THRESHOLD {
                self.wave.theme.high()
            } else if signal_intensity > MED_INTENSITY_THRESHOLD {
                self.wave.theme.med()
            } else {
                self.wave.theme.low()
            };

            let symbol = if signal_intensity > BOLD_SYMBOL_THRESHOLD {
                "#"
            } else {
                "&"
            };
            let style = Style::default().fg(wave_color).add_modifier(Modifier::BOLD);

            if let Some(cell) = buf.cell_mut(Position::new(x_cell, cell_idx_y)) {
                cell.set_symbol(symbol);
                cell.set_style(style);
            }

            // thicken the line by painting `thickness` extra cells above and
            // below the sample point, staying inside the widget's area
            for i in 1..=self.wave.thickness {
                let upper_y = cell_idx_y.saturating_sub(i);
                if upper_y >= area.y && upper_y != cell_idx_y {
                    if let Some(cell) = buf.cell_mut(Position::new(x_cell, upper_y)) {
                        cell.set_symbol(symbol);
                        cell.set_style(style);
                    }
                }

                let lower_y = cell_idx_y.saturating_add(i);
                if lower_y < area.bottom() {
                    if let Some(cell) = buf.cell_mut(Position::new(x_cell, lower_y)) {
                        cell.set_symbol(symbol);
                        cell.set_style(style);
                    }
                }
            }
        }
    }
}

pub struct App {
    sinewave: Sinewave,
    exit: bool,
    jiggle_receiver: Receiver<(f32, f32)>,
    config_watcher: ConfigWatcher,
    /// set when config.json exists but fails to parse, so the problem is
    /// visible in the ui instead of being silently swallowed
    config_error: Option<String>,
    debug: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub high_intensity: [u8; 3],
    pub med_intensity: [u8; 3],
    pub low_intensity: [u8; 3],
}

impl Theme {
    pub fn high(&self) -> Color {
        let [r, g, b] = self.high_intensity;
        Color::Rgb(r, g, b)
    }

    pub fn med(&self) -> Color {
        let [r, g, b] = self.med_intensity;
        Color::Rgb(r, g, b)
    }

    pub fn low(&self) -> Color {
        let [r, g, b] = self.low_intensity;
        Color::Rgb(r, g, b)
    }
}

#[derive(Parser, Debug)]
#[command(author, about, long_about = None)]
pub struct Args {
    #[arg(short, long)]
    debug: bool,
}

impl App {
    pub fn new(jiggle_receiver: Receiver<(f32, f32)>, args: Args) -> Self {
        Self {
            sinewave: Sinewave::default(),
            exit: false,
            jiggle_receiver,
            config_watcher: ConfigWatcher::new(config_path()),
            config_error: None,
            debug: args.debug,
        }
    }

    pub fn run(&mut self, term: &mut DefaultTerminal) -> color_eyre::Result<()> {
        while !self.exit {
            term.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
            self.reload_config_if_changed();
            self.sinewave.advance_time();
            self.apply_latest_jiggle();
        }
        Ok(())
    }

    fn reload_config_if_changed(&mut self) {
        match self.config_watcher.poll() {
            ConfigPoll::Unchanged => {}
            ConfigPoll::Loaded(config) => {
                // carry over the runtime-only timeline value; everything else
                // comes from the file
                let saved_time = self.sinewave.time;
                self.sinewave = config;
                self.sinewave.time = saved_time;
                self.config_error = None;
            }
            ConfigPoll::ParseError(err) => {
                // keep running with the last good settings, but surface
                // the problem instead of pretending nothing happened
                self.config_error = Some(err);
            }
        }
    }

    /// drain the jiggle channel and keep only the most recent update, so a
    /// slow frame doesn't leave a backlog of stale values queued up
    fn apply_latest_jiggle(&mut self) {
        let mut latest = None;
        while let Ok(new_vars) = self.jiggle_receiver.try_recv() {
            latest = Some(new_vars);
        }
        if let Some((wavelength, amplitude)) = latest {
            self.sinewave.wavelength = wavelength;
            self.sinewave.amplitude = amplitude;
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        let wave_widget = WaveWidget {
            wave: &self.sinewave,
        };
        frame.render_widget(wave_widget, area);

        if self.debug {
            let info_string = match &self.config_error {
                Some(err) => format!(" config.json error: {err} | press 'q' to exit"),
                None => format!(
                    " wavelength: {:.2} | amplitude: {:.1} | thickness: {} | press 'q' to exit",
                    self.sinewave.wavelength, self.sinewave.amplitude, self.sinewave.thickness
                ),
            };

            // prolly gonna add customization to the debug stuff
            let info_color = if self.config_error.is_some() {
                Color::Red
            } else {
                Color::Magenta
            };
            let debug_paragraph = Paragraph::new(info_string)
                .style(Style::default().fg(info_color).add_modifier(Modifier::DIM));

            let header_area = Rect::new(area.x, area.y, area.width, 1);
            frame.render_widget(debug_paragraph, header_area);
        }
    }

    // handles all sort of key events, probably gonna add more soon
    fn handle_events(&mut self) -> color_eyre::Result<()> {
        if crossterm::event::poll(EVENT_POLL_INTERVAL)? {
            if let Event::Key(key_event) = crossterm::event::read()? {
                if key_event.kind == KeyEventKind::Press {
                    self.handle_key_event(key_event);
                }
            }
        }
        Ok(())
    }

    // handles 'q' key which is the escape one
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if let KeyCode::Char('q') = key_event.code {
            self.exit = true;
        }
    }
}

/// spawns the background thread that periodically nudges wavelength and
/// amplitude, giving the wave a bit of organic drift
pub fn spawn_jiggle_thread() -> Receiver<(f32, f32)> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut current_wavelength = 60.0_f32;
        let mut counter = 0.0_f32;

        loop {
            std::thread::sleep(JIGGLE_INTERVAL);
            counter += 0.1;

            let variance: f32 = rand::random_range(-1.5..=1.5);
            current_wavelength =
                (current_wavelength + variance).clamp(JIGGLE_WAVELENGTH_MIN, JIGGLE_WAVELENGTH_MAX);
            let current_amplitude =
                JIGGLE_AMPLITUDE_BASE + (counter.sin() * JIGGLE_AMPLITUDE_SWING);

            if tx.send((current_wavelength, current_amplitude)).is_err() {
                break;
            }
        }
    });

    rx
}
