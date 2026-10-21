mod native;
mod view;

use clap::Parser;
use raterm::config::Config;

/// Terminal with rich output: Markdown, HTML, SVG and images.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Window title.
    #[arg(short = 'T', long, default_value = "raterm")]
    title: String,
    /// Command and arguments to run instead of the shell; takes all the
    /// remaining arguments, like in alacritty or xterm.
    #[arg(
        short = 'e',
        long = "command",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
    /// Draw everything in the web view, as before the native view (M6).
    #[arg(long)]
    web: bool,
}

fn main() -> Result<(), raterm::pty::Error> {
    let args = Args::parse();
    // WebKitGTK's DMA-BUF renderer dies with "Error 71 (Protocol error)" on
    // Wayland with Nvidia drivers; fall back to the shared-memory renderer.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    let config = Config::load();
    let command = if args.command.is_empty() {
        vec![config.shell()]
    } else {
        args.command
    };
    if args.web {
        view::run(config, &command, args.title)
    } else {
        native::run(config, &command, args.title)
    }
}
