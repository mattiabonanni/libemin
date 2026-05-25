use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufReader, BufWriter, ErrorKind, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Days, NaiveDate, NaiveDateTime, NaiveTime};
use cookie_store::{CookieStore, serde::json as cookie_store_json};
use regex::Regex;
use reqwest::blocking::{Client, Response};
use reqwest_cookie_store::CookieStoreMutex;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

use crate::storage::cookie_store_path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LookupItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct LookupEndpoints {
    pub employee: Option<String>,
    pub client: Option<String>,
    pub activity: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BookingFieldNames {
    pub employee: Option<String>,
    pub client: Option<String>,
    pub activity: Option<String>,
    pub state: Option<String>,
    pub entry_type: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub total_hours: Option<String>,
    pub description: Option<String>,
    pub note: Option<String>,
    pub productivity: Option<String>,
    pub force: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InsertForm {
    pub page_url: String,
    pub action_url: String,
    pub success_url: Option<String>,
    pub base_fields: BTreeMap<String, String>,
    pub lookup_endpoints: LookupEndpoints,
    pub field_names: BookingFieldNames,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveryBundle {
    pub admin_page_url: String,
    pub insert_form_url: String,
    pub candidate_insert_urls: Vec<String>,
    pub insert_form: InsertForm,
}

#[derive(Debug, Clone)]
pub struct MailAuthenticationPrompt {
    pub email: String,
    pub user_id: String,
}

#[derive(Debug, Clone)]
pub enum LoginOutcome {
    Authenticated,
    NeedsMailAuthentication(MailAuthenticationPrompt),
}

#[derive(Debug, Clone, Default)]
pub struct BookingSubmission {
    pub employee_id: String,
    pub client_id: String,
    pub activity_id: String,
    pub state_id: String,
    pub entry_type: String,
    pub start_date: String,
    pub end_date: String,
    pub start_time: String,
    pub end_time: String,
    pub total_hours: String,
    pub description: String,
    pub note: String,
    pub productivity_json: String,
}

#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    Success(String),
    NeedsForce(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct BatchBookingResult {
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub result: Result<String, String>,
}

pub struct LibemaxApi {
    base_url: Url,
    language: String,
    client: Client,
    cookie_store: Arc<CookieStoreMutex>,
    cookie_store_path: PathBuf,
}

impl LibemaxApi {
    pub fn new(base_url: &str, language: &str) -> Result<Self> {
        let base_url = Url::parse(base_url).context("invalid base URL")?;
        let cookie_store_path = cookie_store_path()?;
        let cookie_store = load_cookie_store(&cookie_store_path)?;
        let client = Client::builder()
            .cookie_provider(cookie_store.clone())
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent("libemin/0.1")
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            base_url,
            language: language.trim().to_owned(),
            client,
            cookie_store,
            cookie_store_path,
        })
    }

    pub fn clear_persisted_session(&self) -> Result<()> {
        {
            let mut store = self
                .cookie_store
                .lock()
                .map_err(|_| anyhow!("failed to lock cookie store"))?;
            *store = CookieStore::default();
        }

        match fs::remove_file(&self.cookie_store_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to remove cookie store {}",
                    self.cookie_store_path.display()
                )
            }),
        }
    }

    pub fn login(
        &self,
        username: &str,
        password: &str,
        otp_code: Option<&str>,
        remember: bool,
    ) -> Result<LoginOutcome> {
        let mut fields = vec![
            ("username", username.trim().to_owned()),
            ("password", password.to_owned()),
        ];

        if remember {
            fields.push(("remember", "1".to_owned()));
        }

        if let Some(code) = otp_code.map(str::trim).filter(|value| !value.is_empty()) {
            fields.push(("doppia_autenticazione_codice", code.to_owned()));
        }

        let response = self
            .client
            .post(self.login_api_url()?)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&fields)
            .send()
            .context("login request failed")?;

        let body = response_to_json(response)?;
        let _ = self.persist_cookie_store();

        if truthy(body.get("error")) {
            bail!(extract_primary_error(&body));
        }

        match body
            .get("action_required")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "mail_authentication" => {
                let email = body
                    .get("email")
                    .and_then(as_string)
                    .unwrap_or_else(|| "indirizzo email non disponibile".to_owned());
                let user_id = body
                    .get("utente_id")
                    .and_then(as_string)
                    .unwrap_or_default();

                Ok(LoginOutcome::NeedsMailAuthentication(
                    MailAuthenticationPrompt { email, user_id },
                ))
            }
            "change_password" => {
                bail!(
                    "Libemax richiede il cambio password: fai il cambio dal sito una volta, poi usa l'app"
                )
            }
            _ => Ok(LoginOutcome::Authenticated),
        }
    }

    pub fn resend_mail_code(&self, user_id: &str) -> Result<String> {
        let response = self
            .client
            .post(self.mail_code_api_url()?)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&[("utente_id", user_id.trim())])
            .send()
            .context("failed to request a new mail authentication code")?;

        let body = response_to_json(response)?;
        let _ = self.persist_cookie_store();
        if truthy(body.get("error")) {
            bail!(extract_primary_error(&body));
        }

        Ok(body
            .get("confirm")
            .and_then(Value::as_str)
            .unwrap_or("Nuovo codice inviato")
            .to_owned())
    }

    pub fn discover(&self, insert_form_url_override: Option<&str>) -> Result<DiscoveryBundle> {
        let admin_page_url = self.admin_page_url()?;
        let admin_html = self.fetch_html(&admin_page_url)?;

        let candidate_insert_urls = discover_insert_candidates(&admin_html, &admin_page_url)?;
        let insert_form_url = match insert_form_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(raw) => absolutize_url(&admin_page_url, raw)?.to_string(),
            None => candidate_insert_urls
                .first()
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "Non sono riuscito a trovare automaticamente la pagina di inserimento ore. Incolla l'URL manuale nel campo 'Insert form URL'."
                    )
                })?,
        };

        let insert_form = self.load_insert_form(&insert_form_url)?;

        Ok(DiscoveryBundle {
            admin_page_url: admin_page_url.to_string(),
            insert_form_url,
            candidate_insert_urls,
            insert_form,
        })
    }

    pub fn load_insert_form(&self, url: &str) -> Result<InsertForm> {
        let page_url = absolutize_url(&self.admin_page_url()?, url)?;
        let html = self.fetch_html(&page_url)?;
        parse_insert_form(&html, &page_url)
    }

    pub fn search_lookup(&self, endpoint: &str, query: &str) -> Result<Vec<LookupItem>> {
        let endpoint = absolutize_url(&self.admin_page_url()?, endpoint)?;
        let query = query.trim();

        let get_attempt = self
            .client
            .get(endpoint.clone())
            .query(&[("q", query)])
            .send();

        if let Ok(response) = get_attempt {
            let text = response
                .text()
                .context("failed to read lookup response body")?;
            if let Ok(items) = parse_lookup_items(&text) {
                return Ok(items);
            }
        }

        let response = self
            .client
            .post(endpoint)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&[("q", query)])
            .send()
            .context("lookup request failed")?;

        let text = response
            .text()
            .context("failed to read lookup response body")?;

        parse_lookup_items(&text)
    }

    pub fn preload_clients(&self, insert_form: &InsertForm) -> Result<Vec<LookupItem>> {
        let endpoint = insert_form
            .lookup_endpoints
            .client
            .as_ref()
            .context("L'endpoint dei luoghi di lavoro non e' stato trovato")?;

        self.search_lookup(endpoint, "")
    }

    pub fn submit_booking(
        &self,
        insert_form: &InsertForm,
        submission: &BookingSubmission,
        force: bool,
    ) -> Result<SubmitOutcome> {
        let fresh_form = self
            .load_insert_form(&insert_form.page_url)
            .unwrap_or_else(|_| insert_form.clone());

        let mut fields = fresh_form.base_fields.clone();
        let default_employee_id = fresh_form
            .base_fields
            .get("globals_utente_id")
            .map(String::as_str)
            .unwrap_or_default();
        let employee_id = if submission.employee_id.trim().is_empty() {
            default_employee_id
        } else {
            submission.employee_id.trim()
        };
        let normalized_end_date = if submission.end_date.trim().is_empty() {
            submission.start_date.trim()
        } else {
            submission.end_date.trim()
        };
        let total_hours = if submission.total_hours.trim().is_empty() {
            compute_total_hours(
                &submission.start_date,
                normalized_end_date,
                &submission.start_time,
                &submission.end_time,
            )?
        } else {
            submission.total_hours.trim().to_owned()
        };

        set_optional_field(
            &mut fields,
            fresh_form.field_names.employee.as_ref(),
            employee_id,
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.client.as_ref(),
            submission.client_id.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.activity.as_ref(),
            submission.activity_id.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.state.as_ref(),
            submission.state_id.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.entry_type.as_ref(),
            submission.entry_type.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.start_date.as_ref(),
            submission.start_date.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.end_date.as_ref(),
            normalized_end_date,
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.start_time.as_ref(),
            submission.start_time.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.end_time.as_ref(),
            submission.end_time.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.total_hours.as_ref(),
            &total_hours,
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.description.as_ref(),
            submission.description.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.note.as_ref(),
            submission.note.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.productivity.as_ref(),
            submission.productivity_json.trim(),
        );
        set_optional_field(
            &mut fields,
            fresh_form.field_names.force.as_ref(),
            if force { "1" } else { "" },
        );

        let response = self
            .client
            .post(&fresh_form.action_url)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&fields)
            .send()
            .context("booking submit request failed")?;

        let body = response_to_json(response)?;

        if truthy(body.get("error")) {
            let details = extract_detail_messages(&body);
            if !force && has_forceable_validation(&body) {
                return Ok(SubmitOutcome::NeedsForce(details));
            }

            bail!(details.join("\n"));
        }

        if let Some(result) = body.get("result") {
            let is_valid = result
                .get("result")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !is_valid {
                let message = result
                    .get("id_msg")
                    .and_then(Value::as_str)
                    .unwrap_or("La timbratura non e' stata accettata dal server");
                bail!(message.to_owned());
            }
        }

        Ok(SubmitOutcome::Success(
            body.get("confirm")
                .and_then(Value::as_str)
                .unwrap_or("Timbratura salvata correttamente")
                .to_owned(),
        ))
    }

    pub fn submit_bookings_batch(
        &self,
        insert_form: &InsertForm,
        submissions: &[BookingSubmission],
    ) -> Vec<BatchBookingResult> {
        submissions
            .iter()
            .map(|submission| BatchBookingResult {
                date: submission.start_date.clone(),
                start_time: submission.start_time.clone(),
                end_time: submission.end_time.clone(),
                result: match self.submit_booking(insert_form, submission, false) {
                    Ok(SubmitOutcome::Success(message)) => Ok(message),
                    Ok(SubmitOutcome::NeedsForce(messages)) => Err(format!(
                        "Serve conferma esplicita: {}",
                        messages.join(" | ")
                    )),
                    Err(error) => Err(error.to_string()),
                },
            })
            .collect()
    }

    fn fetch_html(&self, url: &Url) -> Result<String> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .with_context(|| format!("request failed for {}", url))?;

        let text = response
            .text()
            .with_context(|| format!("failed to read HTML body from {}", url))?;
        let _ = self.persist_cookie_store();

        if looks_like_login_page(&text) {
            bail!("Sessione non autenticata o scaduta: effettua di nuovo il login")
        }

        Ok(text)
    }

    fn admin_page_url(&self) -> Result<Url> {
        self.base_url
            .join(&format!(
                "/app-timbrature/{}/admin_timbratura",
                self.language
            ))
            .context("failed to build admin page URL")
    }

    fn login_api_url(&self) -> Result<Url> {
        self.base_url
            .join(&format!(
                "/app-timbrature/{}/ajax/login.accedi",
                self.language
            ))
            .context("failed to build login API URL")
    }

    fn mail_code_api_url(&self) -> Result<Url> {
        self.base_url
            .join(&format!(
                "/app-timbrature/{}/ajax/login.invia_mail_autenticazione",
                self.language
            ))
            .context("failed to build mail code API URL")
    }
}

fn load_cookie_store(path: &PathBuf) -> Result<Arc<CookieStoreMutex>> {
    let store = match File::open(path) {
        Ok(file) => cookie_store_json::load(BufReader::new(file)).unwrap_or_default(),
        Err(error) if error.kind() == ErrorKind::NotFound => CookieStore::default(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read cookie store {}", path.display()));
        }
    };

    Ok(Arc::new(CookieStoreMutex::new(store)))
}

impl LibemaxApi {
    fn persist_cookie_store(&self) -> Result<()> {
        if let Some(parent) = self.cookie_store_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create cookie store directory {}",
                    parent.display()
                )
            })?;
        }

        let file = File::create(&self.cookie_store_path).with_context(|| {
            format!(
                "failed to create cookie store {}",
                self.cookie_store_path.display()
            )
        })?;
        let mut writer = BufWriter::new(file);
        let store = self
            .cookie_store
            .lock()
            .map_err(|_| anyhow!("failed to lock cookie store"))?;
        cookie_store_json::save(&store, &mut writer)
            .map_err(|error| anyhow!("failed to serialize cookie store: {error}"))?;
        writer.flush().context("failed to flush cookie store")?;

        Ok(())
    }
}

fn parse_insert_form(html: &str, page_url: &Url) -> Result<InsertForm> {
    let document = Html::parse_document(html);
    let form_selector = selector("form#form1, form");

    let form = document
        .select(&form_selector)
        .find(|candidate| {
            candidate.value().attr("id") == Some("form1")
                || candidate.html().contains("timbratura_data_inizio")
                || candidate.html().contains("timbratura_ora_inizio")
        })
        .context("non ho trovato il form di inserimento/modifica ore")?;

    let action_url = absolutize_url(
        page_url,
        form.value()
            .attr("action")
            .context("il form ore non espone un action URL")?,
    )?;

    let success_url = form
        .value()
        .attr("data-success")
        .map(|raw| absolutize_url(page_url, raw))
        .transpose()?
        .map(|url| url.to_string());

    let mut base_fields = BTreeMap::new();
    let mut field_names = BookingFieldNames::default();
    let mut lookup_endpoints = LookupEndpoints::default();

    parse_form_inputs(
        &form,
        page_url,
        &mut base_fields,
        &mut field_names,
        &mut lookup_endpoints,
    )?;
    parse_form_textareas(&form, &mut base_fields, &mut field_names);
    parse_form_selects(
        &form,
        page_url,
        &mut base_fields,
        &mut field_names,
        &mut lookup_endpoints,
    )?;

    Ok(InsertForm {
        page_url: page_url.to_string(),
        action_url: action_url.to_string(),
        success_url,
        base_fields,
        lookup_endpoints,
        field_names,
    })
}

fn parse_form_inputs(
    form: &ElementRef<'_>,
    page_url: &Url,
    base_fields: &mut BTreeMap<String, String>,
    field_names: &mut BookingFieldNames,
    lookup_endpoints: &mut LookupEndpoints,
) -> Result<()> {
    let input_selector = selector("input[name]");

    for input in form.select(&input_selector) {
        if input.value().attr("disabled").is_some() {
            continue;
        }

        let Some(name) = input.value().attr("name") else {
            continue;
        };

        let id = input.value().attr("id");
        assign_known_field_name(field_names, name, id);
        assign_lookup_endpoint(
            lookup_endpoints,
            page_url,
            name,
            id,
            input.value().attr("data-action"),
        )?;

        let kind = input
            .value()
            .attr("type")
            .unwrap_or("text")
            .to_ascii_lowercase();

        match kind.as_str() {
            "submit" | "button" | "reset" | "file" | "image" => {}
            "checkbox" | "radio" => {
                if input.value().attr("checked").is_some() {
                    base_fields.insert(
                        name.to_owned(),
                        input.value().attr("value").unwrap_or("on").to_owned(),
                    );
                }
            }
            _ => {
                base_fields.insert(
                    name.to_owned(),
                    input.value().attr("value").unwrap_or_default().to_owned(),
                );
            }
        }
    }

    Ok(())
}

fn parse_form_textareas(
    form: &ElementRef<'_>,
    base_fields: &mut BTreeMap<String, String>,
    field_names: &mut BookingFieldNames,
) {
    let textarea_selector = selector("textarea[name]");

    for textarea in form.select(&textarea_selector) {
        let Some(name) = textarea.value().attr("name") else {
            continue;
        };

        assign_known_field_name(field_names, name, textarea.value().attr("id"));
        let value = textarea
            .text()
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_owned();
        base_fields.insert(name.to_owned(), value);
    }
}

fn parse_form_selects(
    form: &ElementRef<'_>,
    page_url: &Url,
    base_fields: &mut BTreeMap<String, String>,
    field_names: &mut BookingFieldNames,
    lookup_endpoints: &mut LookupEndpoints,
) -> Result<Vec<LookupItem>> {
    let select_selector = selector("select[name]");
    let option_selector = selector("option");
    let mut state_options = Vec::new();

    for select in form.select(&select_selector) {
        let Some(name) = select.value().attr("name") else {
            continue;
        };

        let id = select.value().attr("id");
        assign_known_field_name(field_names, name, id);
        assign_lookup_endpoint(
            lookup_endpoints,
            page_url,
            name,
            id,
            select.value().attr("data-action"),
        )?;

        let key = format!(
            "{} {}",
            name.to_ascii_lowercase(),
            id.unwrap_or_default().to_ascii_lowercase()
        );

        let mut selected_value = None;
        let mut local_state_options = Vec::new();

        for option in select.select(&option_selector) {
            let value = option.value().attr("value").unwrap_or_default().to_owned();
            let text = option
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_owned();

            if option.value().attr("selected").is_some() {
                selected_value = Some(value.clone());
            }

            if key.contains("timbratura_stato") && !value.is_empty() {
                local_state_options.push(LookupItem { id: value, text });
            }
        }

        if let Some(value) = selected_value {
            base_fields.insert(name.to_owned(), value);
        }

        if !local_state_options.is_empty() {
            state_options = local_state_options;
        }
    }

    Ok(state_options)
}

fn assign_lookup_endpoint(
    lookup_endpoints: &mut LookupEndpoints,
    page_url: &Url,
    name: &str,
    id: Option<&str>,
    action: Option<&str>,
) -> Result<()> {
    let Some(action) = action else {
        return Ok(());
    };

    let key = format!(
        "{} {}",
        name.to_ascii_lowercase(),
        id.unwrap_or_default().to_ascii_lowercase()
    );
    let absolute = absolutize_url(page_url, action)?.to_string();

    if lookup_endpoints.employee.is_none() && key.contains("timbratura_dipendente_id") {
        lookup_endpoints.employee = Some(absolute);
    } else if lookup_endpoints.client.is_none()
        && key.contains("timbratura_cliente_id")
        && !key.contains("gruppo")
    {
        lookup_endpoints.client = Some(absolute);
    } else if lookup_endpoints.activity.is_none() && key.contains("timbratura_attivita_id") {
        lookup_endpoints.activity = Some(absolute);
    }

    Ok(())
}

fn discover_insert_candidates(html: &str, page_url: &Url) -> Result<Vec<String>> {
    let document = Html::parse_document(html);
    let selectors = [
        selector("a[href]"),
        selector("[data-action]"),
        selector("form[action]"),
    ];
    let mut scored_candidates = Vec::new();

    for selector in selectors {
        for element in document.select(&selector) {
            if let Some(raw_url) = element
                .value()
                .attr("href")
                .or_else(|| element.value().attr("data-action"))
                .or_else(|| element.value().attr("action"))
            {
                if let Some((score, url)) = score_insert_candidate(&element, page_url, raw_url) {
                    scored_candidates.push((score, url));
                }
            }
        }
    }

    let regex = Regex::new(
        r#"(?P<url>/app-timbrature/[^"'\s>]*(?:admin_timbratura|timbratura)[^"'\s>]*(?:inserisci|aggiungi|nuov|crea)[^"'\s>]*)"#,
    )?;
    for captures in regex.captures_iter(html) {
        let Some(raw) = captures.name("url") else {
            continue;
        };

        let url = absolutize_url(page_url, raw.as_str())?.to_string();
        scored_candidates.push((60, url));
    }

    let mut ordered = scored_candidates;
    ordered.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
    });

    let mut seen = BTreeSet::new();
    let mut urls = Vec::new();
    for (_, candidate) in ordered {
        if seen.insert(candidate.clone()) {
            urls.push(candidate);
        }
    }

    Ok(urls)
}

fn score_insert_candidate(
    element: &ElementRef<'_>,
    page_url: &Url,
    raw_url: &str,
) -> Option<(i32, String)> {
    let raw_url = raw_url.trim();
    if raw_url.is_empty() || raw_url.starts_with('#') || raw_url.starts_with("javascript:") {
        return None;
    }

    let absolute = absolutize_url(page_url, raw_url).ok()?;
    let url_text = absolute.as_str().to_ascii_lowercase();
    let label = element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();

    let mut score = 0;
    if url_text.contains("admin_timbratura") {
        score += 30;
    }
    if url_text.contains("timbratura") {
        score += 10;
    }
    if url_text.contains("inserisci") {
        score += 40;
    }
    if url_text.contains("aggiungi") || label.contains("aggiungi") {
        score += 35;
    }
    if url_text.contains("nuov") || label.contains("nuov") {
        score += 25;
    }
    if label.contains("timbratura") || label.contains("ore") {
        score += 15;
    }

    if score < 35 {
        return None;
    }

    Some((score, absolute.to_string()))
}

fn looks_like_login_page(html: &str) -> bool {
    html.contains("id=\"login_libemax\"") || html.contains("class=\"form-login\"")
}

fn response_to_json(response: Response) -> Result<Value> {
    let status = response.status();
    let text = response
        .text()
        .context("failed to read JSON response body")?;
    serde_json::from_str(&text).with_context(|| {
        format!(
            "unexpected server response with status {}: {}",
            status,
            text.trim()
        )
    })
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => !value.trim().is_empty() && value.trim() != "0",
        Some(Value::Number(value)) => value.as_i64().unwrap_or_default() != 0,
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        _ => false,
    }
}

fn extract_primary_error(body: &Value) -> String {
    if let Some(message) = body.get("error").and_then(Value::as_str) {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    let messages = extract_detail_messages(body);
    if !messages.is_empty() {
        return messages.join("\n");
    }

    "Richiesta rifiutata dal server".to_owned()
}

fn extract_detail_messages(body: &Value) -> Vec<String> {
    let mut messages = Vec::new();
    if let Some(details) = body.get("dettagli") {
        collect_messages(details, &mut messages);
    }

    if messages.is_empty() {
        if let Some(message) = body.get("error").and_then(Value::as_str) {
            if !message.trim().is_empty() {
                messages.push(message.trim().to_owned());
            }
        }
    }

    if messages.is_empty() {
        messages.push("Il server ha restituito un errore non dettagliato".to_owned());
    }

    messages
}

fn collect_messages(value: &Value, messages: &mut Vec<String>) {
    match value {
        Value::String(value) => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                messages.push(trimmed.to_owned());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_messages(value, messages);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_messages(value, messages);
            }
        }
        _ => {}
    }
}

fn has_forceable_validation(body: &Value) -> bool {
    body.get("dettagli")
        .and_then(Value::as_object)
        .map(|details| {
            details.contains_key("validita_sottotimbratura")
                || details.contains_key("orario_tracimato")
        })
        .unwrap_or(false)
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.to_owned()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_lookup_items(body: &str) -> Result<Vec<LookupItem>> {
    let value: Value = serde_json::from_str(body)
        .with_context(|| format!("failed to parse lookup response: {}", body.trim()))?;

    Ok(match value {
        Value::Array(items) => items.into_iter().filter_map(parse_lookup_item).collect(),
        Value::Object(map) => map
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_lookup_item)
            .collect(),
        _ => Vec::new(),
    })
}

fn parse_lookup_item(value: Value) -> Option<LookupItem> {
    match value {
        Value::Object(map) => {
            let id = map
                .get("id")
                .or_else(|| map.get("value"))
                .or_else(|| map.get("key"))
                .and_then(as_string)?;

            let text = map
                .get("text")
                .or_else(|| map.get("label"))
                .or_else(|| map.get("name"))
                .or_else(|| map.get("descrizione"))
                .and_then(as_string)
                .unwrap_or_else(|| id.clone());

            Some(LookupItem { id, text })
        }
        Value::String(text) => Some(LookupItem {
            id: text.clone(),
            text,
        }),
        Value::Number(number) => {
            let text = number.to_string();
            Some(LookupItem {
                id: text.clone(),
                text,
            })
        }
        _ => None,
    }
}

fn assign_known_field_name(field_names: &mut BookingFieldNames, name: &str, id: Option<&str>) {
    let key = format!(
        "{} {}",
        name.to_ascii_lowercase(),
        id.unwrap_or_default().to_ascii_lowercase()
    );

    if field_names.employee.is_none() && key.contains("timbratura_dipendente_id") {
        field_names.employee = Some(name.to_owned());
    }
    if field_names.client.is_none()
        && key.contains("timbratura_cliente_id")
        && !key.contains("gruppo")
    {
        field_names.client = Some(name.to_owned());
    }
    if field_names.activity.is_none() && key.contains("timbratura_attivita_id") {
        field_names.activity = Some(name.to_owned());
    }
    if field_names.state.is_none()
        && key.contains("timbratura_stato")
        && !key.contains("stato_start")
        && !key.contains("stato_end")
    {
        field_names.state = Some(name.to_owned());
    }
    if field_names.entry_type.is_none() && key.contains("timbratura_tipo") {
        field_names.entry_type = Some(name.to_owned());
    }
    if field_names.start_date.is_none() && key.contains("timbratura_data_inizio") {
        field_names.start_date = Some(name.to_owned());
    }
    if field_names.end_date.is_none() && key.contains("timbratura_data_fine") {
        field_names.end_date = Some(name.to_owned());
    }
    if field_names.start_time.is_none() && key.contains("timbratura_ora_inizio") {
        field_names.start_time = Some(name.to_owned());
    }
    if field_names.end_time.is_none() && key.contains("timbratura_ora_fine") {
        field_names.end_time = Some(name.to_owned());
    }
    if field_names.total_hours.is_none() && key.contains("calcolo_ora") {
        field_names.total_hours = Some(name.to_owned());
    }
    if field_names.description.is_none() && key.contains("descrizione_attivita") {
        field_names.description = Some(name.to_owned());
    }
    if field_names.note.is_none() && key.contains("timbratura_note") {
        field_names.note = Some(name.to_owned());
    }
    if field_names.productivity.is_none() && key.contains("produttivita") {
        field_names.productivity = Some(name.to_owned());
    }
    if field_names.force.is_none() && name == "forza" {
        field_names.force = Some(name.to_owned());
    }
}

fn set_optional_field(fields: &mut BTreeMap<String, String>, key: Option<&String>, value: &str) {
    if let Some(key) = key {
        fields.insert(key.clone(), value.to_owned());
    }
}

fn compute_total_hours(
    start_date: &str,
    end_date: &str,
    start_time: &str,
    end_time: &str,
) -> Result<String> {
    let start_date = parse_date(start_date)?;
    let normalized_end_date = if end_date.trim().is_empty() {
        start_date.format("%d/%m/%Y").to_string()
    } else {
        end_date.trim().to_owned()
    };
    let end_date = parse_date(&normalized_end_date)?;
    let start_time = parse_time(start_time)?;
    let end_time = parse_time(end_time)?;

    let start = NaiveDateTime::new(start_date, start_time);
    let end = NaiveDateTime::new(end_date, end_time);
    if end < start {
        bail!("La data/ora di fine deve essere successiva a quella di inizio")
    }

    let duration = end - start;
    let minutes = duration.num_minutes();
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;

    Ok(format!("{:02}:{:02}", hours, remaining_minutes))
}

pub fn list_dates_inclusive(from: &str, to: &str) -> Result<Vec<String>> {
    let start = parse_date(from)?;
    let end = parse_date(to)?;

    if end < start {
        bail!("La data finale deve essere uguale o successiva a quella iniziale")
    }

    let mut dates = Vec::new();
    let mut current = start;
    while current <= end {
        dates.push(current.format("%d/%m/%Y").to_string());
        current = current
            .checked_add_days(Days::new(1))
            .context("failed to iterate booking dates")?;
    }

    Ok(dates)
}

fn parse_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%d/%m/%Y")
        .with_context(|| format!("invalid date '{}', expected DD/MM/YYYY", value.trim()))
}

fn parse_time(value: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value.trim(), "%H:%M")
        .with_context(|| format!("invalid time '{}', expected HH:MM", value.trim()))
}

fn absolutize_url(base: &Url, raw: &str) -> Result<Url> {
    base.join(raw.trim())
        .with_context(|| format!("failed to resolve URL '{}'", raw.trim()))
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static selector must be valid")
}
