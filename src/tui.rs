use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::bootstrap::run_bootstrap_install;
use crate::install_events;
use crate::types::{
    AdminUser, BootstrapOptions, BootstrapStorageOptions, ClusterMode, ExternalNfsOptions,
    InstallOptions, StorageModeOption, StorageOptions, MANAGED_NFS_NAMESPACE, MANAGED_NFS_RELEASE,
};
use crate::util::{command_exists, command_output, command_success, default_storage_class};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

const STEPS: [&str; 11] = [
    "Welcome", "Cluster", "Checks", "Identity", "Storage", "Access", "Admin", "Grafana", "Review",
    "Install", "Finish",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Welcome,
    Cluster,
    Checks,
    Identity,
    Storage,
    Access,
    Admin,
    Grafana,
    Review,
    Install,
    Finish,
}

struct CheckResult {
    label: &'static str,
    state: CheckState,
    detail: String,
}

enum CheckState {
    Ok,
    Warn,
    Fail,
}

struct BootstrapWizard {
    screen: Screen,
    cluster_selection: usize,
    storage_selection: usize,
    access_selection: usize,
    grafana_enabled: bool,
    active_field: usize,
    server_name: String,
    managed_size: String,
    storage_class: String,
    nfs_server: String,
    nfs_path: String,
    backend_port: String,
    admin_username: String,
    admin_email: String,
    admin_password: String,
    admin_password_confirm: String,
    checks: Vec<CheckResult>,
    error: Option<String>,
    context: Option<String>,
    contexts: Vec<String>,
    install_log: Vec<String>,
    install_rx: Option<Receiver<String>>,
    install_done: bool,
}

impl BootstrapWizard {
    fn new() -> Self {
        Self {
            screen: Screen::Welcome,
            cluster_selection: 0,
            storage_selection: 0,
            access_selection: 0,
            grafana_enabled: false,
            active_field: 0,
            server_name: "Kubarr".into(),
            managed_size: "1Ti".into(),
            storage_class: default_storage_class().unwrap_or_else(|| "cluster default".into()),
            nfs_server: String::new(),
            nfs_path: "/data".into(),
            backend_port: "30081".into(),
            admin_username: "admin".into(),
            admin_email: "admin@kubarr.local".into(),
            admin_password: String::new(),
            admin_password_confirm: String::new(),
            checks: Vec::new(),
            error: None,
            context: current_context(),
            contexts: available_contexts(),
            install_log: Vec::new(),
            install_rx: None,
            install_done: false,
        }
    }

    fn current_step(&self) -> usize {
        match self.screen {
            Screen::Welcome => 0,
            Screen::Cluster => 1,
            Screen::Checks => 2,
            Screen::Identity => 3,
            Screen::Storage => 4,
            Screen::Access => 5,
            Screen::Admin => 6,
            Screen::Grafana => 7,
            Screen::Review => 8,
            Screen::Install => 9,
            Screen::Finish => 10,
        }
    }

    fn next(&mut self) -> Option<Result<(), String>> {
        self.error = None;
        match self.screen {
            Screen::Welcome => self.screen = Screen::Cluster,
            Screen::Cluster => {
                self.run_checks();
                self.screen = Screen::Checks;
            }
            Screen::Checks => self.screen = Screen::Identity,
            Screen::Identity => {
                if self.server_name.trim().is_empty() {
                    self.error = Some("server name is required".into());
                } else {
                    self.screen = Screen::Storage;
                }
            }
            Screen::Storage => {
                if self.storage_selection == 1
                    && (self.nfs_server.trim().is_empty() || self.nfs_path.trim().is_empty())
                {
                    self.error = Some("external NFS requires server and export path".into());
                } else {
                    self.screen = Screen::Access;
                }
            }
            Screen::Access => {
                if self.access_selection == 0 && self.parse_backend_port().is_err() {
                    self.error = Some("NodePort must be between 30000 and 32767".into());
                } else {
                    self.screen = Screen::Admin;
                }
            }
            Screen::Admin => {
                if let Err(err) = self.validate_admin() {
                    self.error = Some(err);
                } else {
                    self.screen = Screen::Grafana;
                }
            }
            Screen::Grafana => self.screen = Screen::Review,
            Screen::Review => {
                if let Err(err) = self.start_install() {
                    self.error = Some(err);
                }
            }
            Screen::Install if self.install_done => self.screen = Screen::Finish,
            Screen::Install => {}
            Screen::Finish => return Some(Ok(())),
        }
        None
    }

    fn back(&mut self) {
        self.error = None;
        self.screen = match self.screen {
            Screen::Welcome => Screen::Welcome,
            Screen::Cluster => Screen::Welcome,
            Screen::Checks => Screen::Cluster,
            Screen::Identity => Screen::Checks,
            Screen::Storage => Screen::Identity,
            Screen::Access => Screen::Storage,
            Screen::Admin => Screen::Access,
            Screen::Grafana => Screen::Admin,
            Screen::Review => Screen::Grafana,
            Screen::Install => Screen::Install,
            Screen::Finish => Screen::Finish,
        };
        self.active_field = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Result<(), String>> {
        match key.code {
            KeyCode::Esc if self.screen == Screen::Install => {}
            KeyCode::Esc => return Some(Err("bootstrap cancelled".into())),
            KeyCode::Enter => return self.next(),
            KeyCode::Right => return self.next(),
            KeyCode::Left => self.back(),
            KeyCode::Backspace if self.editable_field_active() => self.pop_char(),
            KeyCode::Backspace => self.back(),
            KeyCode::Up => self.move_selection(false),
            KeyCode::Down | KeyCode::Tab => self.move_selection(true),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('r' | 'R') if self.screen == Screen::Checks => self.run_checks(),
            KeyCode::Char(value) if self.editable_field_active() => self.push_char(value),
            _ => {}
        }
        None
    }

    fn editable_field_active(&self) -> bool {
        match self.screen {
            Screen::Identity => true,
            Screen::Storage => self.active_field > 1,
            Screen::Access => self.access_selection == 0 && self.active_field == 1,
            Screen::Admin => true,
            _ => false,
        }
    }

    fn move_selection(&mut self, forward: bool) {
        match self.screen {
            Screen::Cluster => {
                self.cluster_selection = move_index(self.cluster_selection, 2, forward)
            }
            Screen::Storage => {
                self.active_field = move_index(self.active_field, 4, forward);
            }
            Screen::Access => {
                self.active_field = move_index(self.active_field, 2, forward);
            }
            Screen::Admin => self.active_field = move_index(self.active_field, 4, forward),
            Screen::Grafana => self.grafana_enabled = !self.grafana_enabled,
            _ => {}
        }
    }

    fn toggle(&mut self) {
        match self.screen {
            Screen::Cluster => self.cluster_selection = 1 - self.cluster_selection,
            Screen::Storage if self.active_field == 0 => self.storage_selection = 0,
            Screen::Storage if self.active_field == 1 => self.storage_selection = 1,
            Screen::Access if self.active_field == 0 => {
                self.access_selection = 1 - self.access_selection
            }
            Screen::Grafana => self.grafana_enabled = !self.grafana_enabled,
            _ => {}
        }
    }

    fn push_char(&mut self, value: char) {
        match self.screen {
            Screen::Identity => self.server_name.push(value),
            Screen::Storage if self.storage_selection == 0 && self.active_field == 2 => {
                self.managed_size.push(value)
            }
            Screen::Storage if self.storage_selection == 0 && self.active_field == 3 => {
                self.storage_class.push(value)
            }
            Screen::Storage if self.storage_selection == 1 && self.active_field == 2 => {
                self.nfs_server.push(value)
            }
            Screen::Storage if self.storage_selection == 1 && self.active_field == 3 => {
                self.nfs_path.push(value)
            }
            Screen::Access if self.access_selection == 0 && self.active_field == 1 => {
                self.backend_port.push(value)
            }
            Screen::Admin if self.active_field == 0 => self.admin_username.push(value),
            Screen::Admin if self.active_field == 1 => self.admin_email.push(value),
            Screen::Admin if self.active_field == 2 => self.admin_password.push(value),
            Screen::Admin if self.active_field == 3 => self.admin_password_confirm.push(value),
            _ => {}
        }
    }

    fn pop_char(&mut self) {
        match self.screen {
            Screen::Identity => {
                self.server_name.pop();
            }
            Screen::Storage if self.storage_selection == 0 && self.active_field == 2 => {
                self.managed_size.pop();
            }
            Screen::Storage if self.storage_selection == 0 && self.active_field == 3 => {
                self.storage_class.pop();
            }
            Screen::Storage if self.storage_selection == 1 && self.active_field == 2 => {
                self.nfs_server.pop();
            }
            Screen::Storage if self.storage_selection == 1 && self.active_field == 3 => {
                self.nfs_path.pop();
            }
            Screen::Access if self.access_selection == 0 && self.active_field == 1 => {
                self.backend_port.pop();
            }
            Screen::Admin if self.active_field == 0 => {
                self.admin_username.pop();
            }
            Screen::Admin if self.active_field == 1 => {
                self.admin_email.pop();
            }
            Screen::Admin if self.active_field == 2 => {
                self.admin_password.pop();
            }
            Screen::Admin if self.active_field == 3 => {
                self.admin_password_confirm.pop();
            }
            _ => {}
        }
    }

    fn run_checks(&mut self) {
        let mut checks = vec![check_tool("kubectl"), check_tool("helm")];
        if self.cluster_selection == 1 {
            checks.push(check_tool("curl"));
        }

        if self.cluster_selection == 0 {
            checks.push(if command_success("kubectl", &["cluster-info"]) {
                CheckResult {
                    label: "Kubernetes API",
                    state: CheckState::Ok,
                    detail: "reachable".into(),
                }
            } else {
                CheckResult {
                    label: "Kubernetes API",
                    state: CheckState::Fail,
                    detail: "not reachable from current context".into(),
                }
            });

            checks.push(
                match command_output("kubectl", &["config", "current-context"]) {
                    Some(context) => CheckResult {
                        label: "Context",
                        state: CheckState::Ok,
                        detail: context,
                    },
                    None => CheckResult {
                        label: "Context",
                        state: CheckState::Warn,
                        detail: "no current context detected".into(),
                    },
                },
            );

            checks.push(
                match command_output("kubectl", &["get", "nodes", "--no-headers"]) {
                    Some(nodes) => {
                        let ready = nodes
                            .lines()
                            .filter(|line| line.split_whitespace().nth(1) == Some("Ready"))
                            .count();
                        CheckResult {
                            label: "Ready nodes",
                            state: if ready > 0 {
                                CheckState::Ok
                            } else {
                                CheckState::Fail
                            },
                            detail: ready.to_string(),
                        }
                    }
                    None => CheckResult {
                        label: "Ready nodes",
                        state: CheckState::Warn,
                        detail: "could not query nodes".into(),
                    },
                },
            );
        }

        checks.push(match default_storage_class() {
            Some(class) => CheckResult {
                label: "StorageClass",
                state: CheckState::Ok,
                detail: class,
            },
            None => CheckResult {
                label: "StorageClass",
                state: CheckState::Warn,
                detail: "no default detected; choose one manually if needed".into(),
            },
        });

        self.checks = checks;
    }

    fn validate_admin(&self) -> Result<(), String> {
        if self.admin_username.trim().is_empty() {
            return Err("admin username is required".into());
        }
        if self.admin_email.trim().is_empty() {
            return Err("admin email is required".into());
        }
        if self.admin_password.len() < 8 {
            return Err("admin password must be at least 8 characters".into());
        }
        if self.admin_password != self.admin_password_confirm {
            return Err("admin passwords do not match".into());
        }
        Ok(())
    }

    fn parse_backend_port(&self) -> Result<u16, String> {
        let port = self
            .backend_port
            .parse::<u16>()
            .map_err(|_| "invalid backend NodePort".to_string())?;
        if (30000..=32767).contains(&port) {
            Ok(port)
        } else {
            Err("invalid backend NodePort".into())
        }
    }

    fn build_options(&self) -> Result<BootstrapOptions, String> {
        self.validate_admin()?;
        let mut install = InstallOptions::default_for_bootstrap(false);
        install.server_name = Some(self.server_name.trim().to_string());
        install.backend_node_port = if self.access_selection == 0 {
            Some(self.parse_backend_port()?)
        } else {
            None
        };
        install.admin = Some(AdminUser {
            username: self.admin_username.trim().to_string(),
            email: self.admin_email.trim().to_string(),
            password: self.admin_password.clone(),
        });

        let storage = if self.storage_selection == 0 {
            let storage_class = self.storage_class.trim();
            BootstrapStorageOptions {
                mode: StorageModeOption::ManagedNfs(StorageOptions {
                    namespace: MANAGED_NFS_NAMESPACE.into(),
                    release: MANAGED_NFS_RELEASE.into(),
                    size: self.managed_size.trim().to_string(),
                    storage_class: if storage_class.is_empty() || storage_class == "cluster default"
                    {
                        None
                    } else {
                        Some(storage_class.to_string())
                    },
                    wait: true,
                    dry_run: false,
                }),
            }
        } else {
            BootstrapStorageOptions {
                mode: StorageModeOption::ExternalNfs(ExternalNfsOptions {
                    server: self.nfs_server.trim().to_string(),
                    export_path: self.nfs_path.trim().to_string(),
                }),
            }
        };

        Ok(BootstrapOptions {
            cluster_mode: if self.cluster_selection == 0 {
                ClusterMode::Existing
            } else {
                ClusterMode::SingleNode
            },
            install,
            storage,
            skip_cluster_check: false,
            interactive: false,
            grafana_enabled: self.grafana_enabled,
        })
    }

    fn start_install(&mut self) -> Result<(), String> {
        let options = self.build_options()?;
        let (tx, rx) = mpsc::channel();
        self.install_rx = Some(rx);
        self.install_log.clear();
        self.install_done = false;
        self.screen = Screen::Install;

        thread::spawn(move || {
            install_events::set_install_log(tx.clone());
            run_bootstrap_install(&options);
            install_events::clear_install_log();
            let _ = tx.send("__KUBARR_INSTALL_DONE__".to_string());
        });

        Ok(())
    }

    fn drain_install_log(&mut self) {
        let Some(rx) = self.install_rx.as_ref() else {
            return;
        };
        while let Ok(message) = rx.try_recv() {
            if message == "__KUBARR_INSTALL_DONE__" {
                self.install_done = true;
                continue;
            }
            self.install_log.extend(message.lines().map(str::to_string));
            if self.install_log.len() > 200 {
                let extra = self.install_log.len() - 200;
                self.install_log.drain(0..extra);
            }
        }
    }
}

pub fn run_bootstrap_wizard() -> Result<(), String> {
    let mut terminal = start_terminal().map_err(|err| err.to_string())?;
    let mut app = BootstrapWizard::new();

    loop {
        app.drain_install_log();
        terminal
            .draw(|frame| render(frame, &app))
            .map_err(|err| err.to_string())?;

        if event::poll(Duration::from_millis(250)).map_err(|err| err.to_string())? {
            if let Event::Key(key) = event::read().map_err(|err| err.to_string())? {
                if let Some(result) = app.handle_key(key) {
                    stop_terminal(&mut terminal).map_err(|err| err.to_string())?;
                    return result;
                }
            }
        }
    }
}

fn start_terminal() -> io::Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn stop_terminal(terminal: &mut TuiTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn render(frame: &mut Frame, app: &BootstrapWizard) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(frame.area());
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26),
            Constraint::Min(42),
            Constraint::Length(32),
        ])
        .split(outer[0]);

    frame.render_widget(Clear, frame.area());
    render_steps(frame, app, columns[0]);
    render_main(frame, app, columns[1]);
    render_context(frame, app, columns[2]);
    render_action_bar(frame, app, outer[1]);
}

fn render_steps(frame: &mut Frame, app: &BootstrapWizard, area: Rect) {
    let current = app.current_step();
    let items = STEPS
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let status = if index < current {
                "done"
            } else if index == current {
                "here"
            } else {
                "next"
            };
            let style = if index == current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if index < current {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>2} ", index + 1), style),
                Span::styled(format!("{step:<9}"), style),
                Span::styled(status, style),
            ]))
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(
        Block::default()
            .title(" KUBARR BOOTSTRAP ")
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1))
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(list, area);
}

fn render_main(frame: &mut Frame, app: &BootstrapWizard, area: Rect) {
    let (title, lines) = match app.screen {
        Screen::Welcome => welcome_lines(),
        Screen::Cluster => cluster_lines(app),
        Screen::Checks => checks_lines(app),
        Screen::Identity => identity_lines(app),
        Screen::Storage => storage_lines(app),
        Screen::Access => access_lines(app),
        Screen::Admin => admin_lines(app),
        Screen::Grafana => grafana_lines(app),
        Screen::Review => review_lines(app),
        Screen::Install => install_lines(app),
        Screen::Finish => finish_lines(app),
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .padding(Padding::new(2, 2, 1, 1))
        .border_style(Style::default().fg(Color::Blue));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_context(frame: &mut Frame, app: &BootstrapWizard, area: Rect) {
    let mut lines = vec![
        line("Context", Color::Cyan, Modifier::BOLD),
        plain(app.context.as_deref().unwrap_or("not detected")),
        plain(""),
        line("Core Observability", Color::Cyan, Modifier::BOLD),
        plain("locked: Fluent Bit"),
        plain("locked: VictoriaLogs"),
        plain("locked: VictoriaMetrics"),
        plain(""),
        line("Current Plan", Color::Cyan, Modifier::BOLD),
        plain(if app.cluster_selection == 0 {
            "cluster: existing"
        } else {
            "cluster: single-node"
        }),
        plain(if app.storage_selection == 0 {
            "storage: managed NFS"
        } else {
            "storage: external NFS"
        }),
        plain(if app.access_selection == 0 {
            "access: NodePort"
        } else {
            "access: port-forward"
        }),
        plain(if app.grafana_enabled {
            "grafana: enabled"
        } else {
            "grafana: skipped"
        }),
    ];
    if let Some(error) = &app.error {
        lines.push(plain(""));
        lines.push(line("Validation", Color::Red, Modifier::BOLD));
        lines.push(plain(error));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Context ")
                .borders(Borders::ALL)
                .padding(Padding::new(1, 1, 1, 1)),
        ),
        area,
    );
}

fn render_action_bar(frame: &mut Frame, app: &BootstrapWizard, area: Rect) {
    let block = Block::default()
        .title(" Actions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(inner);

    for (chunk, action) in chunks.iter().zip(action_specs(app)) {
        let style = if action.enabled {
            Style::default()
                .fg(action.color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(action.label)
                .style(style)
                .alignment(Alignment::Center),
            *chunk,
        );
    }
}

fn welcome_lines() -> (&'static str, Vec<Line<'static>>) {
    (
        "Welcome",
        vec![
            line("Kubarr Bootstrap", Color::Cyan, Modifier::BOLD),
            plain(""),
            plain("This installer will prepare your target cluster and install Kubarr."),
            plain(""),
            muted("Setup phases"),
            plain("  1. Prepare"),
            plain("  2. Configure"),
            plain("  3. Review"),
            plain("  4. Install"),
            plain(""),
            plain("It will always install core logs and metrics:"),
            plain("  - Fluent Bit"),
            plain("  - VictoriaLogs"),
            plain("  - VictoriaMetrics"),
            plain(""),
            action("Use the action bar below to begin."),
        ],
    )
}

fn cluster_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let mut lines = vec![
        plain("Choose where Kubarr should run."),
        muted("Choose the cluster target for this installation."),
        plain(""),
        focused_choice(
            app.cluster_selection == 0,
            app.cluster_selection == 0,
            "Existing Kubernetes context",
        ),
        muted("    Use the active kubeconfig context."),
        plain(""),
        focused_choice(
            app.cluster_selection == 1,
            app.cluster_selection == 1,
            "Single-node local cluster",
        ),
        muted("    Install k3s locally, then install Kubarr."),
        plain(""),
        muted("Kubernetes contexts"),
        summary("current", app.context.as_deref().unwrap_or("not detected")),
    ];
    if app.contexts.is_empty() {
        lines.push(summary("available", "not detected"));
    } else {
        for context in &app.contexts {
            lines.push(plain(&format!("    {context}")));
        }
    }
    ("Cluster", lines)
}

fn checks_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let mut lines = vec![
        plain("Preflight checks for the selected target."),
        muted("Refresh reruns checks against the selected target."),
        plain(""),
    ];
    if app.checks.is_empty() {
        lines.push(plain("Use Refresh in the action bar to run checks again."));
    } else {
        lines.extend(app.checks.iter().map(check_line));
    }
    lines.push(plain(""));
    lines.push(plain(
        "Warnings can be reviewed later. Failures may stop installation.",
    ));
    ("Checks", lines)
}

fn identity_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    (
        "Server Identity",
        vec![
            plain("Name this Kubarr instance."),
            muted("This appears in the dashboard and install summary."),
            plain(""),
            field(true, "Server name", &app.server_name, false),
        ],
    )
}

fn storage_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let mut lines = vec![
        plain("Choose where apps keep media, configuration, and downloads."),
        muted("Move to an option, press Space to select it, then configure its fields."),
        plain(""),
        focused_choice(
            app.storage_selection == 0,
            app.active_field == 0,
            "Managed NFS",
        ),
        muted("    Kubarr deploys an in-cluster NFS service backed by a PVC."),
        plain(""),
        focused_choice(
            app.storage_selection == 1,
            app.active_field == 1,
            "External NFS",
        ),
        muted("    Use an existing NFS server/export."),
        plain(""),
    ];
    if app.storage_selection == 0 {
        lines.push(muted("Managed NFS settings"));
        lines.push(field(
            app.active_field == 2,
            "Size",
            &app.managed_size,
            false,
        ));
        lines.push(field(
            app.active_field == 3,
            "StorageClass",
            &app.storage_class,
            false,
        ));
    } else {
        lines.push(muted("External NFS settings"));
        lines.push(field(
            app.active_field == 2,
            "NFS server",
            &app.nfs_server,
            false,
        ));
        lines.push(field(
            app.active_field == 3,
            "Export path",
            &app.nfs_path,
            false,
        ));
    }
    ("Storage", lines)
}

fn access_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let mut lines = vec![
        plain("Choose how the dashboard should be reached."),
        muted("Focus Access mode to switch between NodePort and port-forward."),
        plain(""),
        focused_choice(
            app.access_selection == 0,
            app.active_field == 0 && app.access_selection == 0,
            "NodePort",
        ),
        muted("    Expose Kubarr on a Kubernetes NodePort."),
        focused_choice(
            app.access_selection == 1,
            app.active_field == 0 && app.access_selection == 1,
            "Port-forward only",
        ),
        muted("    Keep services internal and print a port-forward command."),
        plain(""),
    ];
    if app.access_selection == 0 {
        lines.push(field(
            app.active_field == 1,
            "Backend NodePort",
            &app.backend_port,
            false,
        ));
        lines.push(plain(""));
        lines.push(plain(&format!(
            "Preview: http://<node-ip>:{}",
            app.backend_port
        )));
    }
    ("Access", lines)
}

fn admin_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    (
        "Admin Account",
        vec![
            plain("Create the first dashboard administrator."),
            muted("Use Tab/Up/Down between fields. Passwords are hidden."),
            plain(""),
            field(
                app.active_field == 0,
                "Username",
                &app.admin_username,
                false,
            ),
            field(app.active_field == 1, "Email", &app.admin_email, false),
            field(app.active_field == 2, "Password", &app.admin_password, true),
            field(
                app.active_field == 3,
                "Confirm password",
                &app.admin_password_confirm,
                true,
            ),
            plain(""),
            plain("Password is never printed in review or install output."),
        ],
    )
}

fn grafana_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    (
        "Grafana",
        vec![
            plain("Core observability is always installed."),
            muted("Only Grafana is optional."),
            plain(""),
            plain("  locked  Fluent Bit log collector"),
            plain("  locked  VictoriaLogs log store"),
            plain("  locked  VictoriaMetrics metrics store"),
            plain(""),
            focused_choice(
                app.grafana_enabled,
                true,
                "Install Grafana dashboards and datasources",
            ),
            plain(""),
            action("Grafana can be changed before review."),
        ],
    )
}

fn review_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let storage = if app.storage_selection == 0 {
        format!("managed NFS, {}, {}", app.managed_size, app.storage_class)
    } else {
        format!("external NFS, {}:{}", app.nfs_server, app.nfs_path)
    };
    let access = if app.access_selection == 0 {
        format!("NodePort {}", app.backend_port)
    } else {
        "port-forward only".into()
    };
    (
        "Launch Plan",
        vec![
            plain("Review the final plan."),
            muted("This is the final review before installation starts."),
            plain(""),
            summary(
                "cluster",
                if app.cluster_selection == 0 {
                    "existing context"
                } else {
                    "single-node k3s"
                },
            ),
            summary("server name", &app.server_name),
            summary("storage", &storage),
            summary("access", &access),
            summary("admin", &app.admin_username),
            summary("admin email", &app.admin_email),
            summary(
                "observability",
                "Fluent Bit + VictoriaLogs + VictoriaMetrics",
            ),
            summary(
                "grafana",
                if app.grafana_enabled {
                    "enabled"
                } else {
                    "skipped"
                },
            ),
            plain(""),
            action("This is the last confirmation before cluster changes."),
        ],
    )
}

fn install_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let mut lines = vec![
        plain("Installing Kubarr."),
        muted("The wizard stays open while commands run."),
        plain(""),
    ];

    if app.install_log.is_empty() {
        lines.push(muted("Waiting for install output..."));
    } else {
        let start = app.install_log.len().saturating_sub(24);
        for item in &app.install_log[start..] {
            lines.push(plain(item));
        }
    }

    if app.install_done {
        lines.push(plain(""));
        lines.push(action("Installation complete. Continue to Finish."));
    }

    ("Install", lines)
}

fn finish_lines(app: &BootstrapWizard) -> (&'static str, Vec<Line<'static>>) {
    let access = if app.access_selection == 0 {
        format!("http://<node-ip>:{}", app.backend_port)
    } else {
        "kubectl port-forward -n kubarr-backend svc/kubarr-backend 8000:8000".into()
    };
    (
        "Finish",
        vec![
            line("Kubarr is ready", Color::Green, Modifier::BOLD),
            plain(""),
            summary("open", &access),
            summary("admin", &app.admin_username),
            summary(
                "grafana",
                if app.grafana_enabled {
                    "enabled"
                } else {
                    "skipped"
                },
            ),
            plain(""),
            muted("Useful commands"),
            plain("  kubarr doctor"),
            plain("  kubarr users list"),
            plain("  kubectl get pods -A"),
        ],
    )
}

fn check_tool(tool: &'static str) -> CheckResult {
    if command_exists(tool) {
        CheckResult {
            label: tool,
            state: CheckState::Ok,
            detail: "found".into(),
        }
    } else {
        CheckResult {
            label: tool,
            state: CheckState::Fail,
            detail: "missing from PATH".into(),
        }
    }
}

fn check_line(check: &CheckResult) -> Line<'static> {
    let (label, color) = match check.state {
        CheckState::Ok => ("[OK]", Color::Green),
        CheckState::Warn => ("[WARN]", Color::Yellow),
        CheckState::Fail => ("[FAIL]", Color::Red),
    };
    Line::from(vec![
        Span::styled(format!("  {label:<7}"), Style::default().fg(color)),
        Span::raw(format!("{:<18} {}", check.label, check.detail)),
    ])
}

fn focused_choice(selected: bool, focused: bool, label: &str) -> Line<'static> {
    let focus = if focused { ">" } else { " " };
    let marker_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Gray)
    };
    let label_style = if focused || selected {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let selected_label = if selected { " selected" } else { "" };
    Line::from(vec![
        Span::styled(format!("  {focus} "), marker_style),
        Span::styled(label.to_string(), label_style),
        Span::styled(selected_label.to_string(), Style::default().fg(Color::Cyan)),
    ])
}

fn field(active: bool, label: &str, value: &str, masked: bool) -> Line<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let value = if masked {
        "*".repeat(value.len())
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled(if active { "  > " } else { "    " }, style),
        Span::styled(format!("{:<18}", format!("{label}:")), style),
        Span::raw(value),
        Span::styled(if active { "_" } else { "" }, style),
    ])
}

fn summary(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {:<16}", format!("{label}:")),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(value.to_string()),
    ])
}

fn line(value: &str, color: Color, modifier: Modifier) -> Line<'static> {
    Line::from(Span::styled(
        value.to_string(),
        Style::default().fg(color).add_modifier(modifier),
    ))
}

fn plain(value: &str) -> Line<'static> {
    Line::from(value.to_string())
}

fn muted(value: &str) -> Line<'static> {
    Line::from(Span::styled(
        value.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

fn action(value: &str) -> Line<'static> {
    Line::from(Span::styled(
        value.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

struct ActionSpec {
    label: &'static str,
    color: Color,
    enabled: bool,
}

fn action_specs(app: &BootstrapWizard) -> [ActionSpec; 4] {
    let next_label = match app.screen {
        Screen::Welcome => "Enter/Right  Start",
        Screen::Review => "Enter/Right  Install",
        Screen::Install => "Enter/Right  Finish",
        Screen::Finish => "Enter/Right  Exit",
        _ => "Enter/Right  Next",
    };

    [
        ActionSpec {
            label: "Left  Previous",
            color: Color::Blue,
            enabled: !matches!(
                app.screen,
                Screen::Welcome | Screen::Install | Screen::Finish
            ),
        },
        ActionSpec {
            label: "R  Refresh",
            color: Color::Yellow,
            enabled: app.screen == Screen::Checks,
        },
        ActionSpec {
            label: next_label,
            color: Color::Green,
            enabled: app.screen != Screen::Install || app.install_done,
        },
        ActionSpec {
            label: "Esc  Quit",
            color: Color::Red,
            enabled: true,
        },
    ]
}

fn current_context() -> Option<String> {
    command_output("kubectl", &["config", "current-context"])
}

fn available_contexts() -> Vec<String> {
    command_output("kubectl", &["config", "get-contexts", "-o", "name"])
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn move_index(index: usize, len: usize, forward: bool) -> usize {
    if forward {
        (index + 1) % len
    } else if index == 0 {
        len - 1
    } else {
        index - 1
    }
}
