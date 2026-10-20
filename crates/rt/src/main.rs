mod view;

fn main() -> Result<(), rt::pty::Error> {
    // WebKitGTK's DMA-BUF renderer dies with "Error 71 (Protocol error)" on
    // Wayland with Nvidia drivers; fall back to the shared-memory renderer.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    view::run(&shell)
}
