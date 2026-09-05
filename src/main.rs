//! The binary. Everything it does lives in the library beside it; this
//! file picks the allocator and the runtime flavour and gets out of the
//! way.

#[cfg(target_os = "linux")]
use mimalloc::MiMalloc;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    computer_use_hyprland::run_cli_from_env().await
}
