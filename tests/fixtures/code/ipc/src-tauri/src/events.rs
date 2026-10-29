use tauri::{AppHandle, Emitter};

pub fn announce_form(app: &AppHandle) {
    let _ = app.emit("form_opened", ());
}
