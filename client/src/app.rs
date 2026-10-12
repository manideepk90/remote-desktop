//! The application window: saved connections, the connection form, and the
//! remote session with its toolbar.

use egui::{Align, Key, Layout, RichText, Ui, ViewportCommand, vec2};

use crate::profiles::{Profile, Scale, Store};
use crate::rfb::{self, ConnectOptions, Credentials, Event, Session};
use crate::secrets;
use crate::viewer::{Action, Viewer};

const APP_NAME: &str = "Remote Desk Viewer";
const DEFAULT_PORT: u16 = 5900;

#[derive(Default)]
struct Form {
    /// The saved connection being edited, if any.
    id: Option<String>,
    name: String,
    address: String,
    username: String,
    password: String,
    show_password: bool,
    remember: bool,
    view_only: bool,
    scale: Scale,
    /// A password for `id` is in the credential store.
    password_saved: bool,
    focus_address: bool,
    focus_password: bool,
}

impl Form {
    fn new() -> Form {
        Form { remember: true, focus_address: true, ..Default::default() }
    }

    fn from_profile(p: &Profile) -> Form {
        Form {
            id: Some(p.id.clone()),
            name: p.name.clone(),
            address: p.address(),
            username: p.username.clone(),
            remember: p.remember_password,
            view_only: p.view_only,
            scale: p.scale,
            password_saved: p.password_saved,
            ..Default::default()
        }
    }
}

struct Notice {
    text: String,
    error: bool,
}

struct Prompt {
    need_username: bool,
    username: String,
    password: String,
    remember: bool,
    focus: bool,
}

enum Phase {
    Connecting(String),
    Connected,
}

struct Active {
    profile_id: String,
    title: String,
    session: Session,
    phase: Phase,
    prompt: Option<Prompt>,
    remember: bool,
    used_saved_password: bool,
    viewer: Viewer,
    /// Where the full-screen toolbar was drawn last frame; clicks there are not sent.
    toolbar_rect: Option<egui::Rect>,
}

pub struct App {
    store: Store,
    form: Form,
    notice: Option<Notice>,
    active: Option<Active>,
    confirm_delete: Option<String>,
    fullscreen: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> App {
        style(&cc.egui_ctx);
        let store = Store::load();
        let form = store.sorted().first().map_or_else(Form::new, |p| Form::from_profile(p));
        App { store, form, notice: None, active: None, confirm_delete: None, fullscreen: false }
    }

    fn error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: true });
    }

    fn info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: false });
    }

    fn save_store(&mut self) {
        if let Err(e) = self.store.save() {
            self.error(format!("Could not save connections: {e:#}"));
        }
    }

    // ---- actions -----------------------------------------------------------

    /// Turns the form into a saved profile (new or updated). Returns its id.
    fn save_form(&mut self) -> Option<String> {
        let (host, port) = match rfb::parse_address(&self.form.address, DEFAULT_PORT) {
            Ok(v) => v,
            Err(e) => {
                self.error(e);
                return None;
            }
        };
        let username = self.form.username.trim().to_string();
        let id = self
            .form
            .id
            .clone()
            .or_else(|| self.store.find_same(&host, port, &username).map(|p| p.id.clone()))
            .unwrap_or_else(Profile::new_id);
        let mut p = self.store.get(&id).cloned().unwrap_or_default();
        p.id = id.clone();
        p.name = self.form.name.trim().to_string();
        p.host = host;
        p.port = port;
        p.username = username;
        p.remember_password = self.form.remember;
        p.view_only = self.form.view_only;
        p.scale = self.form.scale;

        if !self.form.remember && p.password_saved {
            match secrets::delete(&id) {
                Ok(()) => p.password_saved = false,
                Err(e) => self.error(e),
            }
        } else if self.form.remember && !self.form.password.is_empty() {
            match secrets::save(&id, &self.form.password) {
                Ok(()) => p.password_saved = true,
                Err(e) => self.error(format!("Could not save the password: {e}")),
            }
        }
        self.form.id = Some(id.clone());
        self.form.password_saved = p.password_saved;
        self.store.upsert(p);
        self.save_store();
        Some(id)
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.notice = None;
        // A typed password is only stored once the server accepts it (see `Event::Authenticated`).
        let typed = std::mem::take(&mut self.form.password);
        let remember = self.form.remember;
        let Some(id) = self.save_form() else {
            self.form.password = typed;
            return;
        };
        let p = self.store.get(&id).cloned().expect("just saved");

        let mut used_saved_password = false;
        let password = if !typed.is_empty() {
            Some(typed)
        } else if p.password_saved {
            match secrets::load(&id) {
                Ok(Some(pw)) => {
                    used_saved_password = true;
                    Some(pw)
                }
                Ok(None) => {
                    if let Some(p) = self.store.get_mut(&id) {
                        p.password_saved = false;
                    }
                    self.form.password_saved = false;
                    None
                }
                Err(e) => {
                    self.error(format!("Could not read the saved password: {e}"));
                    None
                }
            }
        } else {
            None
        };

        let waker = ctx.clone();
        let session = Session::start(
            ConnectOptions { host: p.host.clone(), port: p.port, username: p.username.clone(), password, shared: true },
            move || waker.request_repaint(),
        );
        self.active = Some(Active {
            profile_id: id,
            title: p.title(),
            session,
            phase: Phase::Connecting(format!("Connecting to {}…", p.address())),
            prompt: None,
            remember,
            used_saved_password,
            viewer: Viewer::new(p.scale, p.view_only),
            toolbar_rect: None,
        });
    }

    fn disconnect(&mut self, ctx: &egui::Context) {
        if let Some(mut a) = self.active.take() {
            a.viewer.release_all(&a.session);
            a.session.disconnect();
            self.remember_view_settings(&a);
        }
        self.leave_session(ctx);
    }

    fn leave_session(&mut self, ctx: &egui::Context) {
        if self.fullscreen {
            self.fullscreen = false;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
        ctx.send_viewport_cmd(ViewportCommand::Title(APP_NAME.into()));
    }

    fn remember_view_settings(&mut self, a: &Active) {
        if let Some(p) = self.store.get_mut(&a.profile_id)
            && (p.scale != a.viewer.scale || p.view_only != a.viewer.view_only)
        {
            p.scale = a.viewer.scale;
            p.view_only = a.viewer.view_only;
            if self.form.id.as_deref() == Some(&a.profile_id) {
                self.form.scale = p.scale;
                self.form.view_only = p.view_only;
            }
            self.save_store();
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
    }

    fn delete_profile(&mut self, id: &str) {
        if let Some(p) = self.store.remove(id) {
            if p.password_saved
                && let Err(e) = secrets::delete(id)
            {
                self.error(format!("Removed the connection, but not its saved password: {e}"));
            }
            self.save_store();
            if self.form.id.as_deref() == Some(id) {
                self.form = Form::new();
            }
        }
    }

    fn forget_password(&mut self) {
        let Some(id) = self.form.id.clone() else { return };
        match secrets::delete(&id) {
            Ok(()) => {
                if let Some(p) = self.store.get_mut(&id) {
                    p.password_saved = false;
                }
                self.form.password_saved = false;
                self.save_store();
                self.info("The saved password was removed.");
            }
            Err(e) => self.error(e),
        }
    }

    /// Handles everything the network thread reported since the last frame.
    fn poll_session(&mut self, ctx: &egui::Context) {
        let Some(a) = &mut self.active else { return };
        let events: Vec<Event> = a.session.events().collect();
        for event in events {
            let Some(a) = &mut self.active else { return };
            match event {
                Event::Status(s) => a.phase = Phase::Connecting(s),
                Event::NeedCredentials { username } => {
                    let saved = self.store.get(&a.profile_id);
                    a.prompt = Some(Prompt {
                        need_username: username,
                        username: saved.map(|p| p.username.clone()).unwrap_or_default(),
                        password: String::new(),
                        remember: a.remember,
                        focus: true,
                    });
                }
                Event::Authenticated { username, password } => {
                    let (id, remember, used_saved) = (a.profile_id.clone(), a.remember, a.used_saved_password);
                    let mut failed = None;
                    if let Some(p) = self.store.get_mut(&id) {
                        if !username.is_empty() {
                            p.username = username;
                        }
                        p.remember_password = remember;
                        match password {
                            Some(pw) if remember && !used_saved => match secrets::save(&id, &pw) {
                                Ok(()) => p.password_saved = true,
                                Err(e) => failed = Some(format!("Connected, but the password could not be saved: {e}")),
                            },
                            _ if !remember && p.password_saved => match secrets::delete(&id) {
                                Ok(()) => p.password_saved = false,
                                Err(e) => failed = Some(e),
                            },
                            _ => {}
                        }
                        p.touch();
                        if self.form.id.as_deref() == Some(&id) {
                            self.form.username = p.username.clone();
                            self.form.password_saved = p.password_saved;
                            self.form.remember = remember;
                        }
                    }
                    self.save_store();
                    if let Some(e) = failed {
                        self.error(e);
                    }
                }
                Event::Connected { name } => {
                    a.phase = Phase::Connected;
                    if !name.trim().is_empty() {
                        a.title = name.trim().to_string();
                    }
                    ctx.send_viewport_cmd(ViewportCommand::Title(format!("{} — {APP_NAME}", a.title)));
                }
                Event::Clipboard(text) => ctx.copy_text(text),
                Event::Bell => {}
                Event::Closed { error, auth_failed } => {
                    let a = self.active.take().expect("active session");
                    self.remember_view_settings(&a);
                    self.leave_session(ctx);
                    if let Some(e) = error {
                        let hint = if auth_failed && a.used_saved_password {
                            " The saved password was not accepted; enter it again."
                        } else {
                            ""
                        };
                        self.error(format!("{e}{hint}"));
                        if auth_failed {
                            self.form.focus_password = true;
                        }
                    }
                    return;
                }
            }
        }
    }

    // ---- home screen -------------------------------------------------------

    fn home(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::left("connections")
            .resizable(false)
            .exact_size(270.0)
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::same(14)))
            .show(ui, |ui| self.connection_list(ui, &ctx));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(460.0);
                    ui.with_layout(Layout::top_down(Align::Min), |ui| self.form_ui(ui, &ctx));
                });
            });
        });
        self.delete_dialog(&ctx);
    }

    fn connection_list(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.heading("Connections");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("+ New").on_hover_text("Start a new connection").clicked() {
                    self.form = Form::new();
                    self.notice = None;
                }
            });
        });
        ui.add_space(8.0);
        if self.store.profiles.is_empty() {
            ui.label(RichText::new("Saved connections appear here after you connect.").weak());
            return;
        }
        let profiles: Vec<Profile> = self.store.sorted().into_iter().cloned().collect();
        let mut open = None;
        let mut connect = None;
        let mut delete = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for p in &profiles {
                let selected = self.form.id.as_deref() == Some(p.id.as_str());
                let r = connection_card(ui, p, selected);
                if r.clicked() {
                    open = Some(p.id.clone());
                }
                if r.double_clicked() {
                    connect = Some(p.id.clone());
                }
                r.context_menu(|ui| {
                    if ui.button("Connect").clicked() {
                        connect = Some(p.id.clone());
                    }
                    if ui.button("Edit").clicked() {
                        open = Some(p.id.clone());
                    }
                    ui.separator();
                    if ui.button("Delete…").clicked() {
                        delete = Some(p.id.clone());
                    }
                });
                ui.add_space(4.0);
            }
        });
        if let Some(id) = open.or(connect.clone())
            && let Some(p) = self.store.get(&id)
        {
            self.form = Form::from_profile(p);
            self.notice = None;
        }
        if connect.is_some() {
            self.connect(ctx);
        }
        if delete.is_some() {
            self.confirm_delete = delete;
        }
    }

    fn form_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let editing = self.form.id.as_ref().and_then(|id| self.store.get(id)).map(|p| p.title());
        ui.heading(RichText::new(editing.as_deref().unwrap_or("New connection")).size(22.0));
        ui.label(
            RichText::new("Connect to a VNC server such as Remote Desk, TigerVNC or macOS Screen Sharing.").weak(),
        );
        ui.add_space(12.0);

        if let Some(n) = &self.notice {
            let (fill, text) = if n.error {
                (ui.visuals().error_fg_color.gamma_multiply(0.15), ui.visuals().error_fg_color)
            } else {
                (ui.visuals().selection.bg_fill.gamma_multiply(0.25), ui.visuals().text_color())
            };
            egui::Frame::new().fill(fill).corner_radius(6).inner_margin(10).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&n.text).color(text));
            });
            ui.add_space(10.0);
        }

        let mut submit = false;
        let field_width = ui.available_width();
        let label = |ui: &mut Ui, text: &str| {
            ui.add_space(6.0);
            ui.label(RichText::new(text).strong());
        };

        label(ui, "IP address or host name");
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.form.address)
                .hint_text("192.168.1.20   or   my-pc:5901")
                .desired_width(field_width),
        );
        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if std::mem::take(&mut self.form.focus_address) {
            r.request_focus();
        }

        label(ui, "Username");
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.form.username)
                .hint_text("Optional: only for account logins such as macOS")
                .desired_width(field_width),
        );
        submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));

        label(ui, "Password");
        ui.horizontal(|ui| {
            let hint = if self.form.password_saved {
                format!("Saved in {}", secrets::store_name())
            } else {
                "Asked for when needed if left empty".into()
            };
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.form.password)
                    .password(!self.form.show_password)
                    .hint_text(hint)
                    .desired_width(field_width - 40.0),
            );
            if std::mem::take(&mut self.form.focus_password) {
                r.request_focus();
            }
            submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let eye = if self.form.show_password { "🙈" } else { "👁" };
            if ui
                .add_sized([32.0, r.rect.height()], egui::Button::new(eye))
                .on_hover_text(if self.form.show_password { "Hide password" } else { "Show password" })
                .clicked()
            {
                self.form.show_password = !self.form.show_password;
            }
        });

        ui.add_space(6.0);
        ui.checkbox(&mut self.form.remember, format!("Remember password in {}", secrets::store_name())).on_hover_text(
            "Stored by the operating system, encrypted for your user account. Never written to a file by this app.",
        );
        if self.form.password_saved && ui.small_button("Forget saved password").clicked() {
            self.forget_password();
        }

        egui::CollapsingHeader::new("More options")
            .default_open(self.form.name.is_empty() && self.form.id.is_some())
            .show(ui, |ui| {
                label(ui, "Display name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.name)
                        .hint_text("e.g. Office PC")
                        .desired_width(field_width - 20.0),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.form.view_only, "View only (don't send keyboard or mouse)");
                ui.horizontal(|ui| {
                    ui.label("Scaling:");
                    ui.selectable_value(&mut self.form.scale, Scale::Fit, "Fit to window");
                    ui.selectable_value(&mut self.form.scale, Scale::Actual, "Actual size");
                });
            });

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            let connect = egui::Button::new(RichText::new("Connect").strong().size(15.0))
                .fill(ui.visuals().selection.bg_fill)
                .min_size(vec2(120.0, 34.0));
            if ui.add_enabled(!self.form.address.trim().is_empty(), connect).clicked() {
                submit = true;
            }
            if ui.add(egui::Button::new("Save").min_size(vec2(80.0, 34.0))).clicked() {
                self.notice = None;
                if self.save_form().is_some() {
                    self.form.password.clear();
                    self.info("Connection saved.");
                }
            }
            if let Some(id) = self.form.id.clone()
                && ui.add(egui::Button::new("Delete").min_size(vec2(80.0, 34.0))).clicked()
            {
                self.confirm_delete = Some(id);
            }
        });
        if submit && !self.form.address.trim().is_empty() {
            self.connect(ctx);
        }

        ui.add_space(22.0);
        ui.label(
            RichText::new(
                "VNC is not encrypted. Connect over a VPN such as Tailscale or an SSH tunnel when you are not on a trusted network.",
            )
            .small()
            .weak(),
        );
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else { return };
        let Some(p) = self.store.get(&id).cloned() else {
            self.confirm_delete = None;
            return;
        };
        let mut close = false;
        let mut delete = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-delete")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading(format!("Delete \"{}\"?", p.title()));
            ui.add_space(6.0);
            ui.label(if p.password_saved {
                "The connection and its saved password will be removed."
            } else {
                "The connection will be removed."
            });
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Delete").clicked() {
                    delete = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if delete {
            self.delete_profile(&id);
        }
        if delete || close || modal.should_close() {
            self.confirm_delete = None;
        }
    }

    // ---- session screens ---------------------------------------------------

    fn connecting(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut cancel = false;
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(a) = &self.active else { return };
            let status = match &a.phase {
                Phase::Connecting(s) => s.clone(),
                Phase::Connected => String::new(),
            };
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
                ui.add(egui::Spinner::new().size(36.0));
                ui.add_space(14.0);
                ui.heading(&a.title);
                ui.add_space(4.0);
                ui.label(RichText::new(status).weak());
                ui.add_space(18.0);
                if ui.add(egui::Button::new("Cancel").min_size(vec2(100.0, 30.0))).clicked() {
                    cancel = true;
                }
            });
        });
        self.credentials_dialog(&ctx);
        if cancel {
            self.disconnect(&ctx);
        }
    }

    fn credentials_dialog(&mut self, ctx: &egui::Context) {
        let Some(a) = &mut self.active else { return };
        let title = a.title.clone();
        let Some(p) = &mut a.prompt else { return };
        let mut answer: Option<Option<Credentials>> = None;
        let modal = egui::Modal::new(egui::Id::new("credentials")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Authentication required");
            ui.label(
                RichText::new(format!(
                    "{title} needs {}.",
                    if p.need_username { "a username and password" } else { "a password" }
                ))
                .weak(),
            );
            ui.add_space(10.0);
            let mut enter = false;
            if p.need_username {
                ui.label("Username");
                let r = ui.add(egui::TextEdit::singleline(&mut p.username).desired_width(f32::INFINITY));
                if p.focus && p.username.is_empty() {
                    r.request_focus();
                    p.focus = false;
                }
                enter |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(6.0);
            }
            ui.label("Password");
            let r = ui.add(egui::TextEdit::singleline(&mut p.password).password(true).desired_width(f32::INFINITY));
            if std::mem::take(&mut p.focus) {
                r.request_focus();
            }
            enter |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(6.0);
            ui.checkbox(&mut p.remember, format!("Remember in {}", secrets::store_name()));
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let ok =
                    ui.add_enabled(!p.need_username || !p.username.trim().is_empty(), egui::Button::new("Sign in"));
                if ok.clicked() || (enter && (!p.need_username || !p.username.trim().is_empty())) {
                    answer = Some(Some(Credentials {
                        username: p.username.trim().to_string(),
                        password: p.password.clone(),
                    }));
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(None);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(None);
        }
        if let Some(answer) = answer {
            let p = a.prompt.take().expect("prompt");
            a.remember = p.remember;
            a.used_saved_password = false;
            a.session.answer_credentials(answer);
        }
    }

    fn session_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut disconnect = false;
        let mut fullscreen = false;
        let in_fullscreen = self.fullscreen;
        let Some(a) = &mut self.active else { return };

        if in_fullscreen {
            // Toolbar slides in when the pointer touches the top edge.
            let hover = ctx.pointer_hover_pos();
            let show = hover.is_some_and(|p| p.y <= 3.0 || a.toolbar_rect.is_some_and(|r| r.expand(16.0).contains(p)));
            egui::CentralPanel::no_frame().show(ui, |ui| a.viewer.show(ui, &a.session));
            if show {
                let area = egui::Area::new(egui::Id::new("fullscreen-toolbar"))
                    .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 6.0))
                    .order(egui::Order::Foreground)
                    .show(&ctx, |ui| {
                        egui::Frame::popup(ui.style()).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                            ui.horizontal(|ui| toolbar(ui, a, true, &mut fullscreen, &mut disconnect));
                        });
                    });
                a.toolbar_rect = Some(area.response.rect);
            } else {
                a.toolbar_rect = None;
            }
        } else {
            a.toolbar_rect = None;
            egui::Panel::top("toolbar")
                .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 5)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&a.title).strong());
                        let (w, h) = {
                            let fb = a.session.fb.lock().unwrap();
                            (fb.width, fb.height)
                        };
                        ui.label(RichText::new(format!("{w}×{h}")).weak());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                toolbar(ui, a, false, &mut fullscreen, &mut disconnect)
                            });
                        });
                    });
                });
            egui::CentralPanel::no_frame().show(ui, |ui| a.viewer.show(ui, &a.session));
        }
        if !a.viewer.has_image() {
            ctx.request_repaint();
        }
        if fullscreen {
            self.toggle_fullscreen(&ctx);
        }
        if disconnect {
            self.disconnect(&ctx);
        }
    }
}

fn toolbar(ui: &mut Ui, a: &mut Active, in_fullscreen: bool, fullscreen: &mut bool, disconnect: &mut bool) {
    if in_fullscreen {
        ui.label(RichText::new(&a.title).strong());
        ui.separator();
    }
    let fit = a.viewer.scale == Scale::Fit;
    if ui.selectable_label(fit, "Fit").on_hover_text("Scale the remote screen to the window").clicked() {
        a.viewer.scale = Scale::Fit;
    }
    if ui.selectable_label(!fit, "1:1").on_hover_text("Show the remote screen at its actual size").clicked() {
        a.viewer.scale = Scale::Actual;
    }
    ui.separator();
    let view_only = a.viewer.view_only;
    if ui.selectable_label(view_only, "View only").on_hover_text("Stop sending keyboard and mouse input").clicked() {
        a.viewer.release_all(&a.session);
        a.viewer.view_only = !view_only;
    }
    if ui
        .add_enabled(!view_only, egui::Button::new("Ctrl+Alt+Del"))
        .on_hover_text("Send Ctrl+Alt+Delete to the remote computer")
        .clicked()
    {
        a.viewer.send_ctrl_alt_del(&a.session);
    }
    ui.separator();
    let label = if in_fullscreen { "Exit full screen" } else { "Full screen" };
    if ui.button(label).on_hover_text("Ctrl+Alt+Enter").clicked() {
        *fullscreen = true;
    }
    if ui.button(RichText::new("Disconnect").color(ui.visuals().error_fg_color)).clicked() {
        *disconnect = true;
    }
}

fn connection_card(ui: &mut Ui, p: &Profile, selected: bool) -> egui::Response {
    let visuals = ui.visuals().clone();
    let fill = if selected { visuals.selection.bg_fill.gamma_multiply(0.35) } else { visuals.faint_bg_color };
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 9))
        .stroke(if selected { visuals.selection.stroke } else { egui::Stroke::NONE });
    let inner = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(p.title()).strong());
        let mut sub = Vec::new();
        if !p.username.is_empty() {
            sub.push(format!("{}@{}", p.username, p.address()));
        } else if !p.name.trim().is_empty() {
            sub.push(p.address());
        }
        if p.last_used > 0 {
            sub.push(ago(p.last_used));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(sub.join("  ·  ")).small().weak());
            if p.password_saved {
                ui.label(RichText::new("🔑").small())
                    .on_hover_text(format!("Password saved in {}", secrets::store_name()));
            }
        });
    });
    let r = ui.interact(inner.response.rect, ui.id().with(("card", &p.id)), egui::Sense::click());
    if r.hovered() && !selected {
        ui.painter().rect_stroke(
            inner.response.rect,
            8,
            egui::Stroke::new(1.0, visuals.widgets.hovered.bg_stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    r.on_hover_text("Click to edit, double-click to connect")
}

/// "just now", "5 min ago", "3 days ago".
fn ago(unix: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let s = now.saturating_sub(unix);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", s / 86400),
    }
}

fn style(ctx: &egui::Context) {
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.visuals.widgets.inactive.corner_radius = 6.into();
        s.visuals.widgets.hovered.corner_radius = 6.into();
        s.visuals.widgets.active.corner_radius = 6.into();
        s.visuals.widgets.noninteractive.corner_radius = 6.into();
    });
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_session(ctx);
        // Follow full screen changes made by the window manager.
        if let Some(fs) = ctx.input(|i| i.viewport().fullscreen)
            && self.active.is_some()
        {
            self.fullscreen = fs;
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        match self.active.as_ref().map(|a| &a.phase) {
            None => self.home(ui),
            Some(Phase::Connecting(_)) => self.connecting(ui),
            Some(Phase::Connected) => self.session_ui(ui),
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let Some(a) = &mut self.active else { return };
        if !matches!(a.phase, Phase::Connected) {
            return;
        }
        let actions = a.viewer.handle_input(raw, &a.session, a.toolbar_rect);
        for action in actions {
            match action {
                Action::ToggleFullscreen => self.toggle_fullscreen(ctx),
            }
        }
    }

    fn on_exit(&mut self) {
        if let Some(mut a) = self.active.take() {
            a.viewer.release_all(&a.session);
            a.session.disconnect();
        }
    }
}
