// Linux: screen awareness is not built yet. Say so, plainly, instead of pretending: the
// model is told the screen can't be captured, and the rest of the app is untouched.

use crate::screen::{RawFrame, ScreenError};

pub fn grab() -> Result<RawFrame, ScreenError> {
    Err(ScreenError::Unavailable)
}
