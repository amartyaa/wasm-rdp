/// Input handling module.
/// Keyboard scancode mapping is in JavaScript (app.js) since that's where
/// KeyboardEvents are captured. The WASM module receives pre-mapped scancodes
/// via session.send_keyboard().
///
/// Mouse events are similarly forwarded from app.js with coordinates already
/// mapped to canvas space.
pub(crate) struct _Placeholder;
