//! Desktop build of the Android app.
//!
//! Same crate, same UI, no device. Keeping this compiling is what stops the
//! Android target quietly drifting away from the desktop dev loop. It runs
//! without an agent; `apps/desktop` is the one wired to a model.

use freya::prelude::{LaunchConfig, WindowConfig, launch};

fn main() {
    launch(
        LaunchConfig::new().with_window(
            WindowConfig::new_app(mc_ui::MobileCoder {
                agent: None,
                native_composer: false,
                files: None,
                shell: None,
            })
                .with_size(420., 860.)
                .with_title("mobile-coder"),
        ),
    )
}
