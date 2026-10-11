//! Port handles of the jvs module (`jvs.md` 4): the facade over an empty node list.

use libam_api::input::{InputPort, JvsNodePort};
use libam_api::jvs::{JvsNodeInput, JvsPort};
use libam_api::output::OutputPort;
use libam_api::result::{AmError, AmResult};

use crate::shared::Shared;

/// `JvsPort` handle over the shared libam state.
#[derive(Debug)]
pub struct JvsHandle(pub(crate) Shared);

/// `JvsNodePort` handle over the shared libam state; also serves `InputPort` and `OutputPort`.
#[derive(Debug)]
pub struct JvsNodeHandle(pub(crate) Shared);

/// Number of nodes in the node list (`0x140216A50`): always 0 [jvs.facade.node_count].
const NODE_COUNT: u64 = 0;

/// The failure of every indexed facade call: `index >= nodeCount` is always true, so the result is code 1 with the
/// message `invalid num` (`jvs.md` 4.3 to 4.5). None of these calls tests the libam context pointer; only `handle`
/// does [jvs.facade.handle].
fn invalid_num() -> AmError {
    AmError::with_message(AmError::INVALID_ARGUMENT, "invalid num")
}

impl JvsPort for JvsHandle {
    /// `0x1402187B0`: ok while libam is initialised, else result code 2 [jvs.facade.handle].
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn node_count(&self) -> u64 {
        NODE_COUNT
    }

    /// `0x140216A70` [jvs.facade.auto_output_zero].
    fn node_auto_output_zero(&self, _node: usize) -> AmResult<bool> {
        Err(invalid_num())
    }

    /// `0x140216AF0`: the slot lookup fails, so `GeneralOutput::set` is never reached
    /// [jvs.facade.output_slot].
    fn set_general_output(&mut self, _node: usize, _bits: u64) -> AmResult<()> {
        Err(invalid_num())
    }

    /// `0x140216B70` [jvs.facade.node_input].
    fn node_input(&self, _node: usize) -> AmResult<JvsNodeInput> {
        Err(invalid_num())
    }
}

impl JvsNodePort for JvsNodeHandle {
    /// `0x1402187B0`, see [`JvsHandle::handle`].
    fn jvs_handle(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn jvs_node_count(&mut self) -> u64 {
        NODE_COUNT
    }
}

impl InputPort for JvsNodeHandle {}

impl OutputPort for JvsNodeHandle {}
