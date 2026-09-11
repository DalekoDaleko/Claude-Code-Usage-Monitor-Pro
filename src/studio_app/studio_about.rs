//! The About page: the application's name, version and copyright, and where
//! each enabled provider's sign-in comes from. Sign-in details describe a
//! credential and its expiry, and never show the credential itself.

use super::*;
use crate::poller::{self, SignInReport};
use crate::providers::{ProviderId, ProviderSet};
use crate::ui::theme::{danger, success};

/// The copyright notices from LICENSE, as holder and role.
const COPYRIGHTS: [(&str, &str); 2] = [
    (
        "Copyright (c) 2026 Vitaliy Titov",
        "Updates to UI and self-update logic",
    ),
    ("Copyright (c) 2025 Craig Constable", "Original author"),
];

type SignIns = Vec<(ProviderId, Option<SignInReport>)>;

/// Sign-in details, read on a worker thread: reading them can decrypt stored
/// tokens and, for a Claude login inside WSL, start `wsl.exe`. They are read
/// each time the About page is opened, so a sign-in that changed while the
/// dashboard was open shows up on the next visit.
#[derive(Default)]
pub(super) struct AboutState {
    sign_ins: Option<SignIns>,
    pending: Option<mpsc::Receiver<SignIns>>,
    /// The providers the list was read for; `None` asks for a fresh read.
    read_for: Option<ProviderSet>,
}

impl AboutState {
    /// Read again the next time the page is drawn.
    pub(super) fn invalidate(&mut self) {
        self.read_for = None;
    }

    fn read(&mut self, context: &egui::Context, providers: ProviderSet) {
        let (sender, receiver) = mpsc::channel();
        let context = context.clone();
        std::thread::spawn(move || {
            let sign_ins = providers
                .iter()
                .map(|provider| (provider, poller::sign_in_report(provider)))
                .collect();
            let _ = sender.send(sign_ins);
            context.request_repaint();
        });
        self.pending = Some(receiver);
        self.read_for = Some(providers);
    }

    fn update(&mut self, context: &egui::Context, providers: ProviderSet) {
        if self.read_for != Some(providers) && self.pending.is_none() {
            self.read(context, providers);
        }
        if let Some(sign_ins) = self
            .pending
            .as_ref()
            .and_then(|pending| pending.try_recv().ok())
        {
            self.sign_ins = Some(sign_ins);
            self.pending = None;
        }
    }
}

/// What the About page says about one provider's sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignInStatus {
    Missing,
    ValidUntil(SystemTime),
    ExpiredAt(SystemTime),
    NoExpiryStored,
}

fn sign_in_status(report: Option<&SignInReport>, now: SystemTime) -> SignInStatus {
    match report.map(|report| report.expires_at) {
        None => SignInStatus::Missing,
        Some(None) => SignInStatus::NoExpiryStored,
        Some(Some(expires_at)) if expires_at > now => SignInStatus::ValidUntil(expires_at),
        Some(Some(expires_at)) => SignInStatus::ExpiredAt(expires_at),
    }
}

fn local_time(language: LanguageId, moment: SystemTime) -> String {
    moment
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| {
            crate::theme_engine::format_local_datetime(elapsed.as_secs_f64(), language.code())
        })
        .unwrap_or_else(|| "--".into())
}

fn status_text(language: LanguageId, status: SignInStatus) -> (String, egui::Color32) {
    match status {
        SignInStatus::Missing => (language.text("No sign-in found").into(), danger()),
        SignInStatus::ValidUntil(moment) => (
            format!(
                "{} {}",
                language.text("Valid until"),
                local_time(language, moment)
            ),
            success(),
        ),
        SignInStatus::ExpiredAt(moment) => (
            format!(
                "{} {}",
                language.text("Expired"),
                local_time(language, moment)
            ),
            danger(),
        ),
        SignInStatus::NoExpiryStored => (language.text("No expiry date stored").into(), muted()),
    }
}

fn source_text(language: LanguageId, report: &SignInReport) -> String {
    match &report.detail {
        Some(detail) => format!("{} · {detail}", language.text(report.source)),
        None => language.text(report.source).to_string(),
    }
}

impl StudioApp {
    pub(super) fn about_page(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        let providers = self.settings.enabled_providers();
        self.about.update(ui.ctx(), providers);
        settings_scroll_area(ui, |ui| {
            section(ui, language.text("About"), |ui| {
                setting_row(
                    ui,
                    language.strings().window_title,
                    &format!("{} {}", language.text("Version"), env!("CARGO_PKG_VERSION")),
                    |_| {},
                );
                for (holder, role) in COPYRIGHTS {
                    setting_separator(ui);
                    setting_row(ui, holder, language.text(role), |_| {});
                }
            });
            section(ui, language.text("Active providers"), |ui| {
                let now = SystemTime::now();
                if providers.is_empty() {
                    setting_row(ui, language.text("No providers are enabled"), "", |_| {});
                }
                for (index, provider) in providers.iter().enumerate() {
                    if index > 0 {
                        setting_separator(ui);
                    }
                    let name = language.text(provider.descriptor().display_name);
                    let report = self.about.sign_ins.as_ref().and_then(|sign_ins| {
                        sign_ins
                            .iter()
                            .find(|(id, _)| *id == provider)
                            .map(|(_, report)| report.as_ref())
                    });
                    match report {
                        // Not read yet, or read before this provider was enabled.
                        None => setting_row(ui, name, language.text("Reading sign-in…"), |_| {}),
                        Some(report) => {
                            let source = report
                                .map(|report| source_text(language, report))
                                .unwrap_or_default();
                            let (status, color) =
                                status_text(language, sign_in_status(report, now));
                            setting_row(ui, name, &source, |ui| {
                                ui.label(egui::RichText::new(status).color(color));
                            });
                        }
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn report(expires_at: Option<SystemTime>) -> SignInReport {
        SignInReport {
            source: "Claude desktop app",
            detail: None,
            expires_at,
        }
    }

    #[test]
    fn a_sign_in_is_valid_until_its_expiry() {
        assert_eq!(
            sign_in_status(Some(&report(Some(at(NOW + 60)))), at(NOW)),
            SignInStatus::ValidUntil(at(NOW + 60))
        );
    }

    #[test]
    fn a_sign_in_expiring_now_has_expired() {
        assert_eq!(
            sign_in_status(Some(&report(Some(at(NOW)))), at(NOW)),
            SignInStatus::ExpiredAt(at(NOW))
        );
    }

    #[test]
    fn missing_and_undated_sign_ins_are_told_apart() {
        assert_eq!(sign_in_status(None, at(NOW)), SignInStatus::Missing);
        assert_eq!(
            sign_in_status(Some(&report(None)), at(NOW)),
            SignInStatus::NoExpiryStored
        );
    }

    #[test]
    fn the_source_names_its_detail_when_there_is_one() {
        let language = LanguageId::English;
        let mut from_environment = report(None);
        from_environment.source = "Environment variable";
        from_environment.detail = Some("CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN".into());
        assert_eq!(
            source_text(language, &from_environment),
            "Environment variable · CLAUDECODEUSAGE_CURSOR_SESSION_TOKEN"
        );
        assert_eq!(source_text(language, &report(None)), "Claude desktop app");
    }
}
