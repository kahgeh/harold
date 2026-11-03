//! One live Enter on a Rex block. It is ignored by default. Run it with a throwaway Rex
//! session that you made: `REX_LIVE_SESSION=<label> REX_LIVE_BLOCK=<block id> cargo test
//! -p control-tower --test rex_live -- --ignored --nocapture`.

use control_tower::navigation::{PaneNavigator, TmuxNavigator};

#[test]
#[ignore = "needs a Rex app and a throwaway Rex block"]
fn enter_on_a_live_rex_block_shows_it() {
    let session = std::env::var("REX_LIVE_SESSION").expect("REX_LIVE_SESSION");
    let block = std::env::var("REX_LIVE_BLOCK").expect("REX_LIVE_BLOCK");
    let navigator = TmuxNavigator::new();

    // Look at the Rex app: it shows the block. Harold, not the control-tower, reads the focus.
    navigator.focus_rex_block(&session, &block).unwrap();
}
