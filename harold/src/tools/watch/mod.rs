//! The watch tool: an agent asks Harold to run one of its skills with a model on a
//! schedule, and Harold types `WATCH` lines to the chief or to the agent. Outside this
//! module only `NAME` and `start` are used.

mod validate;

use crate::tools::{ToolContext, ToolError, ToolHandle};

pub(crate) const NAME: &str = "watch";

pub(crate) async fn start(_ctx: ToolContext) -> Result<ToolHandle, ToolError> {
    Err(ToolError {
        reason: "not_built",
    })
}
