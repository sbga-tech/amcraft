//! The video-config manager (`am::video::Manager`, `spec/libam/platform.md` part C 2): the two monitor
//! descriptors that `SystemInfo` reports, filled from the `video` configuration, and the one call that applies
//! the display setting through the vendor DLL named by the registry (`amPlatformSetDisplaySettings`).
//!
//! The descriptors are configuration echoes: nothing here enumerates monitors.

use libam_api::am_config::VideoConfig;
use libam_api::system_info::VideoResolution;

use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use backend::{DisplayMode, DisplayRequest, KeyMode, RegValue, VideoError};

/// The configuration is unusable (a zero resolution).
pub const ERROR_BAD_RESOLUTION: u16 = 4104;
/// The display setting could not be applied.
pub const ERROR_DISPLAY_SETTING: u16 = 912;

/// The key naming the vendor DLL (`name`), opened and never created.
const DRIVER_KEY: &str = r"System\SEGA\SystemProperty\amVideo";
/// The key the applied setting is saved under (volatile).
const SETTING_KEY: &str = r"System\SEGA\SystemProperty\sgsetdisplaysetting\CurrentSetting";

/// `amy` statuses of the video layer.
mod status {
    pub const OK: u32 = 0;
    pub const FAILED: u32 = 0x8100_0002;
    pub const BAD_ARGUMENT: u32 = 0x8100_0003;
    pub const NO_REGISTRY: u32 = 0x8100_0015;
    pub const NO_LIBRARY: u32 = 0x8100_0016;
    pub const UNSUPPORTED: u32 = 0x8100_0017;
    pub const NO_OUTPUT_1: u32 = 0x9A00_102D;
    pub const NO_OUTPUT_2: u32 = 0x9A00_102E;
    pub const EXTRA_OUTPUT_1: u32 = 0x9A00_102F;
    pub const EXTRA_OUTPUT_2: u32 = 0x9A00_1030;
    pub const EXTRA_OUTPUT_BOTH: u32 = 0x9A00_1031;
}

/// One monitor descriptor (16 bytes in the original).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Descriptor {
    /// `+0`: the descriptor is in use.
    configured: bool,
    width: u32,
    height: u32,
    /// Rotation in degrees.
    rotation: u16,
}

/// `am::video::Manager` (`ctx+138216`).
#[derive(Debug, Default)]
pub struct Video {
    life: Lifecycle,
    config: VideoConfig,
    /// `+0x160` (primary) and `+0x170` (secondary).
    descriptors: [Descriptor; 2],
    /// `+0x180`: 0 not attempted, 2 applied, 3 failed. Not cleared by a reset, so the vendor call is made once.
    display_result: u32,
}

impl Video {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `setup` (`0x140252640`), from `OperatingState::enter`: takes the configuration; enabled -> state 1 with the
    /// descriptors marked (the secondary only in dual mode), disabled -> state 2.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.video;
        self.life.error.clear();
        if self.config.enable {
            self.descriptors[0].configured = true;
            self.descriptors[1].configured = self.config.mode == 2;
            self.life.state = StateWord::INITIALIZED;
        } else {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x140252760`, once per Operating frame (M.3): the display setting while enabled and not yet done.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if self.config.enable && self.life.state == StateWord::INITIALIZED {
            self.run(cx);
        }
    }

    /// `0x140252960`: `getDisplaySetting`, then `setDisplaySetting` unless the first latched an error.
    fn run(&mut self, cx: &mut Cx<'_>) {
        self.get_display_setting();
        if self.life.error.is_empty() {
            self.set_display_setting(cx);
        }
        self.life.state = if self.life.error.is_empty() { StateWord::READY } else { StateWord::FAILED };
    }

    /// `getDisplaySetting` (`0x140252A10`, 2.5): fills the descriptors from the configuration.
    fn get_display_setting(&mut self) {
        let primary = self.config.resolution;
        if primary.width == 0 || primary.height == 0 {
            tracing::error!("video: the configured resolution is invalid");
            self.life.error.set(ERROR_BAD_RESOLUTION, 0);
            return;
        }
        if self.descriptors[0].configured {
            self.descriptors[0] = Descriptor {
                configured: true,
                width: primary.width,
                height: primary.height,
                rotation: self.config.angle,
            };
        }
        if self.config.mode != 2 {
            return;
        }
        let secondary = self.config.secondary_resolution;
        let angle = self.config.secondary_angle;
        let none = secondary.width == 0 && secondary.height == 0;
        if none && angle == 0 {
            if self.descriptors[1].configured {
                self.descriptors[1] = Descriptor { configured: true, ..self.descriptors[0] };
            }
        } else if secondary.width != 0 && secondary.height != 0 {
            if self.descriptors[1].configured {
                self.descriptors[1] =
                    Descriptor { configured: true, width: secondary.width, height: secondary.height, rotation: angle };
            }
        } else {
            tracing::error!("video: the configured resolution is invalid");
            self.life.error.set(ERROR_BAD_RESOLUTION, 0);
        }
    }

    /// `setDisplaySetting` (`0x140252C30`, 2.6): the vendor call, at most once per process.
    fn set_display_setting(&mut self, cx: &mut Cx<'_>) {
        if self.display_result != 0 {
            if self.display_result == 3 {
                self.life.error.set(ERROR_DISPLAY_SETTING, 0);
            }
            return;
        }
        let request = self.request();
        let result = set_resolution(cx, &request);
        if is_error(result, self.config.ignore_unconnected) {
            tracing::error!(code = result, "video: applying the display settings failed");
            self.life.error.set(ERROR_DISPLAY_SETTING, 0);
            self.display_result = 3;
        } else {
            self.display_result = 2;
        }
    }

    /// The request of the current descriptors: `cfg.mode` 0 clone, 1 single, 2 dual (anything else clone).
    fn request(&self) -> DisplayRequest {
        let mode = match self.config.mode {
            1 => DisplayMode::Single,
            2 => DisplayMode::Dual,
            _ => DisplayMode::Clone,
        };
        let [first, second] = self.descriptors;
        let code = |descriptor: Descriptor| match descriptor.rotation {
            90 => 1,
            180 => 2,
            270 => 3,
            _ => 0,
        };
        DisplayRequest {
            mode,
            width: [first.width as u16, second.width as u16],
            height: [first.height as u16, second.height as u16],
            rotation: [code(first), code(second)],
        }
    }

    /// `0x1402526D0`, from the main-flow terminate: everything back to the constructed state.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if self.life.state != StateWord::UNINITIALIZED {
            *self = Self::new();
        }
    }

    /// `0x1402527D0`, from the module reset: while set up and enabled the descriptor sizes are cleared and the
    /// setup runs again. The result of the vendor call is kept.
    pub fn restart(&mut self) {
        if self.life.state.is_done() && self.config.enable {
            for descriptor in &mut self.descriptors {
                *descriptor = Descriptor { configured: descriptor.configured, ..Descriptor::default() };
            }
            self.life.begin();
        }
    }

    /// `0x140252860`: whether the manager asks for a platform reboot. The original never stores the request
    /// (2.9), so this is always false; the main flow still asks.
    pub fn wants_reboot(&self) -> bool {
        false
    }

    /// The monitor resolution of facade index `index` (`0x140218150`): index 0 is the secondary descriptor, any
    /// other index the primary one.
    pub fn resolution(&self, index: u32) -> VideoResolution {
        let descriptor = self.descriptors[usize::from(index == 0)];
        VideoResolution { width: descriptor.width, height: descriptor.height }
    }

    /// The configuration taken at initialize.
    pub fn config(&self) -> &VideoConfig {
        &self.config
    }

    /// `ctx+138264`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state in {2, 3}.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`ctx+138268`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

/// `amPlatformSetDisplaySettings` (`0x1402D0290`) and `amyVideoSetResolution`: the public return value, `0` or
/// `-1`, `-2`, `-3`, `-12..=-16`.
fn set_resolution(cx: &mut Cx<'_>, request: &DisplayRequest) -> i32 {
    let amy = apply(cx, request);
    match amy {
        status::OK => 0,
        status::FAILED => -3,
        status::BAD_ARGUMENT => -2,
        status::NO_OUTPUT_1 => -12,
        status::NO_OUTPUT_2 => -13,
        status::EXTRA_OUTPUT_1 => -14,
        status::EXTRA_OUTPUT_2 => -15,
        status::EXTRA_OUTPUT_BOTH => -16,
        _ => -1,
    }
}

/// `amyVideoSetResolution` core (`0x1402FA0F0`, 2.7): the DLL name from the registry, the DLL command, the
/// saved setting; returns the amy status.
fn apply(cx: &mut Cx<'_>, request: &DisplayRequest) -> u32 {
    let Ok(RegValue::Text(library)) = cx.backend.registry.read(DRIVER_KEY, KeyMode::Open, "name") else {
        return status::NO_REGISTRY;
    };
    let code = match cx.backend.video.apply(&library, request) {
        Ok(code) => code,
        Err(VideoError::NotFound) => return status::NO_LIBRARY,
        Err(VideoError::Failed) => return status::FAILED,
    };
    let outcome = match code {
        0 => status::OK,
        1 => status::BAD_ARGUMENT,
        4 => status::NO_LIBRARY,
        5 => status::UNSUPPORTED,
        8 => status::NO_OUTPUT_1,
        9 => status::NO_OUTPUT_2,
        10 => status::EXTRA_OUTPUT_1,
        11 => status::EXTRA_OUTPUT_2,
        12 => status::EXTRA_OUTPUT_BOTH,
        _ => return status::FAILED,
    };
    if (code == 0 || (8..=12).contains(&code)) && !save_setting(cx, request) {
        return status::FAILED;
    }
    outcome
}

/// The registry save of the applied setting (`0x1402FAA50`); an aborted step fails the save.
fn save_setting(cx: &mut Cx<'_>, request: &DisplayRequest) -> bool {
    let registry = &mut *cx.backend.registry;
    let mut put = |name: &str, value: RegValue| registry.write(SETTING_KEY, KeyMode::CreateVolatile, name, &value);
    // A write failure of this first value is only logged by the original.
    let _ = put("use_segatiming", RegValue::Dword(0));
    let rotation = |code: u32| match code {
        1 => 90,
        2 => 180,
        3 => 270,
        _ => 0,
    };
    let setting = |index: usize| {
        format!("{}x{}_{}", request.width[index], request.height[index], rotation(request.rotation[index]))
    };
    if put("monitor_setting_1", RegValue::Text(setting(0))).is_err() {
        return false;
    }
    if request.mode == DisplayMode::Dual && put("monitor_setting_2", RegValue::Text(setting(1))).is_err() {
        return false;
    }
    let ports: [u32; 8] = match request.mode {
        DisplayMode::Single => [1, 0, 0, 0, 0, 0, 0, 0],
        DisplayMode::Clone => [1; 8],
        DisplayMode::Dual => [1, 2, 1, 1, 1, 1, 1, 1],
    };
    ports.iter().enumerate().all(|(index, &port)| put(&format!("port_{}", index + 1), RegValue::Dword(port)).is_ok())
}

/// `isError` (`0x140252E30`): -14..=-16 are logged and accepted, -12 and -13 only with `ignore_unconnected`.
fn is_error(result: i32, ignore_unconnected: bool) -> bool {
    match result {
        0 => false,
        -16..=-14 => {
            let outputs = match result {
                -14 => "1",
                -15 => "2",
                _ => "1, 2",
            };
            tracing::warn!(outputs, "video: an unexpected connector is in use");
            false
        }
        -12 | -13 => !ignore_unconnected,
        _ => true,
    }
}
