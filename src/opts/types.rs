use clap::Clap;

#[derive(Clap)]
#[clap(version = "0.2.0", author = "James Houghton <jhoughton@virginia.edu>")]
pub struct Opts {
    #[clap(short, long, default_value = ".")]
    pub directory: String,
    #[clap(short, long, default_value = "80")]
    pub port: u16,
    #[clap(short = 'm', long, default_value = "0.0.0.0")]
    pub hostmask: String,
    #[clap(short, long = "upload", about = "Enable uploading capabilities")]
    pub uploading_enabled: bool,
    #[clap(long = "nodirs", about = "Disable directory listings")]
    pub disable_directory_listings: bool,
    #[clap(
        long = "start-disabled",
        about = "Start the server as disabled. Files will not be served until the server is \
                 enabled."
    )]
    pub start_disabled: bool,
    #[clap(
        short = 'r',
        long = "ui-refresh-rate",
        default_value = "100",
        about = "In milliseconds, how often the UI will be updated"
    )]
    pub ui_refresh_rate: u64,
    #[clap(long, about = "Do not start the interface (useful for testing)")]
    pub headless: bool,
}
