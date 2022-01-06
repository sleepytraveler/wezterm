use crate::input::*;
use crate::TerminalState;
use termwiz::input::{KeyCodeEncodeModes, KeyboardEncoding};

impl TerminalState {
    fn effective_keyboard_encoding(&self) -> KeyboardEncoding {
        match self.keyboard_encoding {
            KeyboardEncoding::Xterm if self.config.enable_csi_u_key_encoding() => {
                KeyboardEncoding::CsiU
            }
            enc => enc,
        }
    }

    /// Processes a key_down event generated by the gui/render layer
    /// that is embedding the Terminal.  This method translates the
    /// keycode into a sequence of bytes to send to the slave end
    /// of the pty via the `Write`-able object provided by the caller.
    pub fn key_down(&mut self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let to_send = key.encode(
            mods,
            KeyCodeEncodeModes {
                encoding: self.effective_keyboard_encoding(),
                newline_mode: self.newline_mode,
                application_cursor_keys: self.application_cursor_keys,
            },
        )?;

        if self.config.debug_key_events() {
            log::info!("key_down: sending {:?}, {:?} {:?}", to_send, key, mods);
        } else {
            log::trace!("key_down: sending {:?}, {:?} {:?}", to_send, key, mods);
        }
        self.writer.write_all(to_send.as_bytes())?;
        self.writer.flush()?;

        Ok(())
    }
}
