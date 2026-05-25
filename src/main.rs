mod libemax;
mod storage;

use std::sync::mpsc::{self, Receiver};
use std::thread;

use chrono::{Datelike, Days, Local, NaiveDate, NaiveTime, Timelike};
use eframe::egui::{self, Color32, RichText};
use egui_extras::DatePickerButton;

use crate::libemax::{
    BatchBookingResult, BookingSubmission, DiscoveryBundle, LibemaxApi, LoginOutcome, LookupItem,
    MailAuthenticationPrompt, list_dates_inclusive,
};
use crate::storage::{
    SavedSettings, delete_cookie_store, has_cookie_store, load_settings, save_settings,
};

const WORKDAY_START_MINUTES: i32 = 8 * 60;
const WORKDAY_END_MINUTES: i32 = 19 * 60;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 860.0])
            .with_min_inner_size([860.0, 760.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Libemin",
        options,
        Box::new(|_cc| Ok(Box::new(LibeminApp::new()))),
    )
}

#[derive(Default)]
struct StatusMessage {
    kind: StatusKind,
    text: String,
}

#[derive(Default, Clone, Copy)]
enum StatusKind {
    #[default]
    Info,
    Success,
    Error,
    Warning,
}

#[derive(Clone)]
struct DailyOverride {
    date: String,
    enabled: bool,
    client_id: String,
    morning_start_time: String,
    morning_end_time: String,
    afternoon_start_time: String,
    afternoon_end_time: String,
}

impl DailyOverride {
    fn new(
        date: String,
        client_id: String,
        morning_start_time: String,
        morning_end_time: String,
        afternoon_start_time: String,
        afternoon_end_time: String,
    ) -> Self {
        Self {
            date,
            enabled: false,
            client_id,
            morning_start_time,
            morning_end_time,
            afternoon_start_time,
            afternoon_end_time,
        }
    }
}

struct BatchBookingDraft {
    range_start: String,
    range_end: String,
    default_morning_start_time: String,
    default_morning_end_time: String,
    default_afternoon_start_time: String,
    default_afternoon_end_time: String,
    selected_workplace: Option<LookupItem>,
    day_overrides: Vec<DailyOverride>,
}

impl BatchBookingDraft {
    fn with_today() -> Self {
        let today = Local::now().format("%d/%m/%Y").to_string();
        Self {
            range_start: today.clone(),
            range_end: today,
            default_morning_start_time: "09:00".to_owned(),
            default_morning_end_time: "13:00".to_owned(),
            default_afternoon_start_time: "14:00".to_owned(),
            default_afternoon_end_time: "18:00".to_owned(),
            selected_workplace: None,
            day_overrides: Vec::new(),
        }
    }
}

fn format_day_label(date: &str) -> String {
    match NaiveDate::parse_from_str(date, "%d/%m/%Y") {
        Ok(parsed_date) => {
            let weekday = match parsed_date.weekday().num_days_from_monday() {
                0 => "Lunedi'",
                1 => "Martedi'",
                2 => "Mercoledi'",
                3 => "Giovedi'",
                4 => "Venerdi'",
                5 => "Sabato",
                _ => "Domenica",
            };
            format!("{} {}", weekday, date)
        }
        Err(_) => date.to_owned(),
    }
}

fn compact_combo_label(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();

    if count <= max_chars {
        return trimmed.to_owned();
    }

    let shortened: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", shortened)
}

fn parse_ui_date(date: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(date.trim(), "%d/%m/%Y").ok()
}

fn set_ui_date(target: &mut String, date: NaiveDate) {
    *target = date.format("%d/%m/%Y").to_string();
}

fn parse_ui_time(time: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()
}

fn minutes_from_time(time: NaiveTime) -> i32 {
    (time.hour() as i32) * 60 + (time.minute() as i32)
}

fn is_within_workday(time: NaiveTime) -> bool {
    let minutes = minutes_from_time(time);
    (WORKDAY_START_MINUTES..=WORKDAY_END_MINUTES).contains(&minutes)
}

fn parse_minutes_from_midnight(value: &str, default: &str) -> i32 {
    parse_ui_time(value)
        .or_else(|| parse_ui_time(default))
        .map(minutes_from_time)
        .unwrap_or(WORKDAY_START_MINUTES)
        .clamp(WORKDAY_START_MINUTES, WORKDAY_END_MINUTES)
}

fn format_minutes_from_midnight(minutes: i32) -> String {
    let clamped_minutes = minutes.clamp(WORKDAY_START_MINUTES, WORKDAY_END_MINUTES);
    let hour = clamped_minutes / 60;
    let minute = clamped_minutes % 60;
    format!("{:02}:{:02}", hour, minute)
}

fn date_picker(ui: &mut egui::Ui, value: &mut String, id_salt: &str) {
    let mut selected_date = parse_ui_date(value).unwrap_or_else(|| Local::now().date_naive());
    let previous_date = selected_date;

    ui.add(
        DatePickerButton::new(&mut selected_date)
            .id_salt(id_salt)
            .format("%d/%m/%Y")
            .calendar_week(false),
    );

    if selected_date != previous_date || value.trim().is_empty() {
        set_ui_date(value, selected_date);
    }
}

fn time_picker(ui: &mut egui::Ui, value: &mut String, id_prefix: &str) {
    let presets: Vec<i32> = (WORKDAY_START_MINUTES..=WORKDAY_END_MINUTES)
        .step_by(30)
        .collect();
    let mut minutes = parse_minutes_from_midnight(value, "08:00");

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(format!("{}_preset", id_prefix))
            .width(90.0)
            .selected_text(format_minutes_from_midnight(minutes))
            .show_ui(ui, |ui| {
                for preset in presets {
                    if ui
                        .selectable_label(
                            round_to_half_hour_minutes(minutes) == preset,
                            format_minutes_from_midnight(preset),
                        )
                        .clicked()
                    {
                        minutes = preset;
                    }
                }
            });

        ui.vertical(|ui| {
            if ui
                .small_button("^")
                .on_hover_text("Aumenta di 1 minuto")
                .clicked()
            {
                minutes = (minutes + 1).clamp(WORKDAY_START_MINUTES, WORKDAY_END_MINUTES);
            }
            if ui
                .small_button("v")
                .on_hover_text("Riduci di 1 minuto")
                .clicked()
            {
                minutes = (minutes - 1).clamp(WORKDAY_START_MINUTES, WORKDAY_END_MINUTES);
            }
        });
    });

    *value = format_minutes_from_midnight(minutes);
}

fn round_to_half_hour_minutes(minutes: i32) -> i32 {
    (((minutes + 15) / 30) * 30).clamp(WORKDAY_START_MINUTES, WORKDAY_END_MINUTES)
}

fn build_segment_submission(
    date: &str,
    client_id: &str,
    segment_label: &str,
    start_time: &str,
    end_time: &str,
) -> Result<Option<BookingSubmission>, String> {
    let start_time = start_time.trim();
    let end_time = end_time.trim();

    if start_time.is_empty() && end_time.is_empty() {
        return Ok(None);
    }

    if start_time.is_empty() || end_time.is_empty() {
        return Err(format!(
            "{}: compila sia l'inizio che la fine per la fascia {}",
            format_day_label(date),
            segment_label
        ));
    }

    let start_time = parse_ui_time(start_time).ok_or_else(|| {
        format!(
            "{}: orario non valido per la fascia {}",
            format_day_label(date),
            segment_label
        )
    })?;
    let end_time = parse_ui_time(end_time).ok_or_else(|| {
        format!(
            "{}: orario non valido per la fascia {}",
            format_day_label(date),
            segment_label
        )
    })?;

    if !is_within_workday(start_time) || !is_within_workday(end_time) {
        return Err(format!(
            "{}: gli orari della fascia {} devono stare tra 08:00 e 19:00",
            format_day_label(date),
            segment_label
        ));
    }

    let start_time = start_time.format("%H:%M").to_string();
    let end_time = end_time.format("%H:%M").to_string();

    Ok(Some(BookingSubmission {
        employee_id: String::new(),
        client_id: client_id.to_owned(),
        activity_id: String::new(),
        state_id: String::new(),
        entry_type: String::new(),
        start_date: date.to_owned(),
        end_date: date.to_owned(),
        start_time,
        end_time,
        total_hours: String::new(),
        description: String::new(),
        note: String::new(),
        productivity_json: String::new(),
    }))
}

struct LibeminApp {
    settings: SavedSettings,
    password: String,
    otp_code: String,
    remember_me: bool,
    session_active: bool,
    api: Option<LibemaxApi>,
    pending_mail_authentication: Option<MailAuthenticationPrompt>,
    discovery: Option<DiscoveryBundle>,
    batch_booking: BatchBookingDraft,
    workplace_options: Vec<LookupItem>,
    batch_results: Vec<BatchBookingResult>,
    status: StatusMessage,
    busy_message: Option<String>,
    pending_job: Option<PendingJob>,
}

struct PendingJob {
    receiver: Receiver<JobResult>,
}

enum JobResult {
    Login(Result<LoginSuccess, String>),
    VerifyMailCode(Result<LoginSuccess, String>),
    ResendMailCode(Result<String, String>),
    RefreshDiscovery(Result<RefreshDiscoverySuccess, String>),
    RestoreSession(Result<RestoreSessionSuccess, String>),
    SubmitBatch(Result<SubmitBatchSuccess, String>),
}

struct LoginSuccess {
    api: LibemaxApi,
    status_kind: StatusKind,
    status_text: String,
    clear_otp: bool,
    pending_mail_authentication: Option<MailAuthenticationPrompt>,
    discovery: Option<DiscoveryBundle>,
    workplaces: Vec<LookupItem>,
}

struct RefreshDiscoverySuccess {
    discovery: DiscoveryBundle,
    workplaces: Vec<LookupItem>,
    status_kind: StatusKind,
    status_text: String,
}

struct RestoreSessionSuccess {
    api: LibemaxApi,
    discovery: DiscoveryBundle,
    workplaces: Vec<LookupItem>,
    status_kind: StatusKind,
    status_text: String,
}

struct SubmitBatchSuccess {
    results: Vec<BatchBookingResult>,
    status_kind: StatusKind,
    status_text: String,
}

impl LibeminApp {
    fn new() -> Self {
        let mut app = Self {
            settings: load_settings(),
            password: String::new(),
            otp_code: String::new(),
            remember_me: true,
            session_active: false,
            api: None,
            pending_mail_authentication: None,
            discovery: None,
            batch_booking: BatchBookingDraft::with_today(),
            workplace_options: Vec::new(),
            batch_results: Vec::new(),
            status: StatusMessage::default(),
            busy_message: None,
            pending_job: None,
        };

        app.restore_session();
        app
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status = StatusMessage {
            kind,
            text: text.into(),
        };
    }

    fn is_busy(&self) -> bool {
        self.pending_job.is_some()
    }

    fn start_job(
        &mut self,
        busy_message: impl Into<String>,
        job: impl FnOnce() -> JobResult + Send + 'static,
    ) {
        if self.is_busy() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.busy_message = Some(busy_message.into());
        self.pending_job = Some(PendingJob { receiver });

        thread::spawn(move || {
            let _ = sender.send(job());
        });
    }

    fn poll_pending_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.pending_job else {
            return;
        };

        match job.receiver.try_recv() {
            Ok(result) => {
                self.pending_job = None;
                self.busy_message = None;
                self.apply_job_result(result);
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending_job = None;
                self.busy_message = None;
                self.set_status(StatusKind::Error, "Operazione interrotta inaspettatamente");
            }
        }
    }

    fn apply_job_result(&mut self, result: JobResult) {
        match result {
            JobResult::Login(result) | JobResult::VerifyMailCode(result) => match result {
                Ok(success) => {
                    self.session_active = success.pending_mail_authentication.is_none();
                    self.api = Some(success.api);
                    self.pending_mail_authentication = success.pending_mail_authentication;
                    self.discovery = success.discovery;
                    self.workplace_options = success.workplaces;
                    if self.batch_booking.selected_workplace.is_none() {
                        self.batch_booking.selected_workplace =
                            self.workplace_options.first().cloned();
                    }
                    if success.clear_otp {
                        self.otp_code.clear();
                    }
                    let _ = save_settings(&self.settings);
                    self.set_status(success.status_kind, success.status_text);
                }
                Err(error) => {
                    self.session_active = false;
                    self.set_status(StatusKind::Error, error);
                }
            },
            JobResult::ResendMailCode(result) => match result {
                Ok(message) => self.set_status(StatusKind::Success, message),
                Err(error) => self.set_status(StatusKind::Error, error),
            },
            JobResult::RefreshDiscovery(result) => match result {
                Ok(success) => {
                    self.discovery = Some(success.discovery);
                    self.session_active = true;
                    self.workplace_options = success.workplaces;
                    if self.batch_booking.selected_workplace.is_none() {
                        self.batch_booking.selected_workplace =
                            self.workplace_options.first().cloned();
                    }
                    let _ = save_settings(&self.settings);
                    self.set_status(success.status_kind, success.status_text);
                }
                Err(error) => {
                    if error.contains("Sessione non autenticata") {
                        self.session_expired();
                    } else {
                        self.set_status(StatusKind::Error, error);
                    }
                }
            },
            JobResult::RestoreSession(result) => match result {
                Ok(success) => {
                    self.session_active = true;
                    self.api = Some(success.api);
                    self.discovery = Some(success.discovery);
                    self.workplace_options = success.workplaces;
                    if self.batch_booking.selected_workplace.is_none() {
                        self.batch_booking.selected_workplace =
                            self.workplace_options.first().cloned();
                    }
                    self.set_status(success.status_kind, success.status_text);
                }
                Err(error) => self.set_status(StatusKind::Warning, error),
            },
            JobResult::SubmitBatch(result) => match result {
                Ok(success) => {
                    self.batch_results = success.results;
                    self.set_status(success.status_kind, success.status_text);
                }
                Err(error) => self.set_status(StatusKind::Error, error),
            },
        }
    }

    fn login(&mut self) {
        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();
        let username = self.settings.username.clone();
        let password = self.password.clone();
        let remember_me = self.remember_me;
        let insert_form_url_override = self.settings.insert_form_url_override.clone();

        self.start_job("Login in corso...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::Login(Err(error.to_string())),
            };

            match api.login(&username, &password, None, remember_me) {
                Ok(LoginOutcome::Authenticated) => {
                    match api.discover(Some(&insert_form_url_override)) {
                        Ok(discovery) => {
                            let workplaces = api
                                .preload_clients(&discovery.insert_form)
                                .map(|mut items| {
                                    items.sort_by(|left, right| left.text.cmp(&right.text));
                                    items
                                })
                                .unwrap_or_default();

                            JobResult::Login(Ok(LoginSuccess {
                                api,
                                status_kind: StatusKind::Success,
                                status_text: "Login effettuato".to_owned(),
                                clear_otp: true,
                                pending_mail_authentication: None,
                                discovery: Some(discovery),
                                workplaces,
                            }))
                        }
                        Err(error) => JobResult::Login(Err(error.to_string())),
                    }
                }
                Ok(LoginOutcome::NeedsMailAuthentication(prompt)) => {
                    JobResult::Login(Ok(LoginSuccess {
                        api,
                        status_kind: StatusKind::Warning,
                        status_text: format!("Serve il codice ricevuto via mail: {}", prompt.email),
                        clear_otp: false,
                        pending_mail_authentication: Some(prompt),
                        discovery: None,
                        workplaces: Vec::new(),
                    }))
                }
                Err(error) => JobResult::Login(Err(error.to_string())),
            }
        });
    }

    fn verify_mail_code(&mut self) {
        if self.api.is_none() {
            self.set_status(StatusKind::Error, "Esegui prima il login");
            return;
        }

        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();
        let username = self.settings.username.clone();
        let password = self.password.clone();
        let otp_code = self.otp_code.clone();
        let remember_me = self.remember_me;
        let insert_form_url_override = self.settings.insert_form_url_override.clone();

        self.start_job("Verifica codice in corso...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::VerifyMailCode(Err(error.to_string())),
            };

            match api.login(&username, &password, Some(&otp_code), remember_me) {
                Ok(LoginOutcome::Authenticated) => {
                    match api.discover(Some(&insert_form_url_override)) {
                        Ok(discovery) => {
                            let workplaces = api
                                .preload_clients(&discovery.insert_form)
                                .map(|mut items| {
                                    items.sort_by(|left, right| left.text.cmp(&right.text));
                                    items
                                })
                                .unwrap_or_default();

                            JobResult::VerifyMailCode(Ok(LoginSuccess {
                                api,
                                status_kind: StatusKind::Success,
                                status_text: "Codice verificato, sessione attiva".to_owned(),
                                clear_otp: true,
                                pending_mail_authentication: None,
                                discovery: Some(discovery),
                                workplaces,
                            }))
                        }
                        Err(error) => JobResult::VerifyMailCode(Err(error.to_string())),
                    }
                }
                Ok(LoginOutcome::NeedsMailAuthentication(_)) => JobResult::VerifyMailCode(Err(
                    "Il server richiede ancora il codice mail".to_owned(),
                )),
                Err(error) => JobResult::VerifyMailCode(Err(error.to_string())),
            }
        });
    }

    fn resend_mail_code(&mut self) {
        if self.api.is_none() {
            self.set_status(StatusKind::Error, "Esegui prima il login");
            return;
        }
        let Some(prompt) = &self.pending_mail_authentication else {
            self.set_status(StatusKind::Error, "Non c'e' nessun codice da reinviare");
            return;
        };

        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();
        let user_id = prompt.user_id.clone();

        self.start_job("Invio nuovo codice...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::ResendMailCode(Err(error.to_string())),
            };

            JobResult::ResendMailCode(
                api.resend_mail_code(&user_id)
                    .map_err(|error| error.to_string()),
            )
        });
    }

    fn refresh_discovery(&mut self) {
        if self.api.is_none() {
            self.set_status(StatusKind::Error, "Esegui prima il login");
            return;
        }

        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();
        let insert_form_url_override = self.settings.insert_form_url_override.clone();

        self.start_job("Aggiornamento dati...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::RefreshDiscovery(Err(error.to_string())),
            };

            match api.discover(Some(&insert_form_url_override)) {
                Ok(discovery) => {
                    let workplaces = api
                        .preload_clients(&discovery.insert_form)
                        .map(|mut items| {
                            items.sort_by(|left, right| left.text.cmp(&right.text));
                            items
                        })
                        .unwrap_or_default();

                    JobResult::RefreshDiscovery(Ok(RefreshDiscoverySuccess {
                        discovery,
                        workplaces,
                        status_kind: StatusKind::Success,
                        status_text: "Endpoint e form ore scoperti correttamente".to_owned(),
                    }))
                }
                Err(error) => JobResult::RefreshDiscovery(Err(error.to_string())),
            }
        });
    }

    fn restore_session(&mut self) {
        if self.settings.username.trim().is_empty() || !has_cookie_store() {
            return;
        }

        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();
        let insert_form_url_override = self.settings.insert_form_url_override.clone();

        self.start_job("Ripristino sessione...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::RestoreSession(Err(error.to_string())),
            };

            match api.discover(Some(&insert_form_url_override)) {
                Ok(discovery) => {
                    let workplaces = api
                        .preload_clients(&discovery.insert_form)
                        .map(|mut items| {
                            items.sort_by(|left, right| left.text.cmp(&right.text));
                            items
                        })
                        .unwrap_or_default();

                    JobResult::RestoreSession(Ok(RestoreSessionSuccess {
                        api,
                        discovery,
                        workplaces,
                        status_kind: StatusKind::Success,
                        status_text: "Sessione ripristinata".to_owned(),
                    }))
                }
                Err(error) => {
                    if error.to_string().contains("Sessione non autenticata") {
                        let _ = api.clear_persisted_session();
                        JobResult::RestoreSession(Err(
                            "Sessione salvata scaduta, effettua di nuovo il login".to_owned(),
                        ))
                    } else {
                        JobResult::RestoreSession(Err(format!(
                            "Impossibile ripristinare la sessione: {}",
                            error
                        )))
                    }
                }
            }
        });
    }

    fn logout(&mut self) {
        if let Some(api) = &self.api {
            let _ = api.clear_persisted_session();
        } else {
            let _ = delete_cookie_store();
        }

        self.session_active = false;
        self.api = None;
        self.pending_mail_authentication = None;
        self.discovery = None;
        self.workplace_options.clear();
        self.batch_results.clear();
        self.password.clear();
        self.otp_code.clear();
        self.set_status(StatusKind::Info, "Sessione rimossa");
    }

    fn session_expired(&mut self) {
        self.logout();
        self.set_status(
            StatusKind::Warning,
            "La sessione Libemax e' scaduta, effettua di nuovo il login",
        );
    }

    fn generate_day_overrides(&mut self) {
        let dates = match list_dates_inclusive(
            &self.batch_booking.range_start,
            &self.batch_booking.range_end,
        ) {
            Ok(dates) => dates,
            Err(error) => {
                self.set_status(StatusKind::Error, error.to_string());
                return;
            }
        };

        let default_client_id = self
            .batch_booking
            .selected_workplace
            .as_ref()
            .map(|item| item.id.clone())
            .unwrap_or_default();

        self.batch_booking.day_overrides = dates
            .into_iter()
            .map(|date| {
                DailyOverride::new(
                    date,
                    default_client_id.clone(),
                    self.batch_booking.default_morning_start_time.clone(),
                    self.batch_booking.default_morning_end_time.clone(),
                    self.batch_booking.default_afternoon_start_time.clone(),
                    self.batch_booking.default_afternoon_end_time.clone(),
                )
            })
            .collect();

        self.batch_results.clear();
        self.set_status(
            StatusKind::Info,
            "Giorni generati per la prenotazione multipla",
        );
    }

    fn build_batch_submissions(
        &self,
        default_workplace: &str,
    ) -> Result<Vec<BookingSubmission>, String> {
        let mut submissions = Vec::new();

        for day in &self.batch_booking.day_overrides {
            let client_id = if day.enabled {
                day.client_id.as_str()
            } else {
                default_workplace
            };

            let (morning_start_time, morning_end_time, afternoon_start_time, afternoon_end_time) =
                if day.enabled {
                    (
                        day.morning_start_time.as_str(),
                        day.morning_end_time.as_str(),
                        day.afternoon_start_time.as_str(),
                        day.afternoon_end_time.as_str(),
                    )
                } else {
                    (
                        self.batch_booking.default_morning_start_time.as_str(),
                        self.batch_booking.default_morning_end_time.as_str(),
                        self.batch_booking.default_afternoon_start_time.as_str(),
                        self.batch_booking.default_afternoon_end_time.as_str(),
                    )
                };

            if let Some(submission) = build_segment_submission(
                &day.date,
                client_id,
                "mattina",
                morning_start_time,
                morning_end_time,
            )? {
                submissions.push(submission);
            }

            if let Some(submission) = build_segment_submission(
                &day.date,
                client_id,
                "pomeriggio",
                afternoon_start_time,
                afternoon_end_time,
            )? {
                submissions.push(submission);
            }
        }

        if submissions.is_empty() {
            return Err("Non c'e' nessuna fascia oraria da inviare".to_owned());
        }

        Ok(submissions)
    }

    fn submit_batch_bookings(&mut self) {
        if self.batch_booking.selected_workplace.is_none() {
            self.set_status(StatusKind::Error, "Seleziona un luogo di lavoro");
            return;
        }

        if self.batch_booking.day_overrides.is_empty() {
            self.generate_day_overrides();
            if self.batch_booking.day_overrides.is_empty() {
                return;
            }
        }

        let Some(_) = &self.api else {
            self.set_status(StatusKind::Error, "Esegui prima il login");
            return;
        };
        let Some(insert_form) = self
            .discovery
            .as_ref()
            .map(|value| value.insert_form.clone())
        else {
            self.set_status(
                StatusKind::Error,
                "Non ho ancora scoperto il form di inserimento ore",
            );
            return;
        };

        let default_workplace = self
            .batch_booking
            .selected_workplace
            .as_ref()
            .map(|item| item.id.clone())
            .unwrap_or_default();

        let submissions = match self.build_batch_submissions(&default_workplace) {
            Ok(submissions) => submissions,
            Err(error) => {
                self.set_status(StatusKind::Error, error);
                return;
            }
        };

        let base_url = self.settings.base_url.clone();
        let language = self.settings.language.clone();

        self.start_job("Invio prenotazioni in corso...", move || {
            let api = match LibemaxApi::new(&base_url, &language) {
                Ok(api) => api,
                Err(error) => return JobResult::SubmitBatch(Err(error.to_string())),
            };

            let results = api.submit_bookings_batch(&insert_form, &submissions);
            let success_count = results
                .iter()
                .filter(|result| result.result.is_ok())
                .count();
            let failure_count = results.len().saturating_sub(success_count);
            let (status_kind, status_text) = if failure_count == 0 {
                (
                    StatusKind::Success,
                    format!("Prenotazioni create: {} inserimenti", success_count),
                )
            } else {
                (
                    StatusKind::Warning,
                    format!(
                        "Prenotazione multipla completata con {} inserimenti riusciti e {} errori",
                        success_count, failure_count
                    ),
                )
            };

            JobResult::SubmitBatch(Ok(SubmitBatchSuccess {
                results,
                status_kind,
                status_text,
            }))
        });
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        if self.status.text.is_empty() {
            return;
        }

        let color = match self.status.kind {
            StatusKind::Info => Color32::from_rgb(90, 140, 255),
            StatusKind::Success => Color32::from_rgb(60, 170, 110),
            StatusKind::Error => Color32::from_rgb(220, 80, 80),
            StatusKind::Warning => Color32::from_rgb(220, 160, 60),
        };

        ui.colored_label(color, &self.status.text);
    }

    fn is_logged_in(&self) -> bool {
        self.session_active && self.pending_mail_authentication.is_none()
    }
}

impl Drop for LibeminApp {
    fn drop(&mut self) {
        let _ = save_settings(&self.settings);
    }
}

impl eframe::App for LibeminApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_pending_job(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Libemin");
            ui.label("Helper desktop leggero per Libemax: login, discovery del form ore e invio diretto via HTTP.");
            ui.small("La password non viene salvata. La sessione viene riusata localmente finche' Libemax la mantiene valida.");
            ui.separator();
            ui.add_space(8.0);

            let is_busy = self.is_busy();

            ui.add_enabled_ui(!is_busy, |ui| {
                if self.is_logged_in() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("●").color(Color32::from_rgb(60, 170, 110)));
                            ui.label(RichText::new("Sessione attiva").strong());
                            if !self.settings.username.trim().is_empty() {
                                ui.small(format!("{}", self.settings.username.trim()));
                            }
                            ui.separator();
                            ui.small("login riusato da sessione locale");
                            if ui.button("Aggiorna").clicked() {
                                self.refresh_discovery();
                            }
                            if ui.button("Disconnetti").clicked() {
                                self.logout();
                            }
                        });
                    });
                } else {
                    ui.group(|ui| {
                        ui.label(RichText::new("Accedi").strong());
                        ui.horizontal(|ui| {
                            ui.label("Base URL");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.settings.base_url)
                                    .desired_width(320.0)
                                    .hint_text("https://azienda.libemax.com"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Language");
                            ui.add(egui::TextEdit::singleline(&mut self.settings.language).desired_width(60.0));
                            ui.label("Username");
                            ui.add(egui::TextEdit::singleline(&mut self.settings.username).desired_width(220.0));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Password");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.password)
                                    .password(true)
                                    .desired_width(220.0),
                            );
                            ui.checkbox(&mut self.remember_me, "remember=1");
                            if ui.button("Login").clicked() {
                                self.login();
                            }
                        });

                        if let Some(prompt) = &self.pending_mail_authentication {
                            ui.separator();
                            ui.label(
                                RichText::new(format!("Codice mail richiesto per {}", prompt.email))
                                    .color(Color32::from_rgb(220, 160, 60)),
                            );
                            ui.horizontal(|ui| {
                                ui.label("OTP");
                                ui.add(egui::TextEdit::singleline(&mut self.otp_code).desired_width(120.0));
                                if ui.button("Verifica codice").clicked() {
                                    self.verify_mail_code();
                                }
                                if ui.button("Invia nuovo codice").clicked() {
                                    self.resend_mail_code();
                                }
                            });
                        }
                    });
                }

                ui.add_space(10.0);
                ui.separator();
                ui.collapsing("Diagnostica", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Dominio Libemax");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.base_url)
                                .desired_width(320.0)
                                .hint_text("https://azienda.libemax.com"),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Insert form URL");
                        ui.text_edit_singleline(&mut self.settings.insert_form_url_override);
                        if ui.button("Refresh discovery").clicked() {
                            self.refresh_discovery();
                        }
                    });

                    if let Some(discovery) = &self.discovery {
                        ui.monospace(format!("Admin page: {}", discovery.admin_page_url));
                        ui.monospace(format!("Insert page: {}", discovery.insert_form_url));
                        if !discovery.candidate_insert_urls.is_empty() {
                            ui.small("Candidate URLs trovati nella pagina autenticata:");
                            for candidate in &discovery.candidate_insert_urls {
                                ui.monospace(candidate);
                            }
                        }
                        ui.separator();
                        ui.monospace(format!("Form action: {}", discovery.insert_form.action_url));
                        if let Some(success) = &discovery.insert_form.success_url {
                            ui.monospace(format!("Success redirect: {}", success));
                        }
                        ui.monospace(format!(
                            "Employee lookup: {}",
                            discovery
                                .insert_form
                                .lookup_endpoints
                                .employee
                                .as_deref()
                                .unwrap_or("<not found>")
                        ));
                        ui.monospace(format!(
                            "Client lookup: {}",
                            discovery
                                .insert_form
                                .lookup_endpoints
                                .client
                                .as_deref()
                                .unwrap_or("<not found>")
                        ));
                        ui.monospace(format!(
                            "Activity lookup: {}",
                            discovery
                                .insert_form
                                .lookup_endpoints
                                .activity
                                .as_deref()
                                .unwrap_or("<not found>")
                        ));
                        ui.small(format!("Captured default fields: {}", discovery.insert_form.base_fields.len()));
                    } else {
                        ui.small("Nessun endpoint scoperto ancora.");
                    }
                });

                ui.add_space(12.0);
                ui.heading("Prenotazione Multipla");
                ui.small("Seleziona intervallo, luogo di lavoro predefinito e orario standard. Poi abilita override solo nei giorni da modificare.");
                ui.add_space(10.0);
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());

                    egui::Grid::new("batch_booking_form")
                        .num_columns(2)
                        .spacing([18.0, 12.0])
                        .min_col_width(150.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new("Periodo").strong());
                            ui.horizontal(|ui| {
                                ui.label("Da");
                                date_picker(ui, &mut self.batch_booking.range_start, "range_start_picker");
                                ui.add_space(8.0);
                                ui.label("A");
                                date_picker(ui, &mut self.batch_booking.range_end, "range_end_picker");
                                ui.add_space(12.0);
                                if ui.button("Settimana corrente").clicked() {
                                    let today = Local::now().date_naive();
                                    let weekday = today.weekday().num_days_from_monday() as u64;
                                    if let Some(monday) = today.checked_sub_days(Days::new(weekday)) {
                                        if let Some(friday) = monday.checked_add_days(Days::new(4)) {
                                            self.batch_booking.range_start =
                                                monday.format("%d/%m/%Y").to_string();
                                            self.batch_booking.range_end =
                                                friday.format("%d/%m/%Y").to_string();
                                        }
                                    }
                                }
                            });
                            ui.end_row();

                            ui.label(RichText::new("Luogo di lavoro").strong());
                            ui.horizontal(|ui| {
                                egui::ComboBox::from_id_salt("workplace_picker")
                                    .width(220.0)
                                    .selected_text(
                                        self.batch_booking
                                            .selected_workplace
                                            .as_ref()
                                            .map(|item| compact_combo_label(&item.text, 24))
                                            .unwrap_or_else(|| "Seleziona luogo di lavoro".to_owned()),
                                    )
                                    .show_ui(ui, |ui| {
                                        for item in &self.workplace_options {
                                            let is_selected = self
                                                .batch_booking
                                                .selected_workplace
                                                .as_ref()
                                                .map(|selected| selected.id == item.id)
                                                .unwrap_or(false);
                                            if ui.selectable_label(is_selected, &item.text).clicked() {
                                                self.batch_booking.selected_workplace = Some(item.clone());
                                            }
                                        }
                                    });
                                if ui.button("Ricarica").clicked() {
                                    self.refresh_discovery();
                                }
                            });
                            ui.end_row();

                            ui.label(RichText::new("Mattina").strong());
                            ui.horizontal(|ui| {
                                time_picker(
                                    ui,
                                    &mut self.batch_booking.default_morning_start_time,
                                    "default_morning_start_time",
                                );
                                ui.label("fino alle");
                                time_picker(
                                    ui,
                                    &mut self.batch_booking.default_morning_end_time,
                                    "default_morning_end_time",
                                );
                            });
                            ui.end_row();

                            ui.label(RichText::new("Pomeriggio").strong());
                            ui.horizontal(|ui| {
                                time_picker(
                                    ui,
                                    &mut self.batch_booking.default_afternoon_start_time,
                                    "default_afternoon_start_time",
                                );
                                ui.label("fino alle");
                                time_picker(
                                    ui,
                                    &mut self.batch_booking.default_afternoon_end_time,
                                    "default_afternoon_end_time",
                                );
                                ui.add_space(12.0);
                                if ui.button("Preset 8h").clicked() {
                                    self.batch_booking.default_morning_start_time = "09:00".to_owned();
                                    self.batch_booking.default_morning_end_time = "13:00".to_owned();
                                    self.batch_booking.default_afternoon_start_time = "14:00".to_owned();
                                    self.batch_booking.default_afternoon_end_time = "18:00".to_owned();
                                }
                            });
                            ui.end_row();
                        });

                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Genera giorni").clicked() {
                            self.generate_day_overrides();
                        }
                        if ui.button("Invia prenotazioni multiple").clicked() {
                            self.submit_batch_bookings();
                        }
                    });
                });

                if !self.batch_booking.day_overrides.is_empty() {
                    ui.add_space(14.0);
                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(RichText::new("Override giornalieri").strong());
                        ui.small("Attiva solo i giorni che vuoi personalizzare rispetto ai default.");
                        ui.add_space(8.0);

                        egui::ScrollArea::horizontal().show(ui, |ui| {
                            egui::Grid::new("day_overrides_grid")
                                .num_columns(7)
                                .spacing([16.0, 10.0])
                                .striped(true)
                                .show(ui, |ui| {
                                    ui.label(RichText::new("Giorno").strong());
                                    ui.label(RichText::new("Override").strong());
                                    ui.label(RichText::new("Luogo di lavoro").strong());
                                    ui.label(RichText::new("Mattina dalle").strong());
                                    ui.label(RichText::new("Mattina alle").strong());
                                    ui.label(RichText::new("Pomeriggio dalle").strong());
                                    ui.label(RichText::new("Pomeriggio alle").strong());
                                    ui.end_row();

                                    for day in &mut self.batch_booking.day_overrides {
                                        ui.label(format_day_label(&day.date));
                                        ui.checkbox(&mut day.enabled, "");

                                        ui.add_enabled_ui(day.enabled, |ui| {
                                            egui::ComboBox::from_id_salt(format!("override_workplace_{}", day.date))
                                                .width(180.0)
                                                .selected_text(
                                                    self.workplace_options
                                                        .iter()
                                                        .find(|item| item.id == day.client_id)
                                                        .map(|item| compact_combo_label(&item.text, 18))
                                                        .unwrap_or_else(|| "Default".to_owned()),
                                                )
                                                .show_ui(ui, |ui| {
                                                    for item in &self.workplace_options {
                                                        ui.selectable_value(
                                                            &mut day.client_id,
                                                            item.id.clone(),
                                                            &item.text,
                                                        );
                                                    }
                                                });
                                        });

                                        ui.add_enabled_ui(day.enabled, |ui| {
                                            time_picker(
                                                ui,
                                                &mut day.morning_start_time,
                                                &format!("{}_morning_start", day.date),
                                            );
                                        });
                                        ui.add_enabled_ui(day.enabled, |ui| {
                                            time_picker(
                                                ui,
                                                &mut day.morning_end_time,
                                                &format!("{}_morning_end", day.date),
                                            );
                                        });
                                        ui.add_enabled_ui(day.enabled, |ui| {
                                            time_picker(
                                                ui,
                                                &mut day.afternoon_start_time,
                                                &format!("{}_afternoon_start", day.date),
                                            );
                                        });
                                        ui.add_enabled_ui(day.enabled, |ui| {
                                            time_picker(
                                                ui,
                                                &mut day.afternoon_end_time,
                                                &format!("{}_afternoon_end", day.date),
                                            );
                                        });
                                        ui.end_row();
                                    }
                                });
                        });
                    });
                }

                if !self.batch_results.is_empty() {
                    ui.add_space(14.0);
                    ui.group(|ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(RichText::new("Esito prenotazioni multiple").strong());
                        ui.add_space(6.0);
                        for result in &self.batch_results {
                            match &result.result {
                                Ok(message) => {
                                    ui.colored_label(
                                        Color32::from_rgb(60, 170, 110),
                                        format!(
                                            "{} {}-{}: {}",
                                            format_day_label(&result.date),
                                            result.start_time,
                                            result.end_time,
                                            message
                                        ),
                                    );
                                }
                                Err(error) => {
                                    ui.colored_label(
                                        Color32::from_rgb(220, 80, 80),
                                        format!(
                                            "{} {}-{}: {}",
                                            format_day_label(&result.date),
                                            result.start_time,
                                            result.end_time,
                                            error
                                        ),
                                    );
                                }
                            }
                        }
                    });
                }

            });

            ui.add_space(10.0);
            ui.separator();
            self.render_status(ui);
        });

        if let Some(message) = &self.busy_message {
            egui::Area::new(egui::Id::new("busy_overlay"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(260.0);
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new().size(28.0));
                            ui.add_space(10.0);
                            ui.label(RichText::new(message).strong());
                            ui.small("Attendi il completamento dell'operazione");
                        });
                    });
                });
        }
    }
}
