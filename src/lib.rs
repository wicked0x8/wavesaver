use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Paragraph, Widget},
};

pub use clap::Parser;
use noise::{NoiseFn, Perlin};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

// how often we poll for keyboard input / redraw. 16ms ≈ 60Hz.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// how far the noise time axis advances per frame; lower = slower drift
const NOISE_TIME_STEP: f64 = 0.003;
/// perlin output rarely gets near ±1, so stretch it before using it as a modulator
const NOISE_GAIN: f32 = 2.5;
/// how much perlin noise modulates the base amplitude over time (±50% of base)
const AMPLITUDE_VARIATION: f32 = 0.5;

/// how quickly the amplitude envelope changes along x. 0.02 => a new "hill" every ~50 columns
const ENVELOPE_SCALE: f32 = 0.02;
/// how strongly the envelope scales amplitude (0.7 => local amplitude ranges ~0.3x to 1.7x)
const ENVELOPE_VARIATION: f32 = 0.7;

/// hard cap on rendered thickness so a bad config can't turn the wave into a
/// wall of characters or waste time drawing off screen rows
const MAX_THICKNESS: u16 = 20;
/// smallest wavelength we'll accept => anything <= 0 would divide-by-zero
const MIN_WAVELENGTH: f32 = 1.0;
/// upper bound on the smoothing kernel radius (in columns)
const MAX_SMOOTHING: usize = 16;

const HIGH_INTENSITY_THRESHOLD: f32 = 0.9;
const MED_INTENSITY_THRESHOLD: f32 = 0.5;
//const LOW_INTENSITY_THRESHOLD: f32 = 0.3; don't need it tbh

/// soft saturating replacement for clamp -1 1
fn soft_limit(v: f32) -> f32 {
    v.tanh()
}

/// gaussian-weighted moving average; edges renormalize instead of padding
fn smooth(values: &[f32], radius: usize) -> Vec<f32> {
    if radius == 0 || values.len() < 2 {
        return values.to_vec();
    }
    let r = radius as i32;
    let sigma = (radius as f32 / 2.0).max(0.5);
    let weights: Vec<f32> = (-r..=r)
        .map(|k| (-(k as f32).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();

    let n = values.len() as i32;
    (0..n)
        .map(|i| {
            let (mut acc, mut wsum) = (0.0f32, 0.0f32);
            for (k, w) in (-r..=r).zip(&weights) {
                let j = i + k;
                if j < 0 || j >= n {
                    continue;
                }
                acc += values[j as usize] * w;
                wsum += w;
            }
            acc / wsum
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)] // missing fields in config.json fall back to default instead of failing the whole parse
struct Sinewave {
    wavelength: f32,
    amplitude: f32,
    speed: f32,
    thickness: u16,

    /// gaussian smoothing radius in columns; 0 = off
    smoothing: usize,

    theme: Theme,

    #[serde(skip)]
    time: f32,

    /// monotonic time axis for the noise, never wraps
    #[serde(skip)]
    noise_t: f64,
}

impl Default for Sinewave {
    fn default() -> Self {
        Self {
            wavelength: 40.0,
            amplitude: 10.0,
            speed: 0.01,
            time: 0.0,
            noise_t: 0.0,
            thickness: 0,
            smoothing: 4,
            theme: Theme {
                high_intensity: [0, 0, 0],
                med_intensity: [0, 0, 0],
                low_intensity: [0, 0, 0],
            },
        }
    }
}

impl Sinewave {
    fn advance_time(&mut self) {
        self.time = (self.speed + self.time).rem_euclid(1.);
        self.noise_t += NOISE_TIME_STEP;
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
        self.smoothing = self.smoothing.min(MAX_SMOOTHING);
    }

    /// wavelength/amplitude from the config are base values; perlin noise
    /// modulates them smoothly over time. computed once per frame.
    fn current_params(&self, p: &Perlin) -> (f32, f32) {
        let amp_n = soft_limit(p.get([self.noise_t, 200.5]) as f32 * NOISE_GAIN);
        let amplitude = self.amplitude * (1.0 + AMPLITUDE_VARIATION * amp_n);
        (self.wavelength, amplitude)
    }

    /// per-column amplitude: the (time-modulated) base amplitude scaled by a
    /// noise envelope that varies along x, so some stretches of the wave are
    /// taller than others and the pattern slowly evolves over time
    fn amplitude_at(&self, x: f32, amplitude: f32, p: &Perlin) -> f32 {
        let n = p.get([(x * ENVELOPE_SCALE) as f64, self.noise_t + 300.5]) as f32;
        let n = soft_limit(n * NOISE_GAIN);
        amplitude * (1.0 + ENVELOPE_VARIATION * n)
    }

    fn get_y_offset(&self, x: f32, wavelength: f32, amplitude: f32, p: &Perlin) -> f32 {
        let space_factor = x / wavelength;
        let angle = 2.0 * std::f32::consts::PI * (space_factor - self.time);
        let base_sine = amplitude * angle.sin();

        // pass `x` and `noise_t` to the noise so the distortions change across space and time
        let scale = 0.005; //adjust this to make the distortions smooth or jagged
        let noise_sample_point = [(x * scale) as f64, self.noise_t];

        let noise_distortion = p.get(noise_sample_point) as f32;

        base_sine + (noise_distortion * 15.0)
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
    perlin: &'a Perlin,
}

impl<'a> Widget for WaveWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let middle_y = area.y as i32 + (area.height / 2) as i32;
        let p = self.perlin;

        // noise-modulated wavelength/amplitude, computed once per frame
        let (wavelength, amplitude) = self.wave.current_params(p);

        // raw per-column offsets and local amplitudes
        let mut amps = Vec::with_capacity(area.width as usize);
        let mut offsets = Vec::with_capacity(area.width as usize);
        for x_cell in area.x..area.right() {
            let rel_x = (x_cell - area.x) as f32;
            // amplitude varies along x so some stretches are taller than others
            let local_amp = self.wave.amplitude_at(rel_x, amplitude, p);
            amps.push(local_amp);
            offsets.push(self.wave.get_y_offset(rel_x, wavelength, local_amp, p));
        }

        // smooth across x
        let offsets = smooth(&offsets, self.wave.smoothing);

        let mut prev_y: Option<i32> = None;
        let t = self.wave.thickness as i32;
        for (i, x_cell) in (area.x..area.right()).enumerate() {
            let y_offset = offsets[i];
            let local_amp = amps[i];
            let target_y = middle_y - y_offset.round() as i32;

            let signal_intensity = if local_amp.abs() < f32::EPSILON {
                0.0
            } else {
                (y_offset / local_amp.abs()).clamp(0.0, 1.0)
            };

            let (wave_color, symbol) = if signal_intensity > HIGH_INTENSITY_THRESHOLD {
                (self.wave.theme.high(), "#")
            } else if signal_intensity > MED_INTENSITY_THRESHOLD {
                (self.wave.theme.med(), "+")
            } else {
                (self.wave.theme.low(), ":")
            };

            let style = Style::default().fg(wave_color).add_modifier(Modifier::BOLD);

            // span covers the jump from the previous column, plus thickness
            let from = prev_y.unwrap_or(target_y);
            let lo = (from.min(target_y) - t).max(area.y as i32);
            let hi = (from.max(target_y) + t).min(area.bottom() as i32 - 1);
            prev_y = Some(target_y);

            for y in lo..=hi {
                if let Some(cell) = buf.cell_mut(Position::new(x_cell, y as u16)) {
                    cell.set_symbol(symbol);
                    cell.set_style(style);
                }
            }
        }
    }
}

pub struct App {
    sinewave: Sinewave,
    perlin: Perlin,
    exit: bool,
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
    pub fn new(args: Args) -> Self {
        Self {
            sinewave: Sinewave::default(),
            perlin: Perlin::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
            ),
            exit: false,
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
        }
        Ok(())
    }

    fn reload_config_if_changed(&mut self) {
        match self.config_watcher.poll() {
            ConfigPoll::Unchanged => {}
            ConfigPoll::Loaded(config) => {
                // carry over the runtime-only timeline values; everything else
                // comes from the file
                let saved_time = self.sinewave.time;
                let saved_noise_t = self.sinewave.noise_t;
                self.sinewave = config;
                self.sinewave.time = saved_time;
                self.sinewave.noise_t = saved_noise_t;
                self.config_error = None;
            }
            ConfigPoll::ParseError(err) => {
                // keep running with the last good settings, but surface
                // the problem instead of pretending nothing happened
                self.config_error = Some(err);
            }
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        let wave_widget = WaveWidget {
            wave: &self.sinewave,
            perlin: &self.perlin,
        };
        frame.render_widget(wave_widget, area);

        if self.debug {
            let info_string = match &self.config_error {
                Some(err) => format!(" config.json error: {err} | press 'q' to exit"),
                None => {
                    // show the live, noise-modulated values rather than the base config
                    let (wavelength, amplitude) = self.sinewave.current_params(&self.perlin);
                    format!(
                        " wavelength: {:.2} | amplitude: {:.1} | thickness: {} | smoothing: {} | press 'q' to exit",
                        wavelength, amplitude, self.sinewave.thickness, self.sinewave.smoothing
                    )
                }
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
