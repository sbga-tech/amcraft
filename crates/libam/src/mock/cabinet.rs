//! The seed of the simulated healthy cabinet: registry values, computer name, mount directories and the English
//! resource set.

use std::path::Path;

use crate::registry_keys::{MOUNT_KEY, OS_VERSION_KEY, STATIC_KEY};
use backend::RegValue;
use backend::mock::{SimRegistry, SimSystem};

/// Resource folder of the simulated healthy cabinet, which a daemon configuration names as
/// `common.resource.resource_path`: an English error set with the system errors libam itself raises and an empty
/// warning set. A test that needs more errors rewrites `error.en.json`.
pub const SIMULATED_RESOURCE_PATH: &str = r"R:\res";

/// `error.en.json` of the simulated cabinet: the system errors of the compiled-in fallback table (`error.md` 7.1).
const SIMULATED_ERRORS: &str = r#"[
  {"number": 23, "sub": 0, "reset": "shutdown", "message": "Main board Malfunctioning", "tag": ""},
  {"number": 84, "sub": 0, "reset": "shutdown", "message": "Storage Device Malfunctioning", "tag": ""},
  {"number": 87, "sub": 0, "reset": "shutdown", "message": "Storage Device Malfunctioning", "tag": ""},
  {"number": 949, "sub": 0, "reset": "shutdown", "message": "Keychip Not Found", "tag": ""},
  {"number": 4103, "sub": 0, "reset": "shutdown", "message": "Unexpected Error Occurred", "tag": ""},
  {"number": 4104, "sub": 0, "reset": "shutdown", "message": "Unexpected Error Occurred", "tag": ""},
  {"number": 4105, "sub": 0, "reset": "shutdown", "message": "Unexpected Error Occurred", "tag": ""}
]"#;

/// The registry values, computer name, mount directories and English resource set of a working cabinet
/// (`core.md` B.4.4 and C.3, `platform.md` A.6 and D.3, `storage.md` 3.9, `error.md` 13).
pub(super) fn seed_healthy_cabinet(root: &Path, registry: &SimRegistry, system: &SimSystem) {
    registry.set(STATIC_KEY, "PlatformId", RegValue::Text("ABC1".into()));
    registry.set(STATIC_KEY, "CpuTempError", RegValue::Dword(90));
    registry.set(OS_VERSION_KEY, "OSVersion", RegValue::Text("6_1_7601".into()));
    registry.set(MOUNT_KEY, "AMFS", RegValue::Text(r"C:\amfs".into()));
    registry.set(MOUNT_KEY, "APPDATA", RegValue::Text(r"C:\appdata\".into()));
    system.state.borrow_mut().computer_name = Some("ABCD01234567890".into());
    // amNetwork reads the keychip's network (`serverIpIpv4`) through the registry, not the keychip driver.
    registry.set(r"System\Sega\SystemProperty\keychip", "serverIpIpv4", RegValue::Binary(vec![192, 168, 0, 0]));
    // A failure leaves the directory or file missing, which libam reports like a cabinet without it.
    for dir in ["amfs", "appdata"] {
        let _ = std::fs::create_dir_all(root.join("c").join(dir));
    }
    let resources = root.join("r").join("res");
    if std::fs::create_dir_all(&resources).is_ok() {
        let _ = std::fs::write(resources.join("error.en.json"), SIMULATED_ERRORS);
        let _ = std::fs::write(resources.join("warning.en.json"), "[]");
    }
}
