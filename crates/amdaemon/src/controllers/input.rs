//! `amdaemon::server::InputController` (vtable `0x1405CF8B8`) and the relation-map code it shares with
//! [`OutputController`](crate::controllers::output::OutputController).
//!
//! Both controllers are pure map compilers: no hardware, no request protocol, no `ClientData`. They keep a
//! private copy of the `process::config::InputOutputConfig` (five server-data blocks: common plus one profile
//! per detected JVS node count 1..=4) and publish `makeServerData(jvsNodeCount)` into their `ServerData` block
//! on three triggers: the not-ready → ready edge (vf5), the libam JVS `DetectNode` event and the libam USBIO
//! `DetectNode` event. Evidence: `spec/controllers/input_output.md` (`input_output.*` claims).

use std::alloc::{Layout, handle_alloc_error};
use std::fmt;

use zerocopy::FromZeros;

use crate::schema::input::{
    CanInfoPatch, ID_CAPACITY, InputDevicePatch, InputInfoPatch, InputRelationMut, InputRelationPatch,
    InputRelationRef, InputRelationSetMut, InputRelationSetRef, JvsInfoPatch, SYSTEM_PLAYER, ServerInput,
    ServerInputMut, ServerInputRef, UsbInfoPatch,
};
use crate::schema::text::fit_wide_str;
use libam::jvs::{
    CONFIG_BLOCKS, InputDevice, InputPort, InputRelation, JvsNodePort, MAX_JVS_NODES, RelationBlock, RelationMap,
};

use crate::controllers::controller::{Controller, ControllerId, Ctx};
use libam::config::ProcessConfig;

/// The merge of two sets would hold more than 64 relations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeOverflow;

/// An `InputOutputServerData<Id, Device>` schema usable in an [`InputOutputConfig`]: the block's checked views
/// and its relation-set merge.
///
/// Implemented by the `config_block!` macro for the input and output schemas.
pub trait ConfigBlock {
    /// Aligned, zero-filled storage of one block outside a `ServerData` image.
    type Storage: FromZeros;
    /// Read view of the block.
    type Ref<'w>;
    /// Write view of the block.
    type Mut<'w>;
    /// Relation of the libam model this block compiles from.
    type Relation;

    /// Read view of `storage`.
    fn view(storage: &Self::Storage) -> Self::Ref<'_>;

    /// Write view of `storage`.
    fn view_mut(storage: &mut Self::Storage) -> Self::Mut<'_>;

    /// Compiles the libam model of one block into `dst`: clears it, then writes every relation and count.
    fn compile_block(dst: &mut Self::Mut<'_>, src: &RelationBlock<Self::Relation>);

    /// Whether no set holds a relation (`system.count == 0` and every `players[i].count == 0`).
    fn is_empty(block: &Self::Ref<'_>) -> bool;

    /// `*dst = *src`: the whole block, including the unused relation slots.
    fn copy_block(dst: &mut Self::Mut<'_>, src: &Self::Ref<'_>);

    /// `ServerData::merge(dst, src, dst)` (`0x140036D20` / `0x140041440`): per set, appends `src`'s relations
    /// after `dst`'s and zeroes the unused tail. A set whose combined count would exceed 64 stays unchanged and is
    /// reported to `on_overflow` (`0` = `system`, `1 + i` = `players[i]`); the remaining sets are still merged.
    fn merge_block(dst: &mut Self::Mut<'_>, src: &Self::Ref<'_>, on_overflow: impl FnMut(usize));
}

/// Implements [`ConfigBlock`] for one instantiation of the `InputOutputServerData` template: the block schema
/// with its read/write views, and the relation-set and relation views the block is made of.
macro_rules! config_block {
    (
        $block:ty { $block_ref:ident, $block_mut:ident },
        set { $set_ref:ident, $set_mut:ident },
        relation { $relation_ref:ident, $relation_mut:ident, $model:ty, $write:path } $(,)?
    ) => {
        const _: () = {
            use ::libam::jvs::{RelationBlock, RelationSet};
            use $crate::controllers::input::{ConfigBlock, MergeOverflow};
            use $crate::schema::input::{MAX_RELATIONS, PLAYER_SETS};

            const STAYS_VALID: &str = "config blocks change only through their checked capability";

            /// `RelationSet::merge` copies a relation over the destination slot: no byte of the old slot survives.
            fn overwrite_relation(dst: &mut $relation_mut<'_>, src: &$relation_ref<'_>) {
                dst.clear();
                dst.copy_from(&src.copy_into().into()).expect("a relation of the same schema fits its slot");
            }

            /// `RelationSet::merge(set, other, set)` (`0x140037E50` / `0x1400423D0`): `dst`'s relations followed by
            /// `src`'s, with the unused tail zeroed. `dst` is unchanged when the combined count exceeds 64. (The
            /// original adds the two `u16` counts; sums that wrap around `u16` read out of bounds there and are
            /// reported here.)
            fn merge_set(dst: &mut $set_mut<'_>, src: &$set_ref<'_>) -> Result<(), MergeOverflow> {
                let first = usize::from(dst.count());
                let total = first + usize::from(src.count());
                if total > MAX_RELATIONS {
                    return Err(MergeOverflow);
                }
                let mut relations = dst.relations_mut();
                for (slot, relation) in (first..total).zip(src.relations().iter()) {
                    overwrite_relation(&mut relations.get_mut(slot).expect("slot below 64"), &relation);
                }
                for slot in total..MAX_RELATIONS {
                    relations.get_mut(slot).expect("slot below 64").clear();
                }
                // `total <= 64` was checked above.
                dst.count_mut().set(total as u16).expect("u16 accepts every value");
                Ok(())
            }

            /// Compiles one libam relation set into the (cleared) `dst`: the relations in config order, then the
            /// count. A `RelationSet` holds at most 64 relations, which is the capacity.
            fn compile_set(dst: &mut $set_mut<'_>, src: &RelationSet<$model>) {
                let relations = src.as_slice();
                let mut slots = dst.relations_mut();
                for (slot, relation) in relations.iter().enumerate() {
                    $write(&mut slots.get_mut(slot).expect("slot below 64"), relation);
                }
                dst.count_mut().set(relations.len() as u16).expect("u16 accepts every value");
            }

            impl ConfigBlock for $block {
                type Storage = ::zero_schema::schema_buffer!($block);
                type Ref<'w> = $block_ref<'w>;
                type Mut<'w> = $block_mut<'w>;
                type Relation = $model;

                fn view(storage: &Self::Storage) -> $block_ref<'_> {
                    <$block>::access(storage.as_bytes()).expect(STAYS_VALID)
                }

                fn view_mut(storage: &mut Self::Storage) -> $block_mut<'_> {
                    <$block>::access_mut(storage.as_bytes_mut()).expect(STAYS_VALID)
                }

                fn compile_block(dst: &mut $block_mut<'_>, src: &RelationBlock<$model>) {
                    dst.clear();
                    compile_set(&mut dst.system_mut(), &src.system);
                    let mut players = dst.players_mut();
                    for (player, set) in src.players.iter().enumerate() {
                        let mut dst_set = players.get_mut(player).expect("eight player sets");
                        compile_set(&mut dst_set, set);
                    }
                }

                fn is_empty(block: &$block_ref<'_>) -> bool {
                    block.system().count() == 0 && block.players().iter().all(|set| set.count() == 0)
                }

                fn copy_block(dst: &mut $block_mut<'_>, src: &$block_ref<'_>) {
                    dst.clear();
                    dst.system_mut()
                        .copy_from(&src.system().copy_into().into())
                        .expect("a set of the same schema fits");
                    let (mut dst_players, src_players) = (dst.players_mut(), src.players());
                    for player in 0..PLAYER_SETS {
                        let mut dst_set = dst_players.get_mut(player).expect("eight player sets");
                        let src_set = src_players.get(player).expect("eight player sets");
                        dst_set.copy_from(&src_set.copy_into().into()).expect("a set of the same schema fits");
                    }
                }

                fn merge_block(dst: &mut $block_mut<'_>, src: &$block_ref<'_>, mut on_overflow: impl FnMut(usize)) {
                    if merge_set(&mut dst.system_mut(), &src.system()).is_err() {
                        on_overflow(0);
                    }
                    let (mut dst_players, src_players) = (dst.players_mut(), src.players());
                    for player in 0..PLAYER_SETS {
                        let mut dst_set = dst_players.get_mut(player).expect("eight player sets");
                        let src_set = src_players.get(player).expect("eight player sets");
                        if merge_set(&mut dst_set, &src_set).is_err() {
                            on_overflow(1 + player);
                        }
                    }
                }
            }
        };
    };
}
pub(crate) use config_block;

// A libam relation set must fit a schema set exactly, for the input and the output side alike.
const _: () = {
    assert!(libam::jvs::MAX_RELATIONS == crate::schema::input::MAX_RELATIONS);
    assert!(libam::jvs::PLAYER_SETS == crate::schema::input::PLAYER_SETS);
};

config_block! {
    ServerInput<'static> { ServerInputRef, ServerInputMut },
    set { InputRelationSetRef, InputRelationSetMut },
    relation { InputRelationRef, InputRelationMut, InputRelation, write_relation },
}

/// Info bytes of a JVS device; analog and rotary inputs have no player and keep the default
/// [`SYSTEM_PLAYER`].
fn jvs_info(node: u8, player: Option<u8>) -> JvsInfoPatch {
    JvsInfoPatch { node: Some(node), player: Some(player.unwrap_or(SYSTEM_PLAYER)) }
}

/// Info bytes of a CAN device.
fn can_info(port: u8, target: u8) -> CanInfoPatch {
    CanInfoPatch { port: Some(port), target: Some(target) }
}

/// Info bytes of a USB device; analog and rotary inputs have no player and keep the default
/// [`SYSTEM_PLAYER`].
fn usb_info(node: u8, player: Option<u8>) -> UsbInfoPatch {
    UsbInfoPatch { node: Some(node), player: Some(player.unwrap_or(SYSTEM_PLAYER)) }
}

/// Compiles one libam input relation into its (cleared) schema slot: the id cut to the field, the device
/// type selected by the info variant, the index and the type's info bytes.
fn write_relation(dst: &mut InputRelationMut<'_>, src: &InputRelation) {
    let id = fit_wide_str(&src.id, ID_CAPACITY);
    let (index, info) = match src.device {
        InputDevice::PushSwitch { index } => (index, InputInfoPatch::PushSwitch),
        InputDevice::JvsSwitch { index, node, player } => (index, InputInfoPatch::JvsSwitch(jvs_info(node, player))),
        InputDevice::JvsAnalog { index, node } => (index, InputInfoPatch::JvsAnalog(jvs_info(node, None))),
        InputDevice::JvsRotary { index, node } => (index, InputInfoPatch::JvsRotary(jvs_info(node, None))),
        InputDevice::CanSwitch { index, port, target } => (index, InputInfoPatch::CanSwitch(can_info(port, target))),
        InputDevice::CanAnalog { index, port, target } => (index, InputInfoPatch::CanAnalog(can_info(port, target))),
        InputDevice::CanRotary { index, port, target } => (index, InputInfoPatch::CanRotary(can_info(port, target))),
        InputDevice::UsbSwitch { index, node, player } => (index, InputInfoPatch::UsbSwitch(usb_info(node, player))),
        InputDevice::UsbAnalog { index, node } => (index, InputInfoPatch::UsbAnalog(usb_info(node, None))),
        InputDevice::UsbRotary { index, node } => (index, InputInfoPatch::UsbRotary(usb_info(node, None))),
    };
    let patch = InputRelationPatch {
        id: Some(&id),
        device: Some(InputDevicePatch { kind: None, index: Some(index), info: Some(info) }),
    };
    dst.copy_from(&patch).expect("the id is cut to the field");
}

/// `process::config::InputOutputConfig<ServerData>`: `ServerData block[5]`, contiguous, no header, here the
/// controller's private copy compiled from libam's [`RelationMap`].
///
/// Block 0 is the *common* block (non-JVS relations); block `k` (1..=4) holds the JVS relations that apply
/// when libam detects `k` JVS nodes. A default config is all-empty (`count == 0` everywhere).
pub struct InputOutputConfig<B: ConfigBlock> {
    blocks: Box<[B::Storage; CONFIG_BLOCKS]>,
}

/// The input-side relation blocks (`0x25FDA` bytes in the original), compiled from libam's
/// [`InputConfig`](libam::jvs::InputConfig).
pub type CompiledInputConfig = InputOutputConfig<ServerInput<'static>>;

impl<B: ConfigBlock> InputOutputConfig<B> {
    /// Block `index` (0 = common, `k` = profile for `k` JVS nodes).
    ///
    /// # Panics
    ///
    /// If `index >= CONFIG_BLOCKS`.
    pub fn block(&self, index: usize) -> B::Ref<'_> {
        B::view(&self.blocks[index])
    }

    /// Block `index` mutably.
    ///
    /// # Panics
    ///
    /// If `index >= CONFIG_BLOCKS`.
    pub fn block_mut(&mut self, index: usize) -> B::Mut<'_> {
        B::view_mut(&mut self.blocks[index])
    }

    /// Replaces every block with the compilation of `model` (the config observer's `memcpy` of
    /// `Config+0xA8` / `Config+0x26082`, here from libam's relation map): nothing of the previous
    /// content survives.
    pub fn compile(&mut self, model: &RelationMap<B::Relation>) {
        for (storage, block) in self.blocks.iter_mut().zip(&model.blocks) {
            B::compile_block(&mut B::view_mut(storage), block);
        }
    }

    /// `InputOutputConfig::makeServerData(jvsNodeCount, out)` (`0x140037C00` / `0x140042180`).
    ///
    /// `out` becomes the common block with the *highest non-empty profile `k <= jvs_node_count`*
    /// concatenated after it (`common ++ profile[k]`, per set, no dedup); exactly one profile is merged and
    /// none above the node count. A set that would exceed 64 relations is logged and keeps its
    /// common content while the other sets still merge.
    ///
    /// A node count above 4 is logged and leaves `out` untouched.
    pub fn make_server_data(&self, jvs_node_count: u64, out: &mut B::Mut<'_>) {
        let Some(nodes) = usize::try_from(jvs_node_count).ok().filter(|&nodes| nodes < CONFIG_BLOCKS) else {
            tracing::error!(count = jvs_node_count, "jvs node count is out of range");
            return;
        };
        let [common, profiles @ ..] = &*self.blocks;
        B::copy_block(out, &B::view(common));
        let profile = profiles[..nodes].iter().rev().map(|storage| B::view(storage)).find(|view| !B::is_empty(view));
        if let Some(profile) = profile {
            B::merge_block(out, &profile, |set| {
                tracing::error!(set, "merged relation count exceeds the limit");
            });
        }
    }

    /// The refresh function (`0x140036030` / `0x1400407A0`) run by vf5 and both `DetectNode` events: reads
    /// the live JVS node count from libam (0 when libam is absent) and, for at most 4 nodes, rebuilds the
    /// whole block into `out`. More nodes: nothing is written or logged.
    pub(crate) fn refresh(&self, port: &mut dyn JvsNodePort, out: &mut B::Mut<'_>) {
        let nodes = match port.jvs_handle() {
            Ok(()) => port.jvs_node_count(),
            Err(_) => 0,
        };
        if nodes <= MAX_JVS_NODES {
            self.make_server_data(nodes, out);
        }
    }
}

impl<B: ConfigBlock> Default for InputOutputConfig<B> {
    fn default() -> Self {
        let blocks = <[B::Storage; CONFIG_BLOCKS]>::new_box_zeroed()
            .unwrap_or_else(|_| handle_alloc_error(Layout::new::<[B::Storage; CONFIG_BLOCKS]>()));
        Self { blocks }
    }
}

impl<B: ConfigBlock> fmt::Debug for InputOutputConfig<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let empty = self.blocks.each_ref().map(|storage| B::is_empty(&B::view(storage)));
        f.debug_struct("InputOutputConfig").field("empty_blocks", &empty).finish_non_exhaustive()
    }
}

/// `amdaemon::server::InputController` (`App+0x840`, registry index 8).
///
/// Only vf5 is overridden: on the not-ready → ready edge the block is rebuilt and published. The libam JVS
/// and USBIO `DetectNode` events rebuild it regardless of readiness; the process-config observer only
/// replaces the private config and publishes nothing.
pub struct InputController {
    port: Box<dyn InputPort>,
    config: CompiledInputConfig,
}

impl fmt::Debug for InputController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputController").field("config", &self.config).finish_non_exhaustive()
    }
}

impl InputController {
    /// Constructs the controller with an all-empty config (ctor `0x140035ED0`).
    pub fn new(port: Box<dyn InputPort>) -> Self {
        Self { port, config: CompiledInputConfig::default() }
    }

    /// The refresh function `0x140036030`: publishes into `ServerData+0x94EC`.
    fn refresh(&mut self, ctx: &mut Ctx<'_>) {
        self.config.refresh(&mut *self.port, &mut ctx.server.input_mut());
    }
}

impl Controller for InputController {
    fn id(&self) -> ControllerId {
        ControllerId::Input
    }

    /// vf5 `0x140036020`: rebuild and publish the whole block on the ready edge.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }

    /// Config observer `0x140037570`: replaces the private config with `Config+0xA8` (`0x25FDA` bytes in the
    /// original), compiled into the `ServerData` block layout.
    fn on_process_config(&mut self, config: &ProcessConfig, _ctx: &mut Ctx<'_>) {
        self.config.compile(&config.input);
    }

    /// JVS `DetectNode` callback `0x140037560`.
    fn on_jvs_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }

    /// USBIO `DetectNode` callback `0x140037580`.
    fn on_usb_io_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        self.refresh(ctx);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::image::ServerImage;
    use crate::schema::input::{ID_CAPACITY, InputDevicePatch, InputInfoPatch, InputRelationPatch, MAX_RELATIONS};
    use crate::schema::text::fit_wide_str;
    use libam::jvs::{InputConfig, RelationSet};
    use libam::result::{AmError, AmResult};
    use libam::sequence::ProcessState;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// Shared state of the libam test double, so tests can steer it after handing it to a controller.
    #[derive(Debug)]
    pub(crate) struct Libam {
        pub(crate) handle: AmResult<()>,
        pub(crate) nodes: u64,
        pub(crate) calls: Vec<&'static str>,
    }

    impl Default for Libam {
        fn default() -> Self {
            Self { handle: Ok(()), nodes: 0, calls: Vec::new() }
        }
    }

    pub(crate) type SharedLibam = Rc<RefCell<Libam>>;

    /// Port double reading a [`SharedLibam`].
    #[derive(Debug)]
    pub(crate) struct FakePort(pub(crate) SharedLibam);

    impl JvsNodePort for FakePort {
        fn jvs_handle(&mut self) -> AmResult<()> {
            let mut libam = self.0.borrow_mut();
            libam.calls.push("handle");
            libam.handle.clone()
        }

        fn jvs_node_count(&mut self) -> u64 {
            let mut libam = self.0.borrow_mut();
            libam.calls.push("nodeCount");
            libam.nodes
        }
    }

    impl InputPort for FakePort {}

    pub(crate) fn libam(nodes: u64) -> SharedLibam {
        Rc::new(RefCell::new(Libam { nodes, ..Libam::default() }))
    }

    /// Fills `set` with one push-switch relation per name (device index 0) and sets its count.
    fn fill(set: &mut InputRelationSetMut<'_>, names: &[&str]) {
        for (slot, name) in names.iter().enumerate() {
            let id = fit_wide_str(name, ID_CAPACITY);
            let patch = InputRelationPatch {
                id: Some(&id),
                device: Some(InputDevicePatch { kind: None, index: Some(0), info: Some(InputInfoPatch::PushSwitch) }),
            };
            set.relations_mut().get_mut(slot).expect("slot below 64").copy_from(&patch).expect("id fits the field");
        }
        set.count_mut().set(u16::try_from(names.len()).unwrap()).expect("u16 accepts every value");
    }

    /// Appends one push-switch relation per name (device index 0) to `set`: what [`fill`] writes, as config.
    fn push_names(set: &mut RelationSet<InputRelation>, names: &[&str]) {
        for name in names {
            let relation = InputRelation { id: (*name).to_owned(), device: InputDevice::PushSwitch { index: 0 } };
            set.push(relation).expect("at most 64 relations");
        }
    }

    fn names(set: &InputRelationSetRef<'_>) -> Vec<String> {
        set.relations()
            .iter()
            .take(usize::from(set.count()).min(MAX_RELATIONS))
            .map(|relation| String::from_utf16_lossy(relation.id().as_slice()))
            .collect()
    }

    fn player<'w>(block: &ServerInputRef<'w>, index: usize) -> InputRelationSetRef<'w> {
        block.players().get(index).expect("eight player sets")
    }

    /// Config whose block `k` has `system = [<k>S]` and `players[0] = [<k>P]` (`C` for the common block).
    fn config_with_blocks(blocks: &[usize]) -> InputConfig {
        let mut config = InputConfig::default();
        for &k in blocks {
            let tag = if k == 0 { "C".to_owned() } else { k.to_string() };
            let block = &mut config.blocks[k];
            push_names(&mut block.system, &[format!("{tag}S").as_str()]);
            push_names(&mut block.players[0], &[format!("{tag}P").as_str()]);
        }
        config
    }

    fn published(config: &InputConfig, nodes: u64) -> ServerImage {
        let mut compiled = CompiledInputConfig::default();
        compiled.compile(config);
        let mut out = ServerImage::new();
        compiled.make_server_data(nodes, &mut out.input_mut());
        out
    }

    #[test]
    fn highest_non_empty_profile_not_above_the_node_count_is_merged_after_common() {
        // profile 3 is empty, 1/2/4 are not: nodes=3 falls back to profile 2.
        let config = config_with_blocks(&[0, 1, 2, 4]);
        let out = published(&config, 3);
        assert_eq!(names(&out.input().system()), ["CS", "2S"]);
        assert_eq!(names(&player(&out.input(), 0)), ["CP", "2P"]);

        assert_eq!(names(&published(&config, 4).input().system()), ["CS", "4S"]);
        assert_eq!(names(&published(&config, 2).input().system()), ["CS", "2S"]);
        assert_eq!(names(&published(&config, 1).input().system()), ["CS", "1S"]);
    }

    #[test]
    fn a_profile_that_only_fills_a_player_set_still_counts_as_non_empty() {
        let mut config = config_with_blocks(&[0, 1]);
        push_names(&mut config.blocks[2].players[7], &["LATE"]);
        let out = published(&config, 2);
        assert_eq!(names(&player(&out.input(), 7)), ["LATE"]);
        assert_eq!(names(&out.input().system()), ["CS"]);
    }

    #[test]
    fn merge_overflow_leaves_that_set_at_common_content() {
        let mut config = config_with_blocks(&[0, 1]);
        let sixty: Vec<String> = (0..60).map(|i| format!("c{i}")).collect();
        let sixty: Vec<&str> = sixty.iter().map(String::as_str).collect();
        push_names(&mut config.blocks[0].players[1], &sixty);
        push_names(&mut config.blocks[1].players[1], &["p0", "p1", "p2", "p3", "p4"]);

        let out = published(&config, 1);

        let overflowed = player(&out.input(), 1);
        assert_eq!(overflowed.count(), 60);
        assert_eq!(names(&overflowed), sixty);
        // The other sets still merged.
        assert_eq!(names(&out.input().system()), ["CS", "1S"]);
        assert_eq!(names(&player(&out.input(), 0)), ["CP", "1P"]);
    }

    /// `RelationSet::merge`: the second set follows the first without dedup and the unused tail is zeroed.
    #[test]
    fn set_merge_appends_the_second_set_after_the_first_without_dedup() {
        let mut source = CompiledInputConfig::default();
        fill(&mut source.block_mut(0).system_mut(), &["B", "C"]);
        let mut out = ServerImage::new();
        fill(&mut out.input_mut().system_mut(), &["A", "B"]);

        let mut overflow = Vec::new();
        ServerInput::merge_block(&mut out.input_mut(), &source.block(0), |set| overflow.push(set));

        assert!(overflow.is_empty());
        assert_eq!(names(&out.input().system()), ["A", "B", "B", "C"]);
        assert_eq!(out.input().system().count(), 4);
        let tail = 0x94EC + 2 + 4 * 0x36..0x94EC + 0xD82;
        assert!(out.bytes()[tail].iter().all(|&byte| byte == 0));
    }

    /// `RelationSet::merge`: exactly 64 relations fit; 65 are rejected and leave the first set unchanged.
    #[test]
    fn set_merge_fills_exactly_64_and_rejects_65_leaving_the_first_set_unchanged() {
        let full: Vec<String> = (0..32).map(|i| format!("X{i}")).collect();
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        let mut source = CompiledInputConfig::default();
        fill(&mut source.block_mut(0).system_mut(), &full);
        fill(&mut source.block_mut(1).system_mut(), &["Y"]);
        let mut out = ServerImage::new();
        fill(&mut out.input_mut().system_mut(), &full);

        let mut overflow = Vec::new();
        ServerInput::merge_block(&mut out.input_mut(), &source.block(0), |set| overflow.push(set));
        assert!(overflow.is_empty());
        assert_eq!(out.input().system().count(), 64);

        let before = out.bytes().to_vec();
        ServerInput::merge_block(&mut out.input_mut(), &source.block(1), |set| overflow.push(set));
        assert_eq!(overflow, [0]);
        assert_eq!(out.bytes(), before);
    }

    struct Rig {
        harness: Harness,
        libam: SharedLibam,
        slot: ControllerSlot,
    }

    impl Rig {
        fn new(nodes: u64, config: &InputConfig) -> Self {
            let libam = libam(nodes);
            let mut harness = Harness::new();
            let mut controller = InputController::new(Box::new(FakePort(Rc::clone(&libam))));
            let process_config = ProcessConfig { input: config.clone(), ..ProcessConfig::default() };
            controller.on_process_config(&process_config, &mut harness.ctx());
            Self { harness, libam, slot: ControllerSlot::new(Box::new(controller)) }
        }

        fn tick(&mut self) {
            self.slot.pre_execute(&mut self.harness.ctx());
        }

        fn state(&mut self, state: u32) {
            self.harness.manager.on_change_state(ProcessState(state));
        }

        fn system_names(&self) -> Vec<String> {
            names(&self.harness.server.input().system())
        }
    }

    #[test]
    fn block_is_published_once_on_the_ready_edge_and_not_on_later_ticks() {
        let config = config_with_blocks(&[0, 2]);
        let mut rig = Rig::new(2, &config);

        rig.tick();
        assert_eq!(rig.system_names(), Vec::<String>::new(), "not ready: nothing published");

        rig.state(3);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "2S"]);

        // Still ready: libam changes are not polled.
        rig.libam.borrow_mut().nodes = 0;
        rig.tick();
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "2S"]);
    }

    #[test]
    fn every_not_ready_to_ready_edge_republishes_and_the_falling_edge_changes_nothing() {
        let config = config_with_blocks(&[0, 1, 2]);
        let mut rig = Rig::new(1, &config);
        rig.state(3);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "1S"]);

        rig.state(0);
        rig.libam.borrow_mut().nodes = 2;
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "1S"], "falling edge keeps the published block");

        rig.state(1);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "1S"], "setup is not ready");

        rig.state(4);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "2S"]);
    }

    #[test]
    fn jvs_and_usbio_detect_events_rebuild_the_block_even_when_not_ready() {
        let config = config_with_blocks(&[0, 1, 2]);
        let mut rig = Rig::new(1, &config);
        let mut ctx = rig.harness.ctx();
        rig.slot.controller_mut().on_jvs_detect_node(&mut ctx);
        assert_eq!(names(&ctx.server.input().system()), ["CS", "1S"]);

        rig.libam.borrow_mut().nodes = 2;
        rig.slot.controller_mut().on_usb_io_detect_node(&mut ctx);
        assert_eq!(names(&ctx.server.input().system()), ["CS", "2S"]);
        assert!(!rig.slot.is_ready());
    }

    #[test]
    fn absent_libam_publishes_the_common_block_without_asking_for_a_node_count() {
        let config = config_with_blocks(&[0, 1]);
        let mut rig = Rig::new(1, &config);
        rig.libam.borrow_mut().handle = Err(AmError::code(AmError::NOT_INITIALIZED));
        rig.slot.controller_mut().on_jvs_detect_node(&mut rig.harness.ctx());
        assert_eq!(rig.system_names(), ["CS"]);
        assert_eq!(rig.libam.borrow().calls, ["handle"]);
    }

    #[test]
    fn more_than_four_nodes_leave_the_block_untouched() {
        let config = config_with_blocks(&[0, 4]);
        let mut rig = Rig::new(4, &config);
        rig.state(3);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "4S"]);

        rig.libam.borrow_mut().nodes = 5;
        rig.slot.controller_mut().on_jvs_detect_node(&mut rig.harness.ctx());
        assert_eq!(rig.system_names(), ["CS", "4S"]);
    }

    #[test]
    fn process_config_observer_replaces_the_config_but_publishes_nothing_until_a_trigger() {
        let first = config_with_blocks(&[0, 1]);
        let mut rig = Rig::new(1, &first);
        rig.state(3);
        rig.tick();
        assert_eq!(rig.system_names(), ["CS", "1S"]);

        let mut second = InputConfig::default();
        push_names(&mut second.blocks[0].system, &["NEW"]);
        let process_config = ProcessConfig { input: second, ..ProcessConfig::default() };
        let mut ctx = rig.harness.ctx();
        rig.slot.controller_mut().on_process_config(&process_config, &mut ctx);
        assert_eq!(names(&ctx.server.input().system()), ["CS", "1S"], "observer does not publish");

        rig.slot.controller_mut().on_jvs_detect_node(&mut ctx);
        assert_eq!(names(&ctx.server.input().system()), ["NEW"], "config replaced verbatim, no residue of the old one");
    }
}
