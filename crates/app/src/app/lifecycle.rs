//! UAC relaunch, about box, quitting.

use super::*;

impl App {
    pub(super) fn elevate(&self) {
        if self.state.borrow().delete_job.is_some() {
            self.set_status("Bitte warten, bis das Verschieben in den Papierkorb abgeschlossen ist.");
            return;
        }
        if self.state.borrow().dup_job.is_some() {
            self.set_status("Bitte zuerst die Duplikat-Suche abbrechen oder abwarten.");
            return;
        }
        let path = self.ui().map(|ui| ui.get_scan_path().trim().to_string()).unwrap_or_default();
        let mut args: Vec<OsString> = Vec::new();
        if !path.is_empty() {
            args.push("--scan".into());
            args.push(path.into());
        }
        match elevation::relaunch_elevated(&args) {
            Ok(()) => self.quit(),
            Err(RelaunchError::Cancelled) => self.set_status("Administratorrechte wurden nicht erteilt."),
            Err(RelaunchError::Failed(code)) => {
                self.set_status(format!("Neustart als Administrator fehlgeschlagen (Fehler {code})."));
            }
        }
    }

    pub(super) fn show_about(&self) {
        shell::message_box(
            "Über SpaceLens",
            &format!(
                "SpaceLens {}\n\nSchneller Disk-Analyzer für Windows.\nScannen ist rein lesend. Gelöscht wird nur auf Wunsch, nach Rückfrage und ausschließlich in den Papierkorb.\n\nRechte: {}",
                env!("CARGO_PKG_VERSION"),
                if self.elevated { "Administrator" } else { "Standardbenutzer" }
            ),
            false,
        );
    }

    pub(super) fn quit(&self) {
        self.save_settings();
        self.shutdown();
        let _ = slint::quit_event_loop();
    }

    /// Stops running background work so worker threads end promptly.
    pub fn shutdown(&self) {
        let st = self.state.borrow();
        if let Some(job) = &st.job {
            job.progress.cancel();
        }
        if let Some(job) = &st.dup_job {
            job.progress.cancel();
        }
    }
}
