use clap::Parser;

fn main() {
    let cli = magicfs::cli::Cli::parse();
    if let Err(err) = magicfs::app::run(cli) {
        eprintln!("magicfs: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}
