use anyhow::Result;

fn main() -> Result<()> {
    // CEF subprocess entry. On macOS this is bundled as a Helper.app.
    // On Linux/Windows it can be the same binary; macOS forces a separate exe.
    // TODO: cef_execute_process here for renderer/GPU/utility subprocesses.
    Ok(())
}
