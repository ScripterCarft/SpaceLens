//! Drive list.

use super::*;

impl App {
    pub(super) fn refresh_drives(&self) {
        let vols = volumes::list();
        let labels: Vec<SharedString> = vols.iter().map(drive_label).collect();
        let Some(ui) = self.ui() else { return };
        ui.set_drives(ModelRc::new(VecModel::from(labels)));

        if ui.get_scan_path().is_empty() {
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let index = vols.iter().position(|v| v.root.starts_with(&system)).or((!vols.is_empty()).then_some(0));
            if let Some(i) = index {
                ui.set_drive_index(i as i32);
                ui.set_scan_path(vols[i].root.as_str().into());
            }
        } else {
            // Pre-filled path (last scan): show its drive in the combo box.
            let path = ui.get_scan_path().to_uppercase();
            if let Some(i) = vols.iter().position(|v| path.starts_with(&v.root.to_uppercase()[..2])) {
                ui.set_drive_index(i as i32);
            }
        }
        self.state.borrow_mut().volumes = vols;
    }

    pub(super) fn drive_selected(&self, index: i32) {
        let root = self.state.borrow().volumes.get(index as usize).map(|v| v.root.clone());
        if let (Some(root), Some(ui)) = (root, self.ui()) {
            ui.set_scan_path(root.into());
        }
    }
}

fn drive_label(v: &Volume) -> SharedString {
    let name = if v.label.is_empty() { v.kind.label() } else { v.label.as_str() };
    format!(
        "{}  {}  ({})  –  {} von {} belegt",
        v.root.trim_end_matches('\\'),
        name,
        v.fs,
        format::bytes(v.used()),
        format::bytes(v.total)
    )
    .into()
}
