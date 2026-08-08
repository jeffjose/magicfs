fn main() {
    // Not `Cli::parse()`: the command line has to be split before clap sees it,
    // or a trailing `mpv --loop *` loses its flags to our own parser.
    let (cli, rest) = magicfs::cli::parse();
    if let Err(err) = magicfs::app::run(cli, &rest) {
        eprintln!("magicfs: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}
