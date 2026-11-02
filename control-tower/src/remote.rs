//! The remote switcher. Tasks 6 to 8 fill this module.

use crate::api::AgentStateSource;
use crate::cli::Options;
use crate::navigation::TmuxNavigator;
use crate::runtime::AppError;

/// Run the remote switcher. It is a stub until Task 8.
pub fn run(
    _options: Options,
    _source: AgentStateSource,
    _navigator: TmuxNavigator,
) -> Result<(), AppError> {
    Ok(())
}
