mod bundle;
mod config;
mod monitor;
mod resources;
mod system;

pub use crate::{
    bundle::WindowBundle,
    config::DisplayConfig,
    monitor::{MonitorIdent, MonitorsAccess},
    resources::ScreenDimensions,
    system::{EventsLoopSystem, WindowSystem},
};
pub use winit::{Icon, Window};
