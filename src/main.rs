use wavesaver::{App, Args, Parser, spawn_jiggle_thread};

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let jiggle_receiver = spawn_jiggle_thread();
    let args = Args::parse();
    let mut app = App::new(jiggle_receiver, args);

    ratatui::run(|term| app.run(term))
}
