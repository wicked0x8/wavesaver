use keepawake::Builder;
use wavesaver::{App, Args, Parser};

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let mut app = App::new(args);

    let _awake = Builder::default()
        .idle(true)
        .sleep(true)
        .display(true)
        .create()?;

    ratatui::run(|term| app.run(term))
}
