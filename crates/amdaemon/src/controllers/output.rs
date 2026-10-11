//! `amdaemon::server::OutputController` (vtable `0x1405D4848`).
//!
//! Identical to [`InputController`](crate::controllers::input::InputController) with the output relation type: it keeps
//! the process config's output section (five `0x8292`-byte blocks, `0x28CDA` bytes) and publishes
//! `makeServerData(jvsNodeCount)` into `ServerData+0x10E7E`. The shared relation-set code lives in
//! [`crate::controllers::input`]. Evidence: `spec/controllers/input_output.md` (`input_output.*` claims).

use std::fmt;

use crate::schema::input::ID_CAPACITY;
use crate::schema::output::{
    OutputCanInfoPatch, OutputDevicePatch, OutputInfoPatch, OutputJvsInfoPatch, OutputRelationMut, OutputRelationPatch,
    OutputRelationRef, OutputRelationSetMut, OutputRelationSetRef, OutputUsbInfoPatch, ServerOutput, ServerOutputMut,
    ServerOutputRef,
};
use crate::schema::text::fit_wide_str;
use libam::jvs::{self, OutputPort};

use crate::controllers::controller::{Controller, ControllerId, Ctx};
use crate::controllers::input::{InputOutputConfig, config_block};
use libam::config::ProcessConfig;

config_block! {
    ServerOutput<'static> { ServerOutputRef, ServerOutputMut },
    set { OutputRelationSetRef, OutputRelationSetMut },
    relation { OutputRelationRef, OutputRelationMut, jvs::OutputRelation, write_relation },
}

/// The output-side relation blocks (`0x28CDA` bytes in the original), compiled from libam's
/// [`OutputConfig`](libam::jvs::OutputConfig).
pub type CompiledOutputConfig = InputOutputConfig<ServerOutput<'static>>;

/// Compiles one libam output relation into its (cleared) schema slot: the id cut to the field, the device
/// type selected by the info variant, the index, the count (`0` for the types that do not read one) and the
/// type's info bytes.
fn write_relation(dst: &mut OutputRelationMut<'_>, src: &jvs::OutputRelation) {
    let id = fit_wide_str(&src.id, ID_CAPACITY);
    let (index, count, info) = match src.device {
        jvs::OutputDevice::Led { index } => (index, 0, OutputInfoPatch::Led),
        jvs::OutputDevice::JvsGeneral { index, count, node } => {
            (index, count, OutputInfoPatch::JvsGeneral(OutputJvsInfoPatch { node: Some(node) }))
        }
        jvs::OutputDevice::CanRequest { index, count, port, target, command, data_size } => {
            let info = OutputCanInfoPatch {
                port: Some(port),
                target: Some(target),
                command: Some(command),
                data_size: Some(data_size),
            };
            (index, count, OutputInfoPatch::CanRequest(info))
        }
        jvs::OutputDevice::UsbGeneral { index, count, node } => {
            (index, count, OutputInfoPatch::UsbGeneral(OutputUsbInfoPatch { node: Some(node) }))
        }
        jvs::OutputDevice::UsbPwm { index, node } => {
            (index, 0, OutputInfoPatch::UsbPwm(OutputUsbInfoPatch { node: Some(node) }))
        }
    };
    let patch = OutputRelationPatch {
        id: Some(&id),
        device: Some(OutputDevicePatch { kind: None, index: Some(index), count: Some(count), info: Some(info) }),
    };
    dst.copy_from(&patch).expect("the id is cut to the field");
}

/// `amdaemon::server::OutputController` (`App+0x870`, registry index 9).
///
/// Only vf5 is overridden: on the not-ready → ready edge the block is rebuilt and published. The libam JVS
/// and USBIO `DetectNode` events rebuild it regardless of readiness; the process-config observer only
/// replaces the private config and publishes nothing.
pub struct OutputController {
    port: Box<dyn OutputPort>,
    config: CompiledOutputConfig,
}

impl fmt::Debug for OutputController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputController").field("config", &self.config).finish_non_exhaustive()
    }
}

impl OutputController {
    /// Constructs the controller with an all-empty config (ctor `0x140040640`).
    pub fn new(port: Box<dyn OutputPort>) -> Self {
        Self { port, config: CompiledOutputConfig::default() }
    }

    /// The refresh function `0x1400407A0`: publishes into `ServerData+0x10E7E`.
    fn refresh(&mut self, ctx: &mut Ctx<'_>) {
        self.config.refresh(&mut *self.port, &mut ctx.server.output_mut());
    }
}

impl Controller for OutputController {
    fn id(&self) -> ControllerId {
        ControllerId::Output
    }

    /// vf5 `0x140040790`: rebuild and publish the whole block on the ready edge.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }

    /// Config observer `0x140041CA0`: replaces the private config with `Config+0x26082` (`0x28CDA` bytes in the
    /// original), compiled into the `ServerData` block layout.
    fn on_process_config(&mut self, config: &ProcessConfig, _ctx: &mut Ctx<'_>) {
        self.config.compile(&config.output);
    }

    /// JVS `DetectNode` callback `0x140041C90`.
    fn on_jvs_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }

    /// USBIO `DetectNode` callback `0x140041C80`.
    fn on_usb_io_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use widestring::U16CStr;

    use crate::schema::input::{ID_CAPACITY, MAX_RELATIONS};
    use crate::schema::output::{OutputDevice, OutputDeviceType, OutputInfo, OutputJvsInfo, OutputRelation};
    use crate::schema::text::fit_wide_str;
    use libam::jvs::{OutputConfig, RelationSet};
    use libam::sequence::ProcessState;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;
    use crate::controllers::input::tests::{FakePort, SharedLibam, libam};

    impl OutputPort for FakePort {}

    /// Appends one JVS general output relation per name (bits `[0, 2)` of node 1) to `set`.
    fn push_names(set: &mut RelationSet<jvs::OutputRelation>, names: &[&str]) {
        for name in names {
            let relation = jvs::OutputRelation {
                id: (*name).to_owned(),
                device: jvs::OutputDevice::JvsGeneral { index: 0, count: 2, node: 1 },
            };
            set.push(relation).expect("at most 64 relations");
        }
    }

    /// The relation [`push_names`] compiles to for `id`.
    fn filled_relation(id: &U16CStr) -> OutputRelation<'_> {
        OutputRelation {
            id,
            device: OutputDevice {
                kind: OutputDeviceType::JvsGeneral,
                index: 0,
                count: 2,
                info: OutputInfo::JvsGeneral(OutputJvsInfo { node: 1 }),
            },
        }
    }

    fn names(set: &OutputRelationSetRef<'_>) -> Vec<String> {
        set.relations()
            .iter()
            .take(usize::from(set.count()).min(MAX_RELATIONS))
            .map(|relation| String::from_utf16_lossy(relation.id().as_slice()))
            .collect()
    }

    fn player<'w>(block: &ServerOutputRef<'w>, index: usize) -> OutputRelationSetRef<'w> {
        block.players().get(index).expect("eight player sets")
    }

    fn config_with_blocks(blocks: &[usize]) -> OutputConfig {
        let mut config = OutputConfig::default();
        for &k in blocks {
            let tag = if k == 0 { "C".to_owned() } else { k.to_string() };
            let block = &mut config.blocks[k];
            push_names(&mut block.system, &[format!("{tag}S").as_str()]);
            push_names(&mut block.players[3], &[format!("{tag}P").as_str()]);
        }
        config
    }

    struct Rig {
        harness: Harness,
        libam: SharedLibam,
        slot: ControllerSlot,
    }

    impl Rig {
        fn new(nodes: u64, config: &OutputConfig) -> Self {
            let libam = libam(nodes);
            let mut harness = Harness::new();
            let mut controller = OutputController::new(Box::new(FakePort(Rc::clone(&libam))));
            let process_config = ProcessConfig { output: config.clone(), ..ProcessConfig::default() };
            controller.on_process_config(&process_config, &mut harness.ctx());
            Self { harness, libam, slot: ControllerSlot::new(Box::new(controller)) }
        }

        fn state(&mut self, state: u32) {
            self.harness.manager.on_change_state(ProcessState(state));
        }

        fn tick(&mut self) {
            self.slot.pre_execute(&mut self.harness.ctx());
        }

        fn system_names(&self) -> Vec<String> {
            names(&self.harness.server.output().system())
        }
    }

    #[test]
    fn output_profile_selection_merges_the_highest_non_empty_profile_after_common() {
        let config = config_with_blocks(&[0, 1, 2, 4]);
        let mut rig = Rig::new(3, &config);
        rig.state(3);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "2S"]);
        let output = rig.harness.server.output();
        assert_eq!(names(&player(&output, 3)), ["CP", "2P"]);
        let id = fit_wide_str("2P", ID_CAPACITY);
        assert_eq!(player(&output, 3).relations().get(1).expect("slot below 64").copy_into(), filled_relation(&id));
    }

    #[test]
    fn detect_events_republish_from_the_live_node_count() {
        let config = config_with_blocks(&[0, 1, 2]);
        let mut rig = Rig::new(1, &config);
        rig.slot.controller_mut().on_jvs_detect_node(&mut rig.harness.ctx());
        assert_eq!(rig.system_names(), ["CS", "1S"]);
        rig.libam.borrow_mut().nodes = 2;
        rig.slot.controller_mut().on_usb_io_detect_node(&mut rig.harness.ctx());
        assert_eq!(rig.system_names(), ["CS", "2S"]);
        assert!(!rig.slot.is_ready());
    }
}
