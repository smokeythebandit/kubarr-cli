use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

static INSTALL_LOG: OnceLock<Mutex<Option<Sender<String>>>> = OnceLock::new();

pub fn set_install_log(sender: Sender<String>) {
    *slot().lock().expect("install log mutex poisoned") = Some(sender);
}

pub fn clear_install_log() {
    *slot().lock().expect("install log mutex poisoned") = None;
}

pub fn emit(message: impl Into<String>) -> bool {
    let Some(sender) = slot()
        .lock()
        .expect("install log mutex poisoned")
        .as_ref()
        .cloned()
    else {
        return false;
    };
    sender.send(message.into()).is_ok()
}

fn slot() -> &'static Mutex<Option<Sender<String>>> {
    INSTALL_LOG.get_or_init(|| Mutex::new(None))
}
