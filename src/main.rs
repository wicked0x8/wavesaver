use wavesaver::{App, Args, Parser};

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let mut app = App::new(args);

    ratatui::run(|term| app.run(term))
}
