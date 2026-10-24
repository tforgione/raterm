mod native;

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
}

fn main() -> Result<(), raterm::pty::Error> {
    let args = Args::parse();
    let config = Config::load();
    // WebKitGTK's renderer for blocks (see Spec-View): the GPU buffers of its
    // DMA-BUF renderer make GTK die with "Error 71 (Protocol error)" on
    // Wayland with Nvidia drivers, and since 2.54 its fallback renderer
    // paints transparent blocks over themselves. The DMA-BUF renderer with
    // shared-memory buffers does neither, but costs about 200 MiB with
    // blocks in view: only where something shows through blocks, unless
    // the configuration or the environment says otherwise.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
        && std::env::var_os("WEBKIT_DMABUF_RENDERER_FORCE_SHM").is_none()
    {
        let var = if config.fallback_renderer() {
            "WEBKIT_DISABLE_DMABUF_RENDERER"
        } else {
            "WEBKIT_DMABUF_RENDERER_FORCE_SHM"
        };
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var(var, "1") };
    }
    let command = if args.command.is_empty() {
        vec![config.shell()]
    } else {
        args.command
    };
    native::run(config, &command, args.title)
}
