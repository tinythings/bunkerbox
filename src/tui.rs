use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget, Wrap};

use ratatui::Terminal;

use crate::cfg::RemoteToolSpec;
use crate::remote_target::{
    validate_remote_artifact_paths, validate_remote_environment_names, validate_remote_exclusion_entries, validate_remote_resource_overrides,
    validate_remote_target_label, validate_remote_tool_spec, validate_ssh_destination, validate_workspace_path, ActiveBuildTarget,
    BuildTargetCatalog, RemoteConfigDraft, RemoteOverlayDraft, RemoteProjectOverlayDraft, RemoteQuantity, RemoteResourceOverridesDraft,
    RemoteTargetDraft,
};
use crate::vscomm::{self, parse_triggers, Trigger};

mod palette;
mod popup;
use popup::PopupWidget;

static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

const WIDGET_PROGRESS: &str = "progress";
const WIDGET_STATUS: &str = "status";
const WIDGET_POPUP: &str = "popup";
const WIDGET_SPINNER: &str = "spinner";
const WIDGET_PASSWORD: &str = "password";
const WIDGET_ERROR: &str = "error";

const CMD_SHOW: &str = "show";
const CMD_HIDE: &str = "hide";
const CMD_SET: &str = "set";
const CMD_CLEAR: &str = "clear";

const ERROR_TOAST_IN: Duration = Duration::from_millis(220);
const ERROR_TOAST_HOLD: Duration = Duration::from_secs(5);
const ERROR_TOAST_OUT: Duration = Duration::from_millis(260);

struct ErrorToast {
    title: String,
    message: String,
    shown_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseTracking {
    Off,
    Normal,
    Button,
    Any,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseEncoding {
    X10,
    Urxvt,
    Sgr,
}

pub struct PendingAction {
    pub widget: String,
    pub command: String,
    pub triggers: Vec<Trigger>,
    pub value: String,
    pub enqueued_at: Instant,
    pub first_pty_at: Option<Instant>,
}

pub struct OverlayState {
    pub status_text: String,
    pub popup: PopupWidget,
    pub popup_title: Option<String>,
    pub pending: Vec<PendingAction>,
    pub has_error: bool,
    error_toast: Option<ErrorToast>,
    pub hide_on_ascii: bool,
    pub hide_on_content: Option<String>,
    pub last_content_scan: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostPopup {
    None,
    Targets,
    Help,
    Setup,
    ConfigError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupScreen {
    List,
    TargetForm,
    Overrides,
    Tools,
    ToolForm,
    Environment,
    Exclusions,
    Artifacts,
    Resources,
    AdvancedResources,
    ConfirmDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetFormFocus {
    Label,
    Ssh,
    Workspace,
    Overrides,
    Resources,
    Save,
    Cancel,
}

impl TargetFormFocus {
    fn next(self, reverse: bool) -> Self {
        let fields = [Self::Label, Self::Ssh, Self::Workspace, Self::Overrides, Self::Resources, Self::Save, Self::Cancel];
        let index = fields.iter().position(|field| *field == self).unwrap_or(0);
        let next = if reverse { (index + fields.len() - 1) % fields.len() } else { (index + 1) % fields.len() };
        fields[next]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverrideFocus {
    Tools,
    Environment,
    Exclusions,
    Artifacts,
    Done,
}

impl OverrideFocus {
    fn next(self, reverse: bool) -> Self {
        let fields = [Self::Tools, Self::Environment, Self::Exclusions, Self::Artifacts, Self::Done];
        let index = fields.iter().position(|field| *field == self).unwrap_or(0);
        let next = if reverse { (index + fields.len() - 1) % fields.len() } else { (index + 1) % fields.len() };
        fields[next]
    }
}

#[derive(Clone, Debug)]
struct TextField {
    value: String,
    cursor: usize,
}

impl TextField {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
            return false;
        }
        match key.code {
            KeyCode::Char(character) => {
                self.insert(character);
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.byte_index(self.cursor - 1);
                    let end = self.byte_index(self.cursor);
                    self.value.replace_range(start..end, "");
                    self.cursor -= 1;
                }
                true
            }
            KeyCode::Delete => {
                if self.cursor < self.value.chars().count() {
                    let start = self.byte_index(self.cursor);
                    let end = self.byte_index(self.cursor + 1);
                    self.value.replace_range(start..end, "");
                }
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                true
            }
            _ => false,
        }
    }

    fn insert(&mut self, character: char) {
        let index = self.byte_index(self.cursor);
        self.value.insert(index, character);
        self.cursor += 1;
    }

    fn byte_index(&self, character_index: usize) -> usize {
        self.value.char_indices().nth(character_index).map_or(self.value.len(), |(index, _)| index)
    }
}

#[derive(Clone, Debug)]
struct TargetFormState {
    original_label: Option<String>,
    draft: RemoteTargetDraft,
    label: TextField,
    ssh: TextField,
    workspace: TextField,
    focus: TargetFormFocus,
}

impl TargetFormState {
    fn new(original_label: Option<String>, draft: RemoteTargetDraft) -> Self {
        Self {
            original_label: original_label.clone(),
            label: TextField::new(original_label.clone().unwrap_or_default()),
            ssh: TextField::new(draft.ssh.clone()),
            workspace: TextField::new(draft.workspace.clone()),
            draft,
            focus: TargetFormFocus::Label,
        }
    }

    fn candidate(&self) -> RemoteTargetDraft {
        let mut draft = self.draft.clone();
        draft.ssh = self.ssh.value.clone();
        draft.workspace = self.workspace.value.clone();
        draft
    }
}

#[derive(Clone, Debug)]
struct ToolFormState {
    original_index: Option<usize>,
    name: TextField,
    command: TextField,
    allow_args: bool,
    focus: usize,
}

impl ToolFormState {
    fn new(original_index: Option<usize>, tool: Option<&RemoteToolSpec>) -> Self {
        Self {
            original_index,
            name: TextField::new(tool.map_or("", |tool| tool.name.as_str())),
            command: TextField::new(tool.and_then(|tool| tool.command.as_deref()).unwrap_or_default()),
            allow_args: tool.is_some_and(|tool| tool.allow_args),
            focus: 0,
        }
    }

    fn next_focus(&mut self, reverse: bool) {
        self.focus = if reverse { (self.focus + 4) % 5 } else { (self.focus + 1) % 5 };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StringListKind {
    Environment,
    Exclusions,
    Artifacts,
}

impl StringListKind {
    fn title(self) -> &'static str {
        match self {
            Self::Environment => "Remote Environment Names",
            Self::Exclusions => "Remote Exclusions",
            Self::Artifacts => "Remote Artifacts",
        }
    }
}

#[derive(Clone, Debug)]
struct StringListState {
    kind: StringListKind,
    entries: Vec<String>,
    selected: usize,
    changed: bool,
    editing: Option<TextField>,
    editing_index: Option<usize>,
    error: Option<String>,
}

impl StringListState {
    fn new(kind: StringListKind, entries: Option<Vec<String>>) -> Self {
        Self { kind, entries: entries.unwrap_or_default(), selected: 0, changed: false, editing: None, editing_index: None, error: None }
    }
}

#[derive(Clone, Debug)]
struct ToolListState {
    entries: Vec<RemoteToolSpec>,
    selected: usize,
    changed: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceFieldKind {
    Quantity,
    Count,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResourceFieldKey {
    ConnectTimeout,
    SyncTimeout,
    BuildTimeout,
    MaxOutput,
    IdleOutputTimeout,
    CleanupTimeout,
    ArtifactTimeout,
    MaxArtifactBytes,
    MaxArtifactTotalBytes,
    MaxArtifactEntries,
    MaxWorkerUploads,
    MaxWorkerUploadBytes,
    MaxWorkerJobs,
    MaxWorkerJobBytes,
    MaxWorkerArtifactSpools,
    MaxWorkerArtifactSpoolBytes,
    MaxWorkerStateEntries,
    MaxActiveBuilds,
}

#[derive(Clone, Debug)]
struct ResourceInput {
    key: ResourceFieldKey,
    label: &'static str,
    kind: ResourceFieldKind,
    value: TextField,
}

#[derive(Clone, Debug)]
struct ResourceFormState {
    inputs: Vec<ResourceInput>,
    use_defaults: bool,
    focus: usize,
    advanced: bool,
}

impl ResourceFormState {
    fn new(resources: Option<&RemoteResourceOverridesDraft>) -> Self {
        let resources = resources.cloned().unwrap_or_default();
        let inputs = vec![
            resource_input(
                ResourceFieldKey::ConnectTimeout,
                "Connect timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.connect_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::SyncTimeout,
                "Sync timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.sync_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::BuildTimeout,
                "Build timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.build_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxOutput,
                "Max output bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_output.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::IdleOutputTimeout,
                "Idle output timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.idle_output_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::CleanupTimeout,
                "Cleanup timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.cleanup_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::ArtifactTimeout,
                "Artifact timeout",
                ResourceFieldKind::Quantity,
                quantity_text(resources.artifact_timeout.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxArtifactBytes,
                "Max artifact bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_artifact_bytes.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxArtifactTotalBytes,
                "Max artifact total bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_artifact_total_bytes.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxArtifactEntries,
                "Max artifact entries",
                ResourceFieldKind::Count,
                count_text(resources.max_artifact_entries),
            ),
            resource_input(
                ResourceFieldKey::MaxWorkerUploads,
                "Max worker uploads",
                ResourceFieldKind::Count,
                count_text(resources.max_worker_uploads),
            ),
            resource_input(
                ResourceFieldKey::MaxWorkerUploadBytes,
                "Max worker upload bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_worker_upload_bytes.as_ref()),
            ),
            resource_input(ResourceFieldKey::MaxWorkerJobs, "Max worker jobs", ResourceFieldKind::Count, count_text(resources.max_worker_jobs)),
            resource_input(
                ResourceFieldKey::MaxWorkerJobBytes,
                "Max worker job bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_worker_job_bytes.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxWorkerArtifactSpools,
                "Max worker artifact spools",
                ResourceFieldKind::Count,
                count_text(resources.max_worker_artifact_spools),
            ),
            resource_input(
                ResourceFieldKey::MaxWorkerArtifactSpoolBytes,
                "Max worker artifact spool bytes",
                ResourceFieldKind::Quantity,
                quantity_text(resources.max_worker_artifact_spool_bytes.as_ref()),
            ),
            resource_input(
                ResourceFieldKey::MaxWorkerStateEntries,
                "Max worker state entries",
                ResourceFieldKind::Count,
                count_text(resources.max_worker_state_entries),
            ),
            resource_input(ResourceFieldKey::MaxActiveBuilds, "Max active builds", ResourceFieldKind::Count, count_text(resources.max_active_builds)),
        ];
        Self { inputs, use_defaults: false, focus: 0, advanced: false }
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.inputs
            .iter()
            .enumerate()
            .filter_map(|(index, input)| {
                let advanced = matches!(
                    input.key,
                    ResourceFieldKey::ArtifactTimeout
                        | ResourceFieldKey::MaxArtifactBytes
                        | ResourceFieldKey::MaxArtifactTotalBytes
                        | ResourceFieldKey::MaxArtifactEntries
                        | ResourceFieldKey::MaxWorkerUploads
                        | ResourceFieldKey::MaxWorkerUploadBytes
                        | ResourceFieldKey::MaxWorkerJobs
                        | ResourceFieldKey::MaxWorkerJobBytes
                        | ResourceFieldKey::MaxWorkerArtifactSpools
                        | ResourceFieldKey::MaxWorkerArtifactSpoolBytes
                        | ResourceFieldKey::MaxWorkerStateEntries
                );
                (advanced == self.advanced).then_some(index)
            })
            .collect()
    }

    fn action_index(&self, action: usize) -> usize {
        self.visible_indices().len() + action
    }

    fn draft(&self) -> Result<Option<RemoteResourceOverridesDraft>, String> {
        let mut resources = RemoteResourceOverridesDraft::default();
        for input in &self.inputs {
            let value = input.value.value.trim();
            if value.is_empty() {
                continue;
            }
            let _field_kind = input.kind;
            let count = || value.parse::<u64>().map_err(|_| format!("{} must be an integer", input.label));
            match input.key {
                ResourceFieldKey::ConnectTimeout => resources.connect_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::SyncTimeout => resources.sync_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::BuildTimeout => resources.build_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxOutput => resources.max_output = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::IdleOutputTimeout => resources.idle_output_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::CleanupTimeout => resources.cleanup_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::ArtifactTimeout => resources.artifact_timeout = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxArtifactBytes => resources.max_artifact_bytes = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxArtifactTotalBytes => resources.max_artifact_total_bytes = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxArtifactEntries => resources.max_artifact_entries = Some(count()?),
                ResourceFieldKey::MaxWorkerUploads => resources.max_worker_uploads = Some(count()?),
                ResourceFieldKey::MaxWorkerUploadBytes => resources.max_worker_upload_bytes = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxWorkerJobs => resources.max_worker_jobs = Some(count()?),
                ResourceFieldKey::MaxWorkerJobBytes => resources.max_worker_job_bytes = Some(RemoteQuantity::Text(value.to_string())),
                ResourceFieldKey::MaxWorkerArtifactSpools => resources.max_worker_artifact_spools = Some(count()?),
                ResourceFieldKey::MaxWorkerArtifactSpoolBytes => {
                    resources.max_worker_artifact_spool_bytes = Some(RemoteQuantity::Text(value.to_string()))
                }
                ResourceFieldKey::MaxWorkerStateEntries => resources.max_worker_state_entries = Some(count()?),
                ResourceFieldKey::MaxActiveBuilds => resources.max_active_builds = Some(count()?),
            }
        }
        Ok((!resources.is_empty()).then_some(resources))
    }

    fn clear(&mut self) {
        for input in &mut self.inputs {
            input.value = TextField::new("");
        }
        self.use_defaults = true;
        self.focus = 0;
    }
}

fn resource_input(key: ResourceFieldKey, label: &'static str, kind: ResourceFieldKind, value: String) -> ResourceInput {
    ResourceInput { key, label, kind, value: TextField::new(value) }
}

fn quantity_text(value: Option<&RemoteQuantity>) -> String {
    match value {
        Some(RemoteQuantity::Integer(value)) => value.to_string(),
        Some(RemoteQuantity::Text(value)) => value.clone(),
        None => String::new(),
    }
}

fn count_text(value: Option<u64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

#[derive(Clone, Debug)]
struct RemoteSetupState {
    draft: RemoteConfigDraft,
    selected: usize,
    screen: SetupScreen,
    override_focus: OverrideFocus,
    target_form: Option<TargetFormState>,
    tool_form: Option<ToolFormState>,
    tools: Option<ToolListState>,
    strings: Option<StringListState>,
    resources: Option<ResourceFormState>,
    delete_label: Option<String>,
    error: Option<String>,
}

impl RemoteSetupState {
    fn new(draft: RemoteConfigDraft) -> Self {
        Self {
            draft,
            selected: 0,
            screen: SetupScreen::List,
            override_focus: OverrideFocus::Tools,
            target_form: None,
            tool_form: None,
            tools: None,
            strings: None,
            resources: None,
            delete_label: None,
            error: None,
        }
    }

    fn labels(&self) -> Vec<String> {
        self.draft.targets.keys().cloned().collect()
    }
}

#[derive(Clone, Debug)]
struct ConfigErrorState {
    message: String,
    view: bool,
}

struct HostUiState {
    popup: HostPopup,
    target_index: usize,
    confirmation: Option<(String, Instant)>,
    free_bytes: Option<u64>,
    last_free_refresh: Instant,
    setup: Option<RemoteSetupState>,
    config_error: Option<ConfigErrorState>,
}

impl HostUiState {
    fn new(catalog: &BuildTargetCatalog) -> Self {
        Self {
            popup: HostPopup::None,
            target_index: catalog.summaries().iter().position(|target| target.label() == "localhost").unwrap_or(0),
            confirmation: None,
            free_bytes: None,
            last_free_refresh: Instant::now() - Duration::from_secs(10),
            setup: None,
            config_error: None,
        }
    }

    fn refresh_free_space(&mut self, catalog: &BuildTargetCatalog) {
        if self.last_free_refresh.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_free_refresh = Instant::now();
        self.free_bytes = local_free_space(catalog.project_root());
    }

    fn handle_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog, active: &ActiveBuildTarget) -> bool {
        match self.popup {
            HostPopup::Targets => return self.handle_target_popup_key(key, catalog, active),
            HostPopup::Help => {
                if key.code == KeyCode::Esc {
                    self.popup = HostPopup::None;
                }
                return true;
            }
            HostPopup::Setup => return self.handle_setup_key(key, catalog),
            HostPopup::ConfigError => {
                if key.code == KeyCode::Char('v') && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                    if let Some(error) = &mut self.config_error {
                        error.view = true;
                    }
                } else if key.code == KeyCode::Esc {
                    self.popup = HostPopup::None;
                    self.config_error = None;
                }
                return true;
            }
            HostPopup::None => {}
        }

        if key.code == KeyCode::Char('b') && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            let current = active.current();
            self.target_index = catalog.summaries().iter().position(|target| target.label() == current).unwrap_or(0);
            self.popup = HostPopup::Targets;
            return true;
        }
        if key.code == KeyCode::Char('h') && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            self.popup = HostPopup::Help;
            return true;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT) {
            self.open_setup(catalog);
            return true;
        }
        false
    }

    fn handle_target_popup_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog, active: &ActiveBuildTarget) -> bool {
        match key.code {
            KeyCode::Up => self.target_index = self.target_index.saturating_sub(1),
            KeyCode::Down => self.target_index = (self.target_index + 1).min(catalog.summaries().len().saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(target) = catalog.summaries().get(self.target_index) {
                    if active.select(catalog, target.label()).is_ok() {
                        self.confirmation = Some((format!("Build target: {}", target.label()), Instant::now()));
                    }
                }
                self.popup = HostPopup::None;
            }
            KeyCode::Esc => self.popup = HostPopup::None,
            _ => {}
        }
        true
    }

    fn open_setup(&mut self, catalog: &BuildTargetCatalog) {
        match RemoteConfigDraft::load_optional(catalog.project_root(), catalog.base_project()) {
            Ok(Some(draft)) => {
                self.setup = Some(RemoteSetupState::new(draft));
                self.config_error = None;
                self.popup = HostPopup::Setup;
            }
            Ok(None) => {
                self.setup = Some(RemoteSetupState::new(RemoteConfigDraft::default()));
                self.config_error = None;
                self.popup = HostPopup::Setup;
            }
            Err(error) => {
                self.setup = None;
                self.config_error = Some(ConfigErrorState { message: error, view: false });
                self.popup = HostPopup::ConfigError;
            }
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog) -> bool {
        let screen = self.setup.as_ref().map_or(SetupScreen::List, |setup| setup.screen);
        match screen {
            SetupScreen::List => self.handle_setup_list_key(key, catalog),
            SetupScreen::TargetForm => self.handle_target_form_key(key, catalog),
            SetupScreen::Overrides => self.handle_overrides_key(key, catalog),
            SetupScreen::Tools => self.handle_tools_key(key),
            SetupScreen::ToolForm => self.handle_tool_form_key(key),
            SetupScreen::Environment | SetupScreen::Exclusions | SetupScreen::Artifacts => self.handle_string_list_key(key),
            SetupScreen::Resources | SetupScreen::AdvancedResources => self.handle_resources_key(key, catalog),
            SetupScreen::ConfirmDelete => self.handle_delete_key(key),
        }
    }

    fn handle_setup_list_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(setup) = &mut self.setup {
                    setup.selected = setup.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(setup) = &mut self.setup {
                    setup.selected = (setup.selected + 1).min(setup.draft.targets.len().saturating_sub(1));
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => self.begin_target_form(None),
            KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                let label = self.setup.as_ref().and_then(|setup| setup.labels().get(setup.selected).cloned());
                if let Some(label) = label {
                    self.begin_target_form(Some(label));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                let label = self.setup.as_ref().and_then(|setup| setup.labels().get(setup.selected).cloned());
                if let Some(setup) = &mut self.setup {
                    setup.delete_label = label;
                    if setup.delete_label.is_some() {
                        setup.screen = SetupScreen::ConfirmDelete;
                    }
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => self.save_setup(catalog),
            KeyCode::Esc => {
                self.setup = None;
                self.popup = HostPopup::None;
            }
            _ => {}
        }
        true
    }

    fn begin_target_form(&mut self, label: Option<String>) {
        let draft = label.as_ref().and_then(|label| self.setup.as_ref()?.draft.targets.get(label).cloned()).unwrap_or_else(|| RemoteTargetDraft {
            ssh: String::new(),
            workspace: String::new(),
            project: None,
            resources: None,
        });
        if let Some(setup) = &mut self.setup {
            setup.error = None;
            setup.target_form = Some(TargetFormState::new(label, draft));
            setup.screen = SetupScreen::TargetForm;
        }
    }

    fn handle_target_form_key(&mut self, key: KeyEvent, catalog: &BuildTargetCatalog) -> bool {
        let consumed = if let Some(setup) = &mut self.setup {
            if let Some(form) = &mut setup.target_form {
                match form.focus {
                    TargetFormFocus::Label => form.label.handle_key(key),
                    TargetFormFocus::Ssh => form.ssh.handle_key(key),
                    TargetFormFocus::Workspace => form.workspace.handle_key(key),
                    _ => false,
                }
            } else {
                false
            }
        } else {
            false
        };
        if consumed {
            return true;
        }

        if key.code == KeyCode::Tab {
            if let Some(form) = self.setup.as_mut().and_then(|setup| setup.target_form.as_mut()) {
                form.focus = form.focus.next(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            return true;
        }
        match key.code {
            KeyCode::Enter => {
                let focus = self.setup.as_ref().and_then(|setup| setup.target_form.as_ref()).map_or(TargetFormFocus::Cancel, |form| form.focus);
                match focus {
                    TargetFormFocus::Overrides => self.open_overrides(),
                    TargetFormFocus::Resources => self.open_resources(),
                    TargetFormFocus::Save => self.commit_target_form(catalog),
                    TargetFormFocus::Cancel => self.cancel_target_form(),
                    _ => {
                        if let Some(form) = self.setup.as_mut().and_then(|setup| setup.target_form.as_mut()) {
                            form.focus = form.focus.next(false);
                        }
                    }
                }
            }
            KeyCode::Esc => self.cancel_target_form(),
            _ => {}
        }
        true
    }

    fn open_overrides(&mut self) {
        if let Some(setup) = &mut self.setup {
            setup.error = None;
            setup.override_focus = OverrideFocus::Tools;
            setup.screen = SetupScreen::Overrides;
        }
    }

    fn open_resources(&mut self) {
        let resources =
            self.setup.as_ref().and_then(|setup| setup.target_form.as_ref()).map(|form| ResourceFormState::new(form.draft.resources.as_ref()));
        if let Some(setup) = &mut self.setup {
            setup.resources = resources;
            setup.error = None;
            setup.screen = SetupScreen::Resources;
        }
    }

    fn cancel_target_form(&mut self) {
        if let Some(setup) = &mut self.setup {
            setup.target_form = None;
            setup.tools = None;
            setup.tool_form = None;
            setup.strings = None;
            setup.resources = None;
            setup.error = None;
            setup.screen = SetupScreen::List;
        }
    }

    fn commit_target_form(&mut self, catalog: &BuildTargetCatalog) {
        let Some(setup) = &mut self.setup else { return };
        let Some(form) = setup.target_form.take() else { return };
        let candidate_label = form.label.value.clone();
        let candidate = normalize_target_draft(form.candidate());
        let result = (|| {
            validate_remote_target_label(&candidate_label)?;
            validate_ssh_destination(&candidate.ssh)?;
            validate_workspace_path(&candidate.workspace)?;
            if form.original_label.as_deref() != Some(candidate_label.as_str()) && setup.draft.targets.contains_key(&candidate_label) {
                return Err(format!("duplicate target label: {candidate_label}"));
            }
            let mut draft = setup.draft.clone();
            if let Some(original) = &form.original_label {
                draft.targets.remove(original);
            }
            draft.targets.insert(candidate_label.clone(), candidate);
            draft.validate(catalog.base_project())?;
            Ok::<RemoteConfigDraft, String>(draft)
        })();

        match result {
            Ok(draft) => {
                setup.draft = draft;
                setup.selected = setup.draft.targets.keys().position(|label| label == &candidate_label).unwrap_or(0);
                setup.error = None;
                setup.screen = SetupScreen::List;
            }
            Err(error) => {
                setup.error = Some(error);
                setup.target_form = Some(form);
            }
        }
    }

    fn handle_overrides_key(&mut self, key: KeyEvent, _catalog: &BuildTargetCatalog) -> bool {
        let focus = self.setup.as_ref().map_or(OverrideFocus::Done, |setup| setup.override_focus);
        if key.code == KeyCode::Tab {
            if let Some(setup) = &mut self.setup {
                setup.override_focus = setup.override_focus.next(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            return true;
        }
        match key.code {
            KeyCode::Up => {
                if let Some(setup) = &mut self.setup {
                    setup.override_focus = focus.next(true);
                }
            }
            KeyCode::Down => {
                if let Some(setup) = &mut self.setup {
                    setup.override_focus = focus.next(false);
                }
            }
            KeyCode::Enter => match focus {
                OverrideFocus::Tools => self.open_tools(),
                OverrideFocus::Environment => self.open_string_list(StringListKind::Environment),
                OverrideFocus::Exclusions => self.open_string_list(StringListKind::Exclusions),
                OverrideFocus::Artifacts => self.open_string_list(StringListKind::Artifacts),
                OverrideFocus::Done => {
                    if let Some(setup) = &mut self.setup {
                        setup.screen = SetupScreen::TargetForm;
                    }
                }
            },
            KeyCode::Esc => {
                if let Some(setup) = &mut self.setup {
                    setup.screen = SetupScreen::TargetForm;
                    setup.error = None;
                }
            }
            _ => {}
        }
        true
    }

    fn open_tools(&mut self) {
        let entries = self
            .setup
            .as_ref()
            .and_then(|setup| setup.target_form.as_ref())
            .and_then(|form| form.draft.project.as_ref())
            .and_then(|project| project.remote.as_ref())
            .and_then(|remote| remote.tools.clone())
            .unwrap_or_default();
        if let Some(setup) = &mut self.setup {
            setup.tools = Some(ToolListState { entries, selected: 0, changed: false, error: None });
            setup.error = None;
            setup.screen = SetupScreen::Tools;
        }
    }

    fn open_string_list(&mut self, kind: StringListKind) {
        let entries = self
            .setup
            .as_ref()
            .and_then(|setup| setup.target_form.as_ref())
            .and_then(|form| form.draft.project.as_ref())
            .and_then(|project| project.remote.as_ref())
            .and_then(|remote| match kind {
                StringListKind::Environment => remote.environment.clone(),
                StringListKind::Exclusions => remote.exclude.clone(),
                StringListKind::Artifacts => remote.artifacts.clone(),
            });
        if let Some(setup) = &mut self.setup {
            setup.strings = Some(StringListState::new(kind, entries));
            setup.error = None;
            setup.screen = match kind {
                StringListKind::Environment => SetupScreen::Environment,
                StringListKind::Exclusions => SetupScreen::Exclusions,
                StringListKind::Artifacts => SetupScreen::Artifacts,
            };
        }
    }

    fn save_setup(&mut self, catalog: &BuildTargetCatalog) {
        let Some(draft) = self.setup.as_ref().map(|setup| setup.draft.clone()) else { return };
        match draft.write_atomic(catalog.project_root(), catalog.base_project()) {
            Ok(()) => {
                self.confirmation = Some(("Saved .bunkerbox/remote.conf; changes apply next run".to_string(), Instant::now()));
                self.setup = None;
                self.popup = HostPopup::None;
            }
            Err(error) => {
                if let Some(setup) = &mut self.setup {
                    setup.error = Some(error);
                }
            }
        }
    }

    fn handle_tools_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(tools) = self.setup.as_mut().and_then(|setup| setup.tools.as_mut()) {
                    tools.selected = tools.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(tools) = self.setup.as_mut().and_then(|setup| setup.tools.as_mut()) {
                    tools.selected = (tools.selected + 1).min(tools.entries.len().saturating_sub(1));
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => self.begin_tool_form(None),
            KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                let selected = self.setup.as_ref().and_then(|setup| setup.tools.as_ref()).map(|tools| tools.selected);
                if let Some(index) = selected {
                    self.begin_tool_form(Some(index));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(setup) = &mut self.setup {
                    if let Some(tools) = &mut setup.tools {
                        if tools.selected < tools.entries.len() {
                            tools.entries.remove(tools.selected);
                            tools.selected = tools.selected.min(tools.entries.len().saturating_sub(1));
                            tools.changed = true;
                            tools.error = None;
                        }
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.finish_tools(),
            _ => {}
        }
        true
    }

    fn begin_tool_form(&mut self, index: Option<usize>) {
        let tool = index.and_then(|index| self.setup.as_ref()?.tools.as_ref()?.entries.get(index).cloned());
        if let Some(setup) = &mut self.setup {
            setup.tool_form = Some(ToolFormState::new(index, tool.as_ref()));
            setup.error = None;
            setup.screen = SetupScreen::ToolForm;
        }
    }

    fn handle_tool_form_key(&mut self, key: KeyEvent) -> bool {
        let focus = self.setup.as_ref().and_then(|setup| setup.tool_form.as_ref()).map_or(4, |form| form.focus);
        let consumed = if let Some(form) = self.setup.as_mut().and_then(|setup| setup.tool_form.as_mut()) {
            match form.focus {
                0 => form.name.handle_key(key),
                1 => form.command.handle_key(key),
                _ => false,
            }
        } else {
            false
        };
        if consumed {
            return true;
        }
        if key.code == KeyCode::Tab {
            if let Some(form) = self.setup.as_mut().and_then(|setup| setup.tool_form.as_mut()) {
                form.next_focus(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            return true;
        }
        match key.code {
            KeyCode::Char(' ') if focus == 2 => {
                if let Some(form) = self.setup.as_mut().and_then(|setup| setup.tool_form.as_mut()) {
                    form.allow_args = !form.allow_args;
                }
            }
            KeyCode::Enter if focus == 3 => self.commit_tool_form(),
            KeyCode::Enter if focus == 4 => self.cancel_tool_form(),
            KeyCode::Enter => {
                if let Some(form) = self.setup.as_mut().and_then(|setup| setup.tool_form.as_mut()) {
                    form.next_focus(false);
                }
            }
            KeyCode::Esc => self.cancel_tool_form(),
            _ => {}
        }
        true
    }

    fn commit_tool_form(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(form) = setup.tool_form.take() else { return };
        let name = form.name.value.trim().to_string();
        let command = match form.command.value.trim() {
            "" => None,
            value => Some(value.to_string()),
        };
        let candidate = RemoteToolSpec { name, command, allow_args: form.allow_args };
        let result = (|| {
            validate_remote_tool_spec(&candidate)?;
            let tools = setup.tools.as_ref().ok_or_else(|| "tool list is unavailable".to_string())?;
            if tools.entries.iter().enumerate().any(|(index, tool)| Some(index) != form.original_index && tool.name == candidate.name) {
                return Err(format!("duplicate remote tool: {}", candidate.name));
            }
            Ok::<(), String>(())
        })();
        match result {
            Ok(()) => {
                if let Some(tools) = &mut setup.tools {
                    if let Some(index) = form.original_index {
                        if index < tools.entries.len() {
                            tools.entries[index] = candidate;
                            tools.selected = index;
                        }
                    } else {
                        tools.entries.push(candidate);
                        tools.selected = tools.entries.len().saturating_sub(1);
                    }
                    tools.changed = true;
                    tools.error = None;
                }
                setup.screen = SetupScreen::Tools;
                setup.error = None;
            }
            Err(error) => {
                setup.error = Some(error.clone());
                let mut restored = form;
                restored.name = TextField::new(candidate.name);
                restored.command = TextField::new(candidate.command.unwrap_or_default());
                setup.tool_form = Some(restored);
            }
        }
    }

    fn cancel_tool_form(&mut self) {
        if let Some(setup) = &mut self.setup {
            setup.tool_form = None;
            setup.error = None;
            setup.screen = SetupScreen::Tools;
        }
    }

    fn finish_tools(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(tools) = setup.tools.take() else { return };
        if tools.changed {
            let mut names = std::collections::BTreeSet::new();
            let result = tools.entries.iter().try_for_each(|tool| {
                validate_remote_tool_spec(tool)?;
                if !names.insert(tool.name.clone()) {
                    return Err(format!("duplicate remote tool: {}", tool.name));
                }
                Ok::<(), String>(())
            });
            if let Err(error) = result {
                setup.error = Some(error);
                setup.tools = Some(tools);
                return;
            }
            if let Some(form) = &mut setup.target_form {
                remote_overlay_mut(&mut form.draft).tools = Some(tools.entries);
            }
        }
        setup.tool_form = None;
        setup.error = None;
        setup.screen = SetupScreen::Overrides;
    }

    fn handle_string_list_key(&mut self, key: KeyEvent) -> bool {
        let editing = self.setup.as_ref().and_then(|setup| setup.strings.as_ref()).is_some_and(|state| state.editing.is_some());
        if editing {
            let consumed = if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
                state.editing.as_mut().is_some_and(|field| field.handle_key(key))
            } else {
                false
            };
            if consumed {
                return true;
            }
            match key.code {
                KeyCode::Enter => self.commit_string_edit(),
                KeyCode::Esc => {
                    if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
                        state.editing = None;
                        state.editing_index = None;
                    }
                }
                _ => {}
            }
            return true;
        }

        match key.code {
            KeyCode::Up => {
                if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
                    state.selected = state.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
                    state.selected = (state.selected + 1).min(state.entries.len().saturating_sub(1));
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => self.begin_string_edit(None),
            KeyCode::Char('e') | KeyCode::Char('E') | KeyCode::Enter => {
                let selected = self.setup.as_ref().and_then(|setup| setup.strings.as_ref()).map(|state| state.selected);
                if let Some(index) = selected {
                    self.begin_string_edit(Some(index));
                }
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
                    if state.selected < state.entries.len() {
                        state.entries.remove(state.selected);
                        state.selected = state.selected.min(state.entries.len().saturating_sub(1));
                        state.changed = true;
                    }
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.finish_string_list(),
            _ => {}
        }
        true
    }

    fn begin_string_edit(&mut self, index: Option<usize>) {
        let value = index.and_then(|index| self.setup.as_ref()?.strings.as_ref()?.entries.get(index).cloned()).unwrap_or_default();
        if let Some(state) = self.setup.as_mut().and_then(|setup| setup.strings.as_mut()) {
            state.editing = Some(TextField::new(value));
            state.editing_index = index;
            state.error = None;
        }
    }

    fn commit_string_edit(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(state) = &mut setup.strings else { return };
        let Some(editing) = state.editing.take() else { return };
        let value = editing.value.trim().to_string();
        if value.is_empty() {
            state.error = Some("value must not be empty".to_string());
            state.editing = Some(TextField::new(value));
            return;
        }
        let mut candidate = state.entries.clone();
        if let Some(index) = state.editing_index {
            if index < candidate.len() {
                candidate[index] = value;
            }
        } else {
            candidate.push(value);
        }
        let result = validate_string_entries(state.kind, &candidate);
        if let Err(error) = result {
            state.error = Some(error);
            state.editing = Some(editing);
            return;
        }
        state.entries = candidate;
        state.selected = state.editing_index.unwrap_or_else(|| state.entries.len().saturating_sub(1));
        state.editing_index = None;
        state.changed = true;
        state.error = None;
    }

    fn finish_string_list(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(state) = setup.strings.take() else { return };
        if state.changed {
            if let Err(error) = validate_string_entries(state.kind, &state.entries) {
                setup.error = Some(error);
                setup.strings = Some(state);
                return;
            }
            if let Some(form) = &mut setup.target_form {
                let remote = remote_overlay_mut(&mut form.draft);
                match state.kind {
                    StringListKind::Environment => remote.environment = Some(state.entries),
                    StringListKind::Exclusions => remote.exclude = Some(state.entries),
                    StringListKind::Artifacts => remote.artifacts = Some(state.entries),
                }
            }
        }
        setup.error = None;
        setup.screen = SetupScreen::Overrides;
    }

    fn handle_resources_key(&mut self, key: KeyEvent, _catalog: &BuildTargetCatalog) -> bool {
        let consumed = if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
            resources
                .visible_indices()
                .get(resources.focus)
                .and_then(|index| resources.inputs.get_mut(*index))
                .is_some_and(|input| input.value.handle_key(key))
        } else {
            false
        };
        if consumed {
            return true;
        }
        if key.code == KeyCode::Tab {
            if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                let count = resources.visible_indices().len() + 3;
                resources.focus =
                    if key.modifiers.contains(KeyModifiers::SHIFT) { (resources.focus + count - 1) % count } else { (resources.focus + 1) % count };
            }
            return true;
        }
        match key.code {
            KeyCode::Up => {
                if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                    let count = resources.visible_indices().len() + 3;
                    resources.focus = (resources.focus + count - 1) % count;
                }
            }
            KeyCode::Down => {
                if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                    let count = resources.visible_indices().len() + 3;
                    resources.focus = (resources.focus + 1) % count;
                }
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Char('a') | KeyCode::Char('A') => {
                let advanced = if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                    resources.advanced = !resources.advanced;
                    resources.focus = 0;
                    resources.advanced
                } else {
                    false
                };
                if let Some(setup) = &mut self.setup {
                    setup.screen = if advanced { SetupScreen::AdvancedResources } else { SetupScreen::Resources };
                }
            }
            KeyCode::Enter => {
                let action = self.setup.as_ref().and_then(|setup| setup.resources.as_ref()).map(|resources| {
                    let fields = resources.visible_indices().len();
                    match resources.focus {
                        focus if focus < fields => 0,
                        focus if focus == resources.action_index(0) => 1,
                        focus if focus == resources.action_index(1) => 2,
                        _ => 3,
                    }
                });
                match action {
                    Some(1) => {
                        if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                            resources.clear();
                        }
                    }
                    Some(2) => self.commit_resources(),
                    Some(3) => self.cancel_resources(),
                    _ => {
                        if let Some(resources) = self.setup.as_mut().and_then(|setup| setup.resources.as_mut()) {
                            let count = resources.visible_indices().len() + 3;
                            resources.focus = (resources.focus + 1) % count;
                        }
                    }
                }
            }
            KeyCode::Esc => self.cancel_resources(),
            _ => {}
        }
        true
    }

    fn commit_resources(&mut self) {
        let Some(setup) = &mut self.setup else { return };
        let Some(resources) = setup.resources.take() else { return };
        let draft = match resources.draft() {
            Ok(draft) => draft,
            Err(error) => {
                setup.error = Some(error);
                setup.resources = Some(resources);
                return;
            }
        };
        let result = draft.as_ref().map_or(Ok(()), |resources| validate_remote_resource_overrides(resources).map(|_| ()));
        match result {
            Ok(()) => {
                if let Some(form) = &mut setup.target_form {
                    form.draft.resources = draft;
                }
                setup.error = None;
                setup.screen = SetupScreen::TargetForm;
            }
            Err(error) => {
                setup.error = Some(error);
                setup.resources = Some(resources);
            }
        }
    }

    fn cancel_resources(&mut self) {
        if let Some(setup) = &mut self.setup {
            setup.resources = None;
            setup.error = None;
            setup.screen = SetupScreen::TargetForm;
        }
    }

    fn handle_delete_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                if let Some(setup) = &mut self.setup {
                    if let Some(label) = setup.delete_label.take() {
                        setup.draft.targets.remove(&label);
                        setup.selected = setup.selected.min(setup.draft.targets.len().saturating_sub(1));
                    }
                    setup.screen = SetupScreen::List;
                }
            }
            KeyCode::Esc => {
                if let Some(setup) = &mut self.setup {
                    setup.delete_label = None;
                    setup.screen = SetupScreen::List;
                }
            }
            _ => {}
        }
        true
    }
}

fn validate_string_entries(kind: StringListKind, entries: &[String]) -> Result<(), String> {
    match kind {
        StringListKind::Environment => validate_remote_environment_names(entries),
        StringListKind::Exclusions => validate_remote_exclusion_entries(entries),
        StringListKind::Artifacts => validate_remote_artifact_paths(entries),
    }
}

fn remote_overlay_mut(target: &mut RemoteTargetDraft) -> &mut RemoteOverlayDraft {
    target.project.get_or_insert_with(RemoteProjectOverlayDraft::default).remote.get_or_insert_with(RemoteOverlayDraft::default)
}

fn normalize_target_draft(mut target: RemoteTargetDraft) -> RemoteTargetDraft {
    if target.project.as_ref().is_some_and(|project| {
        project
            .remote
            .as_ref()
            .is_some_and(|remote| remote.exclude.is_none() && remote.environment.is_none() && remote.tools.is_none() && remote.artifacts.is_none())
    }) {
        target.project = None;
    }
    if target.project.as_ref().is_some_and(|project| project.remote.is_none()) {
        target.project = None;
    }
    if target.resources.as_ref().is_some_and(RemoteResourceOverridesDraft::is_empty) {
        target.resources = None;
    }
    target
}

pub fn show_host_error(overlay: &Arc<Mutex<OverlayState>>, title: &str, message: &str) {
    if let Ok(mut state) = overlay.lock() {
        state.error_toast =
            Some(ErrorToast { title: title.chars().take(80).collect(), message: message.chars().take(512).collect(), shown_at: Instant::now() });
    }
}

pub fn guest_rows(physical_rows: u16) -> u16 {
    physical_rows.saturating_sub(1).max(1)
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            status_text: String::new(),
            popup: PopupWidget::new(),
            popup_title: None,
            pending: Vec::new(),
            has_error: false,
            error_toast: None,
            hide_on_ascii: false,
            hide_on_content: None,
            last_content_scan: Instant::now(),
        }
    }
}

pub fn dispatch_ui_command(state: &mut OverlayState, widget: &str, command: &str, options: &str, value: &str) {
    if widget == WIDGET_ERROR && command == CMD_SHOW {
        state.error_toast = Some(ErrorToast {
            title: if options.is_empty() { "Bunkerbox error".to_string() } else { options.chars().take(80).collect() },
            message: value.chars().take(512).collect(),
            shown_at: Instant::now(),
        });
        return;
    }

    if widget == WIDGET_POPUP && command == CMD_HIDE && !value.is_empty() {
        if value == "ASCII" {
            state.hide_on_ascii = true;
        } else {
            state.hide_on_content = Some(value.to_string());
        }
        return;
    }

    let triggers = parse_triggers(options);
    if !triggers.is_empty() {
        state.pending.push(PendingAction {
            widget: widget.to_string(),
            command: command.to_string(),
            triggers,
            value: value.to_string(),
            enqueued_at: Instant::now(),
            first_pty_at: None,
        });
        return;
    }

    match (widget, command) {
        (WIDGET_PROGRESS, CMD_SET) => {
            if let Ok(pct) = value.parse::<f64>() {
                state.popup.set_progress(pct.clamp(0.0, 1.0), None);
            }
        }
        (WIDGET_PROGRESS, CMD_SHOW) => {
            if let Some((pct, label)) = value.split_once(';') {
                let pct: f64 = pct.trim().parse().unwrap_or(0.0);
                state.popup.show_progress("", pct.clamp(0.0, 1.0), Some(label.trim().to_string()));
            } else if let Ok(pct) = value.parse::<f64>() {
                state.popup.show_progress("", pct.clamp(0.0, 1.0), None);
            }
        }
        (WIDGET_PROGRESS, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_STATUS, "phase") => {
            state.popup_title = if value.is_empty() { None } else { Some(value.to_string()) };
        }
        (WIDGET_STATUS, CMD_SET) => {
            let title = state.popup_title.clone();
            state.popup.show_info(title, value, Some(palette::FG), Some(palette::ACCENT));
        }
        (WIDGET_STATUS, CMD_CLEAR) => {
            state.popup.hide();
            state.popup_title = None;
        }
        (WIDGET_POPUP, CMD_SHOW) => {
            state.popup.show_info(None, value, None, None);
        }
        (WIDGET_POPUP, "info") => {
            let title = if options.is_empty() { None } else { Some(options.to_string()) };
            if title.as_deref() == Some("Error") {
                state.has_error = true;
            }
            state.popup.show_info(title, value, Some(palette::FG), Some(palette::ACCENT));
        }
        (WIDGET_POPUP, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_SPINNER, CMD_SHOW) => {
            state.popup.show_spinner(value);
        }
        (WIDGET_SPINNER, CMD_HIDE) => {
            state.popup.hide();
        }
        (WIDGET_PASSWORD, CMD_SHOW) => {
            state.popup.show_password(options, value);
        }
        (WIDGET_PASSWORD, CMD_HIDE) => {
            state.popup.hide();
        }
        _ => {}
    }
}

/// Terminal emulator wrapper around [`vt100::Parser`] with DEC Special Graphics
/// character set translation and HVP-to-CUP normalization.
struct Term {
    parser: vt100::Parser,
    responses: Vec<Vec<u8>>,
    escape_state: EscapeState,
    g0_dec_special_graphics: bool,
    g1_dec_special_graphics: bool,
    using_g1_charset: bool,
    application_cursor_keys: bool,
    mouse_normal: bool,
    mouse_button: bool,
    mouse_any: bool,
    mouse_sgr: bool,
    mouse_urxvt: bool,
    csi_bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
enum EscapeState {
    Ground,
    Escape,
    CharsetSelect(u8),
    Csi,
    String,
    StringEscape,
}

impl Term {
    /// Creates a new terminal of the given rows and columns.
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            responses: Vec::new(),
            escape_state: EscapeState::Ground,
            g0_dec_special_graphics: false,
            g1_dec_special_graphics: false,
            using_g1_charset: false,
            application_cursor_keys: false,
            mouse_normal: false,
            mouse_button: false,
            mouse_any: false,
            mouse_sgr: false,
            mouse_urxvt: false,
            csi_bytes: Vec::new(),
        }
    }

    /// Returns a reference to the vt100 screen grid for rendering.
    fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Resizes the terminal grid after a window resize event.
    fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Feeds raw bytes through DEC translation then into the vt100 parser.
    fn process(&mut self, bytes: &[u8]) {
        self.process_bytes(bytes);
    }

    fn drain_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.responses)
    }

    #[allow(dead_code)]
    fn application_cursor_keys(&self) -> bool {
        self.application_cursor_keys
    }

    fn mouse_tracking(&self) -> MouseTracking {
        if self.mouse_any {
            MouseTracking::Any
        } else if self.mouse_button {
            MouseTracking::Button
        } else if self.mouse_normal {
            MouseTracking::Normal
        } else {
            MouseTracking::Off
        }
    }

    fn mouse_encoding(&self) -> MouseEncoding {
        if self.mouse_sgr {
            MouseEncoding::Sgr
        } else if self.mouse_urxvt {
            MouseEncoding::Urxvt
        } else {
            MouseEncoding::X10
        }
    }

    fn active_dec_special_graphics(&self) -> bool {
        if self.using_g1_charset {
            self.g1_dec_special_graphics
        } else {
            self.g0_dec_special_graphics
        }
    }

    /// Translates `\e(0` DEC Special Graphics characters to Unicode
    /// box-drawing glyphs, normalizes HVP (`CSI … f`) to CUP (`CSI … H`),
    /// and answers stream-split terminal queries after preceding bytes have
    /// been parsed.
    fn process_bytes(&mut self, bytes: &[u8]) {
        let mut translated = Vec::with_capacity(bytes.len());

        for &byte in bytes {
            match self.escape_state {
                EscapeState::Ground => match byte {
                    0x1b => {
                        self.escape_state = EscapeState::Escape;
                    }
                    0x0e => {
                        self.using_g1_charset = true;
                    }
                    0x0f => {
                        self.using_g1_charset = false;
                    }
                    0x20..=0x7e if self.active_dec_special_graphics() => {
                        push_dec_special_graphic(&mut translated, byte);
                    }
                    _ => {
                        translated.push(byte);
                    }
                },
                EscapeState::Escape => match byte {
                    b'(' | b')' => {
                        self.escape_state = EscapeState::CharsetSelect(byte);
                    }
                    b'[' => {
                        self.escape_state = EscapeState::Csi;
                        self.csi_bytes.clear();
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                    b']' | b'P' | b'^' | b'_' => {
                        self.escape_state = EscapeState::String;
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                    _ => {
                        self.escape_state = EscapeState::Ground;
                        translated.push(0x1b);
                        translated.push(byte);
                    }
                },
                EscapeState::CharsetSelect(charset) => {
                    match charset {
                        b'(' => self.g0_dec_special_graphics = byte == b'0',
                        b')' => self.g1_dec_special_graphics = byte == b'0',
                        _ => {}
                    }
                    self.escape_state = EscapeState::Ground;
                }
                EscapeState::Csi => {
                    self.csi_bytes.push(byte);
                    if byte == b'f' {
                        translated.push(b'H');
                    } else {
                        translated.push(byte);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape_state = EscapeState::Ground;
                        if self.handle_csi_complete(&mut translated) {
                            translated.clear();
                        }
                        self.csi_bytes.clear();
                    }
                }
                EscapeState::String => {
                    translated.push(byte);
                    match byte {
                        0x07 => self.escape_state = EscapeState::Ground,
                        0x1b => self.escape_state = EscapeState::StringEscape,
                        _ => {}
                    }
                }
                EscapeState::StringEscape => {
                    translated.push(byte);
                    self.escape_state = if byte == b'\\' { EscapeState::Ground } else { EscapeState::String };
                }
            }
        }

        if !translated.is_empty() {
            self.parser.process(&translated);
        }
    }

    fn handle_csi_complete(&mut self, translated: &mut [u8]) -> bool {
        self.update_private_modes();

        match self.csi_bytes.as_slice() {
            b"?1h" => self.application_cursor_keys = true,
            b"?1l" => self.application_cursor_keys = false,
            b"6n" => {
                self.parser.process(translated);
                let (cur_row, cur_col) = self.parser.screen().cursor_position();
                self.responses.push(format!("\x1b[{};{}R", cur_row + 1, cur_col + 1).into_bytes());
                return true;
            }
            b"18t" => {
                self.parser.process(translated);
                let (rows, cols) = self.parser.screen().size();
                self.responses.push(format!("\x1b[8;{};{}t", rows, cols).into_bytes());
                return true;
            }
            _ => {}
        }

        false
    }

    fn update_private_modes(&mut self) {
        let bytes = self.csi_bytes.clone();
        if bytes.first() != Some(&b'?') || bytes.len() < 3 {
            return;
        }

        let Some((&final_byte, params)) = bytes.split_last() else {
            return;
        };
        let enabled = match final_byte {
            b'h' => true,
            b'l' => false,
            _ => return,
        };

        for raw_mode in params[1..].split(|byte| *byte == b';') {
            let Ok(mode) = std::str::from_utf8(raw_mode).unwrap_or_default().parse::<u16>() else {
                continue;
            };
            match mode {
                1000 => self.mouse_normal = enabled,
                1002 => self.mouse_button = enabled,
                1003 => self.mouse_any = enabled,
                1006 => self.mouse_sgr = enabled,
                1015 => self.mouse_urxvt = enabled,
                1005 => {}
                _ => {}
            }
        }
    }
}

/// Converts a single byte from the DEC Special Graphics table to its Unicode
/// equivalent (e.g. `x` → `│`, `q` → `─`). Appends the UTF-8 bytes to `out`.
fn push_dec_special_graphic(out: &mut Vec<u8>, byte: u8) {
    let mapped = match byte {
        b'`' => '◆',
        b'a' => '▒',
        b'b' => '␉',
        b'c' => '␌',
        b'd' => '␍',
        b'e' => '␊',
        b'f' => '°',
        b'g' => '±',
        b'h' => '␤',
        b'i' => '␋',
        b'j' => '┘',
        b'k' => '┐',
        b'l' => '┌',
        b'm' => '└',
        b'n' => '┼',
        b'o' => '⎺',
        b'p' => '⎻',
        b'q' => '─',
        b'r' => '⎼',
        b's' => '⎽',
        b't' => '├',
        b'u' => '┤',
        b'v' => '┴',
        b'w' => '┬',
        b'x' => '│',
        b'y' => '≤',
        b'z' => '≥',
        b'{' => 'π',
        b'|' => '≠',
        b'}' => '£',
        b'~' => '·',
        _ => {
            out.push(byte);
            return;
        }
    };
    let mut buf = [0u8; 4];
    out.extend_from_slice(mapped.encode_utf8(&mut buf).as_bytes());
}

fn hold_error_popup(
    overlay: &Arc<Mutex<OverlayState>>, term: &mut Terminal<CrosstermBackend<io::Stdout>>, screen: &vt100::Screen, host: &HostUiState,
    catalog: &BuildTargetCatalog, active: &ActiveBuildTarget,
) {
    let is_error = overlay.lock().unwrap().has_error;
    if !is_error {
        return;
    }

    {
        let mut state = overlay.lock().unwrap();
        state.popup.show_info(Some("Error".to_string()), "Press any key to exit", Some(palette::FG), Some(palette::ACCENT));
    }
    term.draw(|f| render_frame(f, screen, &overlay.lock().unwrap(), host, catalog, active)).ok();
    let _ = event::read();
}

fn screen_contains(screen: &vt100::Screen, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let (rows, cols) = screen.size();
    let mut buf = String::with_capacity((rows * cols) as usize);
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    continue;
                }
                buf.push_str(cell.contents());
            }
        }
    }
    buf.contains(pattern)
}

fn screen_has_ascii_alphanumeric(screen: &vt100::Screen) -> bool {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.contents().chars().any(|c| c.is_ascii_alphanumeric()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Renders PTY output through ratatui until the child process exits.
///
/// Forwards real keystrokes to the PTY master, parses terminal output through
/// [`vt100::Parser`], and draws each frame with full 24-bit color plus a
/// floating status overlay in the top-right corner.
///
/// `status_fd` is polled continuously for newline-delimited UI messages.
///
/// `overlay` is shared with the VSOCK status listener so VM-originated
/// UI commands can update popups, progress bars, and status text.
#[allow(clippy::too_many_arguments)]
pub fn event_loop(
    master_fd: RawFd, rows: u16, cols: u16, status_fd: RawFd, startup_status_fd: RawFd, overlay: Arc<Mutex<OverlayState>>,
    catalog: BuildTargetCatalog, active: ActiveBuildTarget,
) -> Result<(), String> {
    let stdin_fd = io::stdin().as_raw_fd();

    let mut stdout = io::stdout();
    terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    stdout.execute(EnterAlternateScreen).map_err(|e| format!("alt screen: {e}"))?;
    stdout.execute(cursor::Show).map_err(|e| format!("cursor: {e}"))?;

    let mut term = Term::new(guest_rows(rows), cols);

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("terminal: {e}"))?;

    let mut pty_buf = [0u8; 4096];

    let mut last_rows = rows;
    let mut last_cols = cols;
    let mut host = HostUiState::new(&catalog);

    let mut status_buf = Vec::new();
    let mut startup_status_buf = Vec::new();
    let mut startup_status_fd = startup_status_fd;
    let mut mouse_capture_enabled = false;

    unsafe {
        libc::signal(libc::SIGWINCH, handle_sigwinch as *const () as libc::sighandler_t);
    }

    loop {
        if RESIZED.swap(false, Ordering::SeqCst) {
            if let Ok((new_cols, new_rows)) = terminal::size() {
                if new_cols != last_cols || new_rows != last_rows {
                    last_cols = new_cols;
                    last_rows = new_rows;
                    term.set_size(guest_rows(new_rows), new_cols);
                    let ws = libc::winsize { ws_row: guest_rows(new_rows), ws_col: new_cols, ws_xpixel: 0, ws_ypixel: 0 };
                    unsafe {
                        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
                    }
                }
            }
        }

        let mut fds = [
            libc::pollfd { fd: master_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stdin_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: status_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: startup_status_fd, events: libc::POLLIN, revents: 0 },
        ];

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 4, 16) };

        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            cleanup_terminal(&mut terminal, mouse_capture_enabled);
            return Err(format!("poll: {err}"));
        }

        let mut pty_output = false;

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let n = unsafe { libc::read(master_fd, pty_buf.as_mut_ptr() as *mut libc::c_void, pty_buf.len()) };
            if n > 0 {
                term.process(&pty_buf[..n as usize]);
                let wants_mouse_capture = term.mouse_tracking() != MouseTracking::Off;
                if wants_mouse_capture != mouse_capture_enabled {
                    let result = if wants_mouse_capture {
                        terminal.backend_mut().execute(EnableMouseCapture).map(|_| ())
                    } else {
                        terminal.backend_mut().execute(DisableMouseCapture).map(|_| ())
                    };
                    if let Err(err) = result {
                        cleanup_terminal(&mut terminal, mouse_capture_enabled);
                        return Err(format!("mouse capture: {err}"));
                    }
                    mouse_capture_enabled = wants_mouse_capture;
                }
                for response in term.drain_responses() {
                    unsafe {
                        libc::write(master_fd, response.as_ptr() as *const libc::c_void, response.len());
                    }
                }
                pty_output = true;
            } else {
                hold_error_popup(&overlay, &mut terminal, term.screen(), &host, &catalog, &active);
                break;
            }
        }

        if fds[2].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            read_status_messages(status_fd, &mut status_buf, &overlay);
        }
        if startup_status_fd >= 0
            && fds[3].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && read_status_messages(startup_status_fd, &mut startup_status_buf, &overlay) == 0
        {
            startup_status_fd = -1;
        }

        if fds[1].revents & libc::POLLIN != 0 {
            if let Ok(input_event) = event::read() {
                match input_event {
                    Event::Key(key) => {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }

                        let is_password = overlay.lock().is_ok_and(|s| matches!(s.popup.content, popup::PopupContent::Password { .. }));
                        if is_password {
                            handle_password_key(&overlay, status_fd, key);
                        } else if host.handle_key(key, &catalog, &active) {
                            continue;
                        } else if let Some(bytes) = key_to_bytes(&key, term.application_cursor_keys()) {
                            unsafe {
                                libc::write(master_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        if mouse.row >= guest_rows(last_rows) {
                            continue;
                        }
                        if let Some(bytes) = mouse_to_bytes(mouse, term.mouse_tracking(), term.mouse_encoding()) {
                            unsafe {
                                libc::write(master_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        {
            let mut state = overlay.lock().unwrap();
            let now = Instant::now();
            host.refresh_free_space(&catalog);

            if pty_output {
                for action in &mut state.pending {
                    if action.first_pty_at.is_none() && action.triggers.iter().any(|t| matches!(t, Trigger::OnPty | Trigger::DelayMsAfterPty(_))) {
                        action.first_pty_at = Some(now);
                    }
                }
            }

            if (now - state.last_content_scan).as_millis() >= 100 {
                state.last_content_scan = now;
                if state.hide_on_ascii && screen_has_ascii_alphanumeric(term.screen()) {
                    state.popup.hide();
                    state.hide_on_ascii = false;
                }
                if let Some(ref pat) = state.hide_on_content.clone() {
                    if screen_contains(term.screen(), pat) {
                        state.popup.hide();
                        state.hide_on_content = None;
                    }
                }
            }

            let mut i = 0;
            while i < state.pending.len() {
                let fire = state.pending[i].triggers.iter().any(|t| match t {
                    Trigger::OnPty => pty_output,
                    Trigger::DelayMs(d) => (now - state.pending[i].enqueued_at).as_millis() as u64 >= *d,
                    Trigger::DelayMsAfterPty(d) => {
                        if let Some(first) = state.pending[i].first_pty_at {
                            (now - first).as_millis() as u64 >= *d
                        } else {
                            false
                        }
                    }
                });
                if fire {
                    let action = state.pending.remove(i);
                    dispatch_ui_command(&mut state, &action.widget, &action.command, "", &action.value);
                } else {
                    i += 1;
                }
            }

            state.popup.tick();
            if let Some(toast) = state.error_toast.as_ref() {
                let lifetime = ERROR_TOAST_IN + ERROR_TOAST_HOLD + ERROR_TOAST_OUT;
                if toast.shown_at.elapsed() >= lifetime {
                    state.error_toast = None;
                }
            }

            if let Err(err) = terminal.draw(|f| render_frame(f, term.screen(), &state, &host, &catalog, &active)) {
                cleanup_terminal(&mut terminal, mouse_capture_enabled);
                return Err(format!("draw: {err}"));
            }
        }
    }

    cleanup_terminal(&mut terminal, mouse_capture_enabled);

    Ok(())
}

fn read_status_messages(fd: RawFd, buffer: &mut Vec<u8>, overlay: &Arc<Mutex<OverlayState>>) -> isize {
    let mut chunk = [0u8; 256];
    let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
    if n <= 0 {
        return 0;
    }
    process_status_bytes(buffer, &chunk[..n as usize], overlay);
    n
}

fn process_status_bytes(buffer: &mut Vec<u8>, bytes: &[u8], overlay: &Arc<Mutex<OverlayState>>) {
    buffer.extend_from_slice(bytes);
    while let Some(pos) = buffer.iter().position(|&byte| byte == b'\n') {
        let line = String::from_utf8_lossy(&buffer[..pos]).into_owned();
        buffer.drain(..=pos);
        if let Some(command) = line.strip_prefix('@') {
            if let Some((widget, command, options, value)) = vscomm::decode_ui_payload(command.as_bytes()) {
                let mut state = overlay.lock().unwrap();
                dispatch_ui_command(&mut state, widget, command, options, value);
            }
        } else {
            let mut state = overlay.lock().unwrap();
            let title = state.popup_title.clone();
            state.popup.show_info(title, &line, Some(palette::FG), Some(palette::ACCENT));
        }
    }
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, mouse_capture_enabled: bool) {
    if mouse_capture_enabled {
        terminal.backend_mut().execute(DisableMouseCapture).ok();
    }
    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();
    unsafe {
        libc::signal(libc::SIGWINCH, libc::SIG_DFL);
    }
}

fn handle_password_key(overlay: &Arc<Mutex<OverlayState>>, status_fd: RawFd, key: KeyEvent) {
    let mut state = overlay.lock().unwrap();
    if key.code == KeyCode::Enter {
        if let Some(password) = state.popup.password_value() {
            let mut response = password.into_bytes();
            response.push(b'\n');
            state.popup.hide();
            unsafe {
                libc::write(status_fd, response.as_ptr() as *const libc::c_void, response.len());
            }
        }
    } else {
        state.popup.handle_password_key(&key);
    }
}

fn key_to_bytes(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    match key.code {
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                if c.is_ascii_alphabetic() {
                    Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f])
                } else {
                    None
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut v = vec![0x1b];
                let mut buf = [0u8; 4];
                let len = c.encode_utf8(&mut buf).len();
                v.extend_from_slice(&buf[..len]);
                Some(v)
            } else {
                let mut buf = [0u8; 4];
                let len = c.encode_utf8(&mut buf).len();
                Some(buf[..len].to_vec())
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(if app_cursor { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() }),
        KeyCode::Down => Some(if app_cursor { b"\x1bOB".to_vec() } else { b"\x1b[B".to_vec() }),
        KeyCode::Right => Some(if app_cursor { b"\x1bOC".to_vec() } else { b"\x1b[C".to_vec() }),
        KeyCode::Left => Some(if app_cursor { b"\x1bOD".to_vec() } else { b"\x1b[D".to_vec() }),
        KeyCode::Home => Some(if app_cursor { b"\x1bOH".to_vec() } else { b"\x1b[H".to_vec() }),
        KeyCode::End => Some(if app_cursor { b"\x1bOF".to_vec() } else { b"\x1b[F".to_vec() }),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => fn_key(n),
        _ => None,
    }
}

fn mouse_to_bytes(event: MouseEvent, tracking: MouseTracking, encoding: MouseEncoding) -> Option<Vec<u8>> {
    let (base_code, is_drag, is_release) = match event.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), false, false),
        MouseEventKind::Up(button) => (mouse_button_code(button), false, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button), true, false),
        MouseEventKind::Moved => (3, true, false),
        MouseEventKind::ScrollUp => (64, false, false),
        MouseEventKind::ScrollDown => (65, false, false),
        MouseEventKind::ScrollLeft => (66, false, false),
        MouseEventKind::ScrollRight => (67, false, false),
    };

    match (tracking, event.kind) {
        (MouseTracking::Off, _) => return None,
        (MouseTracking::Normal, MouseEventKind::Drag(_) | MouseEventKind::Moved) => return None,
        (MouseTracking::Button, MouseEventKind::Moved) => return None,
        _ => {}
    }

    let mut code = base_code;
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    if is_drag {
        code += 32;
    }

    let column = u32::from(event.column) + 1;
    let row = u32::from(event.row) + 1;

    match encoding {
        MouseEncoding::Sgr => {
            let suffix = if is_release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", code, column, row, suffix).into_bytes())
        }
        MouseEncoding::Urxvt => {
            let legacy_code = if is_release { 3 } else { code };
            Some(format!("\x1b[{};{};{}M", legacy_code + 32, column, row).into_bytes())
        }
        MouseEncoding::X10 => {
            let legacy_code = if is_release { 3 } else { code };
            if column > 223 || row > 223 || legacy_code + 32 > 255 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', (legacy_code + 32) as u8, (column + 32) as u8, (row + 32) as u8])
        }
    }
}

fn mouse_button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn fn_key(n: u8) -> Option<Vec<u8>> {
    match n {
        1 => Some(b"\x1bOP".to_vec()),
        2 => Some(b"\x1bOQ".to_vec()),
        3 => Some(b"\x1bOR".to_vec()),
        4 => Some(b"\x1bOS".to_vec()),
        5 => Some(b"\x1b[15~".to_vec()),
        6 => Some(b"\x1b[17~".to_vec()),
        7 => Some(b"\x1b[18~".to_vec()),
        8 => Some(b"\x1b[19~".to_vec()),
        9 => Some(b"\x1b[20~".to_vec()),
        10 => Some(b"\x1b[21~".to_vec()),
        11 => Some(b"\x1b[23~".to_vec()),
        12 => Some(b"\x1b[24~".to_vec()),
        _ => None,
    }
}

/// Converts a [`vt100::Color`] to a [`ratatui::style::Color`], preserving
/// 24-bit RGB, 256-color indexed palette, and terminal default.
fn to_ratatui_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn local_free_space(path: &std::path::Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = unsafe { std::mem::zeroed::<libc::statvfs>() };
    let result = unsafe { libc::statvfs(path.as_ptr(), &mut stats) };
    if result != 0 {
        return None;
    }
    stats.f_bavail.checked_mul(stats.f_frsize)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + area.width.saturating_sub(width) / 2, y: area.y + area.height.saturating_sub(height) / 2, width, height }
}

fn render_status_bar(area: Rect, buf: &mut Buffer, host: &HostUiState, active: &ActiveBuildTarget) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let target = active.current();
    let free = host.free_bytes.map(format_bytes).unwrap_or_else(|| "unknown".to_string());
    let target_segment = host
        .confirmation
        .as_ref()
        .filter(|(_, shown_at)| shown_at.elapsed() < Duration::from_secs(3))
        .map_or_else(|| format!("Target: {target}"), |(message, _)| format!("Target: {target} ({message})"));
    let mut segments = vec![
        target_segment,
        "Ctrl+Alt+B Targets".to_string(),
        "Ctrl+Alt+S Setup".to_string(),
        "Ctrl+Alt+H Help".to_string(),
        format!("Local workspace free: {free}"),
    ];
    while segments.len() > 1 {
        let text = format!(" {}", segments.join(" | "));
        if text.chars().count() <= usize::from(area.width) {
            break;
        }
        segments.pop();
    }
    let mut text = format!(" {}", segments.join(" | "));
    if text.chars().count() > usize::from(area.width) {
        text = text.chars().take(usize::from(area.width)).collect();
    }
    Paragraph::new(text).style(Style::default().fg(palette::FG).bg(palette::BG_1)).render(area, buf);
}

fn render_host_popup(area: Rect, buf: &mut Buffer, host: &HostUiState, catalog: &BuildTargetCatalog) {
    let (title, lines, height) = match host.popup {
        HostPopup::None => return,
        HostPopup::Help => (
            "Bunkerbox Help".to_string(),
            vec![
                Line::from(Span::styled("Ctrl-Alt-B", Style::default().fg(palette::ACCENT))),
                Line::from("Select Build Target"),
                Line::from(Span::styled("Ctrl-Alt-S", Style::default().fg(palette::ACCENT))),
                Line::from("Remote Setup (next run)"),
                Line::from(Span::styled("Ctrl-Alt-H", Style::default().fg(palette::ACCENT))),
                Line::from("Show this help"),
                Line::from(Span::styled("Esc", Style::default().fg(palette::ACCENT))),
                Line::from("Close host popup"),
            ],
            12,
        ),
        HostPopup::Targets => {
            let mut lines = Vec::new();
            for (index, target) in catalog.summaries().iter().enumerate() {
                let marker = if index == host.target_index { "> " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(palette::ACCENT)),
                    Span::styled(target.label(), Style::default().fg(palette::FG)),
                    Span::styled(format!(": {}", target.workspace()), Style::default().fg(palette::MUTED)),
                ]));
            }
            let height = (lines.len() as u16 + 4).max(5);
            ("Build Targets".to_string(), lines, height)
        }
        HostPopup::Setup => render_setup_popup(host),
        HostPopup::ConfigError => render_config_error_popup(host),
    };
    let popup_area = centered_rect(area, 64, height);
    if popup_area.width < 4 || popup_area.height < 3 {
        return;
    }
    Clear.render(popup_area, buf);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::ACCENT))
        .style(Style::default().fg(palette::FG).bg(palette::POPUP_BG))
        .padding(Padding::horizontal(1));
    Paragraph::new(lines).block(block).render(popup_area, buf);
}

fn render_config_error_popup(host: &HostUiState) -> (String, Vec<Line<'static>>, u16) {
    let Some(error) = &host.config_error else {
        return ("Remote Configuration Error".to_string(), vec![Line::from("No configuration error available")], 7);
    };
    if error.view {
        (
            "Remote Configuration Error".to_string(),
            vec![Line::from("The existing remote.conf was not changed."), Line::from(error.message.clone()), Line::from("Esc Cancel")],
            9,
        )
    } else {
        (
            "Remote Configuration Error".to_string(),
            vec![Line::from("Remote Setup cannot edit this file."), Line::from("Press V to view the error."), Line::from("Esc Cancel")],
            8,
        )
    }
}

fn render_setup_popup(host: &HostUiState) -> (String, Vec<Line<'static>>, u16) {
    let Some(setup) = &host.setup else {
        return ("Remote Setup".to_string(), vec![Line::from("No setup state available")], 7);
    };
    let mut lines = Vec::new();
    match setup.screen {
        SetupScreen::List => {
            lines.push(Line::from("A Add   Enter/E Edit   D Delete   S Save   Esc Cancel"));
            lines.push(Line::from("Targets are applied on the next Bunkerbox run."));
            for (index, label) in setup.labels().iter().enumerate() {
                let marker = if index == setup.selected { "> " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(palette::ACCENT)),
                    Span::styled(label.clone(), Style::default().fg(palette::FG)),
                ]));
            }
            if setup.draft.targets.is_empty() {
                lines.push(Line::from(Span::styled("(empty; press A to add a target)", Style::default().fg(palette::MUTED))));
            }
            setup_error_line(setup, &mut lines);
            let height = (lines.len() as u16 + 4).max(8);
            ("Remote Setup".to_string(), lines, height)
        }
        SetupScreen::TargetForm => {
            if let Some(form) = &setup.target_form {
                lines.push(text_field_line("Label", &form.label, form.focus == TargetFormFocus::Label));
                lines.push(text_field_line("SSH", &form.ssh, form.focus == TargetFormFocus::Ssh));
                lines.push(text_field_line("Workspace", &form.workspace, form.focus == TargetFormFocus::Workspace));
                lines.push(action_line("Project Overrides...", form.focus == TargetFormFocus::Overrides));
                lines.push(action_line("Resources...", form.focus == TargetFormFocus::Resources));
                lines.push(action_line("Save", form.focus == TargetFormFocus::Save));
                lines.push(action_line("Cancel", form.focus == TargetFormFocus::Cancel));
            }
            setup_error_line(setup, &mut lines);
            let height = (lines.len() as u16 + 4).max(10);
            ("Remote Target".to_string(), lines, height)
        }
        SetupScreen::Overrides => {
            lines.push(Line::from("Tab/Up/Down select   Enter edit   Esc back"));
            for (focus, label) in [
                (OverrideFocus::Tools, "Tools..."),
                (OverrideFocus::Environment, "Environment names..."),
                (OverrideFocus::Exclusions, "Snapshot exclusions..."),
                (OverrideFocus::Artifacts, "Artifact paths..."),
                (OverrideFocus::Done, "Done"),
            ] {
                lines.push(action_line(label, setup.override_focus == focus));
            }
            setup_error_line(setup, &mut lines);
            ("Project Remote Overrides".to_string(), lines, 12)
        }
        SetupScreen::Tools => {
            lines.push(Line::from("A Add   Enter/E Edit   D Delete   Esc Done"));
            if let Some(tools) = &setup.tools {
                for (index, tool) in tools.entries.iter().enumerate() {
                    let marker = if index == tools.selected { "> " } else { "  " };
                    let command = tool.command.as_deref().unwrap_or("(logical name)");
                    lines.push(Line::from(format!("{marker}{} -> {}{}", tool.name, command, if tool.allow_args { " [args]" } else { "" })));
                }
                if let Some(error) = &tools.error {
                    lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(palette::ERROR))));
                }
            }
            setup_error_line(setup, &mut lines);
            let height = (lines.len() as u16 + 4).max(8);
            ("Remote Tools".to_string(), lines, height)
        }
        SetupScreen::ToolForm => {
            if let Some(form) = &setup.tool_form {
                lines.push(text_field_line("Logical name", &form.name, form.focus == 0));
                lines.push(text_field_line("Command basename", &form.command, form.focus == 1));
                lines.push(action_line(&format!("Allow args: {}", if form.allow_args { "yes" } else { "no" }), form.focus == 2));
                lines.push(action_line("Save", form.focus == 3));
                lines.push(action_line("Cancel", form.focus == 4));
            }
            setup_error_line(setup, &mut lines);
            ("Remote Tool".to_string(), lines, 11)
        }
        SetupScreen::Environment | SetupScreen::Exclusions | SetupScreen::Artifacts => {
            if let Some(state) = &setup.strings {
                lines.push(Line::from("A Add   Enter/E Edit   D Delete   Esc Done"));
                for (index, entry) in state.entries.iter().enumerate() {
                    let marker = if index == state.selected { "> " } else { "  " };
                    lines.push(Line::from(format!("{marker}{entry}")));
                }
                if let Some(editing) = &state.editing {
                    lines.push(text_field_line("Value", editing, true));
                    lines.push(Line::from("Enter accept   Esc cancel"));
                }
                if let Some(error) = &state.error {
                    lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(palette::ERROR))));
                }
                setup_error_line(setup, &mut lines);
                let height = (lines.len() as u16 + 4).max(8);
                (state.kind.title().to_string(), lines, height)
            } else {
                ("Remote List".to_string(), vec![Line::from("No list state available")], 7)
            }
        }
        SetupScreen::Resources | SetupScreen::AdvancedResources => {
            if let Some(resources) = &setup.resources {
                lines.push(Line::from("Blank unset   Tab/Up/Down navigate   A or Page toggles core/advanced"));
                for index in resources.visible_indices() {
                    let input = &resources.inputs[index];
                    lines.push(text_field_line(
                        input.label,
                        &input.value,
                        resources.focus == resources.visible_indices().iter().position(|candidate| *candidate == index).unwrap_or(0),
                    ));
                }
                lines.push(action_line("Use defaults (clear overrides)", resources.focus == resources.action_index(0)));
                lines.push(action_line("Save", resources.focus == resources.action_index(1)));
                lines.push(action_line("Cancel", resources.focus == resources.action_index(2)));
                setup_error_line(setup, &mut lines);
                let height = (lines.len() as u16 + 4).max(10);
                (if resources.advanced { "Advanced Resources".to_string() } else { "Core Resources".to_string() }, lines, height)
            } else {
                ("Resources".to_string(), vec![Line::from("No resource state available")], 7)
            }
        }
        SetupScreen::ConfirmDelete => {
            let label = setup.delete_label.as_deref().unwrap_or("<unknown>");
            (
                "Delete Remote Target".to_string(),
                vec![Line::from(format!("Delete target \"{label}\"?")), Line::from("Enter Delete"), Line::from("Esc Cancel")],
                8,
            )
        }
    }
}

fn setup_error_line(setup: &RemoteSetupState, lines: &mut Vec<Line<'static>>) {
    if let Some(error) = &setup.error {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(palette::ERROR))));
    }
}

fn text_field_line(label: &str, field: &TextField, focused: bool) -> Line<'static> {
    let marker = if focused { "> " } else { "  " };
    let value = text_field_value(field, focused);
    Line::from(vec![
        Span::styled(marker, Style::default().fg(palette::ACCENT)),
        Span::styled(format!("{label}: "), Style::default().fg(palette::MUTED)),
        Span::styled(value, Style::default().fg(palette::FG)),
    ])
}

fn action_line(label: &str, focused: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(if focused { "> " } else { "  " }, Style::default().fg(palette::ACCENT)),
        Span::styled(label.to_string(), Style::default().fg(if focused { palette::FG } else { palette::MUTED })),
    ])
}

fn text_field_value(field: &TextField, focused: bool) -> String {
    if !focused {
        return field.value.clone();
    }
    let mut value = field.value.clone();
    let index = field.byte_index(field.cursor);
    value.insert(index, '|');
    value
}

/// Renders one frame: writes the guest vt100 screen into the reduced guest
/// viewport, then draws host-owned status and popup controls.
fn render_frame(
    f: &mut Frame, screen: &vt100::Screen, overlay: &OverlayState, host: &HostUiState, catalog: &BuildTargetCatalog, active: &ActiveBuildTarget,
) {
    let area = f.area();
    let guest_area = Rect { height: area.height.saturating_sub(1), ..area };
    let (rows, cols) = screen.size();
    let max_rows = guest_area.height.min(rows);
    let max_cols = guest_area.width.min(cols);
    let buf = f.buffer_mut();

    for row in 0..max_rows {
        let mut col: u16 = 0;
        while col < max_cols {
            let x = guest_area.x + col;
            let y = guest_area.y + row;

            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }

                let mut style = Style::default().fg(to_ratatui_color(cell.fgcolor())).bg(to_ratatui_color(cell.bgcolor()));

                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.dim() {
                    style = style.add_modifier(Modifier::DIM);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let ch = cell.contents();
                let display: &str = if ch.is_empty() { " " } else { ch };

                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_symbol(display);
                    c.set_style(style);
                }

                if cell.is_wide() {
                    if col + 1 < max_cols {
                        if let Some(c) = buf.cell_mut((x + 1, y)) {
                            c.set_symbol(" ");
                            c.set_style(style);
                        }
                    }
                    col += 2;
                } else {
                    col += 1;
                }
            } else {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_symbol(" ");
                    c.set_style(Style::default());
                }
                col += 1;
            }
        }
    }

    {
        let buf = f.buffer_mut();
        overlay.popup.render(guest_area, buf);
        render_error_toast(guest_area, buf, overlay.error_toast.as_ref());
        render_status_bar(Rect { y: area.bottom().saturating_sub(1), height: area.height.min(1), ..area }, buf, host, active);
        render_host_popup(area, buf, host, catalog);
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_row < max_rows && cursor_col < max_cols {
        f.set_cursor_position((guest_area.x + cursor_col, guest_area.y + cursor_row));
    }
}

fn render_error_toast(area: Rect, buf: &mut Buffer, toast: Option<&ErrorToast>) {
    let Some(toast) = toast else {
        return;
    };

    let elapsed = toast.shown_at.elapsed();
    let width = toast
        .message
        .lines()
        .chain(std::iter::once(toast.title.as_str()))
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(24)
        .saturating_add(8)
        .max(28)
        .min(area.width.saturating_sub(2));
    let height = (toast.message.lines().count().max(1) as u16 + 4).min(area.height.saturating_sub(2));
    if width < 4 || height < 3 {
        return;
    }

    let travel = width.saturating_add(2);
    let target_x = area.right().saturating_sub(travel);
    let offset = if elapsed < ERROR_TOAST_IN {
        let progress = elapsed.as_secs_f64() / ERROR_TOAST_IN.as_secs_f64();
        ((1.0 - progress) * f64::from(travel)) as u16
    } else if elapsed < ERROR_TOAST_IN + ERROR_TOAST_HOLD {
        0
    } else {
        let out_elapsed = elapsed - ERROR_TOAST_IN - ERROR_TOAST_HOLD;
        let progress = (out_elapsed.as_secs_f64() / ERROR_TOAST_OUT.as_secs_f64()).min(1.0);
        (progress * f64::from(travel)) as u16
    };
    let x = target_x.saturating_add(offset);
    let y = area.y.saturating_add(1);
    let canvas = Rect { x, y, width, height };

    Clear.render(canvas, buf);
    let block = Block::default()
        .title(toast.title.as_str())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette::ERROR))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(palette::BG_1));
    let inner = block.inner(canvas);
    block.render(canvas, buf);
    Paragraph::new(toast.message.as_str()).style(Style::default().fg(palette::FG)).wrap(Wrap { trim: true }).render(inner, buf);
}

#[cfg(test)]
#[path = "tui_ut.rs"]
mod tui_tests;
