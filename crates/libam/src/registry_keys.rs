//! The SEGA registry keys libam reads and writes through the [`Registry`](backend::Registry) backend
//! interface.

/// Registry key of the launcher channel (`core.md` B.4.1).
pub const MASTER_KEY: &str = r"System\SEGA\SystemProperty\Master";
/// Registry key holding the AMFS mount path (`core.md` B.4.4).
pub const MOUNT_KEY: &str = r"System\SEGA\SystemProperty\mount";
/// Registry key holding the platform id (`core.md` B.4.4).
pub const STATIC_KEY: &str = r"System\SEGA\SystemProperty\Static";
/// Registry key holding the OS version (`core.md` B.4.4).
pub const OS_VERSION_KEY: &str = r"SYSTEM\SEGA\SYSTEMPROPERTY";
