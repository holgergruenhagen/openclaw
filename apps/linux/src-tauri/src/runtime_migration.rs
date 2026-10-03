//! The companion owns its marked launcher; the CLI owns updates and service mutations.
use crate::cli::{output_tail, OpenClawCli};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const MARKER: &str = "# OpenClaw-Tauri runtime v1 ";
const CHANGED: &str = "The Gateway or CLI changed ownership. Its current selection was preserved; inspect it before retrying.";
pub(crate) const INTERRUPTED: &str = "The runtime transition was interrupted; this app is not managing it until you choose Use bundled runtime or Restore previous Node runtime.";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct BundledRuntime {
    pub bun: PathBuf,
    pub sqlite: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Fresh,
    Adopt,
    OwnedUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MigrationOutcome {
    Migrated,
    Current,
    DeferredPaused,
    PreservedExternal,
}

/// Startup must not execute or adopt an unmarked CLI just to discover app ownership.
pub(crate) fn has_gateway_runtime_record(cli: &OpenClawCli) -> Result<bool, String> {
    let Some(path) = cli.managed_wrapper() else {
        return Ok(false);
    };
    // npm links and other executable contracts can live at the managed lookup path.
    // Their presence permits attachment, not app ownership of the link or its target.
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= 65536 => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Could not inspect CLI ownership: {error}")),
    }
    let bytes = read_regular(&path)?;
    if !std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.lines().nth(1))
        .is_some_and(|line| line.starts_with(MARKER))
    {
        return Ok(false);
    }
    Ok(read_wrapper(cli)?
        .managed
        .is_some_and(|metadata| metadata.purpose != Purpose::Browser))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Target {
    runtime: PathBuf,
    entry: PathBuf,
    sqlite: Option<PathBuf>,
    bun: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Binding {
    revision: String,
    pin_revision: String,
    config_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Managed {
    purpose: Purpose,
    target: Target,
    node: Target,
    retained_wrapper: PathBuf,
    retained_wrapper_sha256: String,
    package_version: String,
    binding: Option<Binding>,
    pending: Option<Pending>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Pending {
    original: Binding,
    original_wrapper: PathBuf,
    original_wrapper_sha256: String,
    mode: Mode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Purpose {
    Gateway,
    Browser,
    External,
}

impl Managed {
    fn owns(&self, state: &Snapshot) -> bool {
        self.purpose == Purpose::Gateway
            && self.pending.is_none()
            && self.binding.as_ref().is_some_and(|binding| {
                state.binding().is_ok_and(|current| {
                    binding.pin_revision == current.pin_revision
                        && binding.config_path == current.config_path
                }) && state.matches_bun(&self.target)
            })
    }
}

#[derive(Clone)]
struct Wrapper {
    path: PathBuf,
    bytes: Vec<u8>,
    node: Target,
    managed: Option<Managed>,
}

impl Wrapper {
    fn inspection_target(&self) -> Option<&Target> {
        self.managed
            .as_ref()
            .filter(|metadata| metadata.pending.is_some() || metadata.purpose == Purpose::External)
            .map(|_| &self.node)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    loaded: Option<bool>,
    target_role: Option<String>,
    command: Option<ServiceCommand>,
    runtime: Option<ServiceRuntime>,
    runtime_intent: Option<RuntimeIntent>,
    revision: Option<String>,
    definition_mutation: Option<String>,
    #[serde(default)]
    launcher_overridden: bool,
    layout: Option<ServiceLayout>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct ServiceCommand {
    program_arguments: Vec<String>,
    #[serde(flatten)]
    other: std::collections::BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct ServiceRuntime {
    status: String,
    pid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
struct ServicePort {
    port: u16,
    status: String,
    listeners: Vec<PortListener>,
}

#[derive(Clone, Debug, Deserialize)]
struct PortListener {
    pid: Option<u32>,
    ppid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceLayout {
    entrypoint_real: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
struct RuntimeIntent {
    status: String,
    revision: Option<String>,
    definition: Option<String>,
    stored: Option<bool>,
    pin: Option<RuntimePin>,
}

#[derive(Clone, Debug, Deserialize)]
struct RuntimePin {
    runtime: String,
    path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
struct Snapshot {
    cli: Option<CliStatus>,
    service: Service,
    gateway: Value,
    config: Value,
    rpc: Option<Value>,
    port: Option<ServicePort>,
}

#[derive(Clone, Debug, Deserialize)]
struct CliStatus {
    version: String,
    runtime: Option<CliRuntime>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliRuntime {
    kind: String,
    exec_path: PathBuf,
    supported: bool,
}

fn require_retained_node(state: &Snapshot, node: &Target, version: &str) -> Result<(), String> {
    if !node.bun
        && state.cli.as_ref().is_some_and(|cli| cli.version == version)
        && state.uses_runtime(node)
    {
        return Ok(());
    }
    Err("The retained Node runtime could not qualify for this package version. Repair the managed Node installation before retrying.".into())
}

impl Snapshot {
    fn uses_runtime(&self, target: &Target) -> bool {
        self.cli
            .as_ref()
            .and_then(|cli| cli.runtime.as_ref())
            .is_some_and(|runtime| {
                runtime.kind == (if target.bun { "bun" } else { "node" })
                    && runtime.supported
                    && fs::canonicalize(&runtime.exec_path).ok().as_ref() == Some(&target.runtime)
            })
    }

    fn healthy_for(&self, target: &Target) -> bool {
        let Some(runtime) = self
            .service
            .runtime
            .as_ref()
            .filter(|runtime| runtime.status == "running")
        else {
            return false;
        };
        let Some(pid) = runtime.pid.filter(|pid| *pid > 0) else {
            return false;
        };
        self.service.loaded == Some(true)
            && self.service.target_role.as_deref() == Some("target")
            && self.uses_runtime(target)
            && self.matches_target(target)
            && self.port.as_ref().is_some_and(|port| {
                port.status == "busy" && port.port > 0
                    && self.gateway.get("port").and_then(Value::as_u64) == Some(u64::from(port.port))
                    && !port.listeners.is_empty()
                    // Consume the CLI's PID/PPID evidence using its restart-port-ownership contract.
                    && port.listeners.iter().all(|listener| listener.pid == Some(pid) || listener.ppid == Some(pid))
            })
            && self.rpc.as_ref().and_then(|rpc| rpc.get("ok")).and_then(Value::as_bool) == Some(true)
    }

    fn binding(&self) -> Result<Binding, String> {
        let intent = self.service.runtime_intent.as_ref().ok_or(
            "Update the installed CLI to this app's version before selecting the bundled runtime.",
        )?;
        if intent.status != "known"
            || self.service.definition_mutation.as_deref() != Some("writable")
            || self.service.loaded.is_none()
            || self.service.launcher_overridden
            || self.config.get("mismatch").and_then(Value::as_bool) == Some(true)
        {
            return Err("Gateway runtime intent or service ownership could not be verified. The current service was preserved.".into());
        }
        Ok(Binding {
            revision: self.service.revision.clone().ok_or(CHANGED)?,
            pin_revision: intent.revision.clone().ok_or(CHANGED)?,
            config_path: self
                .config
                .pointer("/daemon/path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or(CHANGED)?,
        })
    }

    fn paused(&self) -> bool {
        self.service.command.is_some()
            && (self.service.loaded == Some(false)
                || self
                    .service
                    .runtime
                    .as_ref()
                    .map(|value| value.status.as_str())
                    == Some("stopped"))
    }

    fn absent(&self) -> bool {
        self.service.loaded == Some(false) && self.service.command.is_none()
    }

    fn matches_target(&self, target: &Target) -> bool {
        let Some(command) = &self.service.command else {
            return false;
        };
        command
            .program_arguments
            .first()
            .and_then(|path| fs::canonicalize(path).ok())
            .as_ref()
            == Some(&target.runtime)
            && match self
                .service
                .layout
                .as_ref()
                .and_then(|layout| layout.entrypoint_real.as_ref())
            {
                Some(entry) => same_package_entry(entry, &target.entry),
                None => legacy_service_entry(&command.program_arguments)
                    .and_then(|path| fs::canonicalize(path).ok())
                    .is_some_and(|entry| same_package_entry(&entry, &target.entry)),
            }
    }

    fn matches_bun(&self, target: &Target) -> bool {
        self.binding().is_ok()
            && self.matches_target(target)
            && self
                .service
                .runtime_intent
                .as_ref()
                .and_then(|intent| intent.pin.as_ref())
                .is_some_and(|pin| pin.runtime == "bun" && pin.path == target.runtime)
    }

    fn unpinned(&self) -> bool {
        self.service.runtime_intent.as_ref().is_some_and(|intent| {
            intent.status == "known" && intent.stored == Some(false) && intent.pin.is_none()
        })
    }
}

pub(crate) fn interrupted_notice(cli: &OpenClawCli) -> Option<&'static str> {
    read_wrapper(cli)
        .ok()?
        .managed
        .filter(|metadata| metadata.purpose == Purpose::External)
        .map(|_| INTERRUPTED)
}

/// Private browser launchers carry no Gateway service authority or binding.
pub(crate) fn bind_runtime(
    cli: &OpenClawCli,
    runtime: &BundledRuntime,
    purpose: Purpose,
) -> Result<(), String> {
    let wrapper = read_wrapper(cli)?;
    if wrapper
        .managed
        .as_ref()
        .is_some_and(|metadata| metadata.binding.is_some() || metadata.pending.is_some())
    {
        return Err(
            "A service-bound CLI must be updated through its Gateway runtime owner.".into(),
        );
    }
    let target = bundled_target(runtime, &wrapper.node.entry)?;
    let version = version(cli, Some(&target))?;
    let mut metadata = managed_metadata(&wrapper, target, version, None)?;
    metadata.purpose = purpose;
    publish(&wrapper, &render(&metadata)?)
}

pub(crate) fn migrate(
    cli: &OpenClawCli,
    runtime: &BundledRuntime,
    app_version: &str,
    mode: Mode,
    is_current: &dyn Fn() -> bool,
) -> Result<MigrationOutcome, String> {
    check_current(is_current)?;
    let mut wrapper = read_wrapper(cli)?;
    if wrapper
        .managed
        .as_ref()
        .is_some_and(|metadata| metadata.pending.is_some())
    {
        let outcome = resume_pending(cli, runtime, app_version, is_current, &wrapper)?;
        return if mode == Mode::Adopt && outcome == MigrationOutcome::PreservedExternal {
            migrate(cli, runtime, app_version, mode, is_current)
        } else {
            Ok(outcome)
        };
    }
    let requested_mode = mode;
    let mode = match (mode, wrapper.managed.as_ref()) {
        (Mode::OwnedUpdate, Some(metadata))
            if metadata.purpose == Purpose::Gateway && metadata.binding.is_none() =>
        {
            Mode::Fresh
        }
        (Mode::OwnedUpdate, metadata)
            if metadata.and_then(|value| value.binding.as_ref()).is_none() =>
        {
            return Ok(MigrationOutcome::PreservedExternal);
        }
        _ => mode,
    };
    let mut state = capture(cli, wrapper.inspection_target(), false)?;
    if requested_mode == Mode::OwnedUpdate
        && !(wrapper
            .managed
            .as_ref()
            .is_some_and(|metadata| metadata.owns(&state))
            || (mode == Mode::Fresh && state.absent() && state.unpinned()))
    {
        return Ok(MigrationOutcome::PreservedExternal);
    }
    if mode == Mode::Fresh && !state.absent() {
        return Err(CHANGED.into());
    }
    if state.paused() {
        return Ok(MigrationOutcome::DeferredPaused);
    }
    if !state.absent()
        && state
            .service
            .runtime
            .as_ref()
            .map(|runtime| runtime.status.as_str())
            != Some("running")
    {
        return Err("The Gateway runtime state is unknown. Its installation was preserved; retry after checking Gateway status.".into());
    }
    // Explicit adoption authorizes the package update even with a saved pin. Pin inspection
    // still gates the later runtime switch; older published CLIs may lack that projection.
    if state.service.runtime_intent.is_some() {
        if mode == Mode::Adopt && wrapper.managed.is_none() {
            state.binding()?;
            if !state.absent() && !state.matches_target(&wrapper.node) {
                return Err(CHANGED.into());
            }
        } else {
            admit(&wrapper, &state, mode)?;
        }
    } else if mode == Mode::OwnedUpdate || (!state.absent() && !state.matches_target(&wrapper.node))
    {
        return Err(CHANGED.into());
    }
    let installed = version(cli, wrapper.inspection_target())?;
    let app_version = if crate::is_release_version(app_version) {
        app_version
    } else {
        installed.as_str()
    };
    let target = bundled_target(runtime, &wrapper.node.entry)?;
    if wrapper.managed.as_ref().is_some_and(|metadata| {
        metadata.owns(&state)
            && metadata.target == target
            && metadata.package_version == app_version
            && installed == app_version
    }) && state.matches_bun(&target)
    {
        return Ok(MigrationOutcome::Current);
    }
    let package_updated = installed != app_version;
    if package_updated {
        recheck(cli, &wrapper, &state, is_current, false)?;
        let mut command = target_command(cli, &wrapper.node)?;
        command.args(["update", "--yes", "--json", "--tag", app_version]);
        checked_output(command, "Gateway update")?;
        let unverified = |reason: String| {
            format!("The installed updater completed, but its package version could not be verified and bundled Bun was not activated: {reason}")
        };
        check_current(is_current).map_err(unverified)?;
        let observed = version(cli, Some(&wrapper.node)).map_err(unverified)?;
        if observed != app_version {
            return Err(format!("The installed updater completed, but the CLI reports {observed} instead of {app_version}. Bundled Bun was not activated."));
        }
    }
    let refusal = |reason: String| {
        if package_updated {
            format!(
                "CLI package reached {app_version}, but bundled Bun was not activated: {reason}"
            )
        } else {
            reason
        }
    };
    if package_updated {
        wrapper = read_wrapper(cli).map_err(refusal)?;
        state = capture(cli, wrapper.inspection_target(), false).map_err(refusal)?;
    }
    // Core may refresh the service's definition and pin binding while preserving runtime intent.
    admit(&wrapper, &state, mode).map_err(refusal)?;
    let qualified = capture(cli, Some(&wrapper.node), false).map_err(refusal)?;
    require_retained_node(&qualified, &wrapper.node, app_version).map_err(refusal)?;
    if qualified.binding().map_err(refusal)? != state.binding().map_err(refusal)? {
        return Err(refusal(CHANGED.into()));
    }
    state = qualified;
    if mode != Mode::Fresh {
        recheck(cli, &wrapper, &state, is_current, true).map_err(refusal)?;
        let mut command = target_command(cli, &wrapper.node).map_err(refusal)?;
        command.args(["update", "repair", "--yes", "--no-restart", "--json"]);
        checked_output(command, "Gateway update repair").map_err(refusal)?;
        check_current(is_current).map_err(refusal)?;
        state = capture(cli, Some(&wrapper.node), false).map_err(refusal)?;
        require_retained_node(&state, &wrapper.node, app_version).map_err(refusal)?;
        admit(&wrapper, &state, mode).map_err(refusal)?;
    }
    if state.paused() {
        return Ok(MigrationOutcome::DeferredPaused);
    }
    state.binding().map_err(refusal)?;
    let target = bundled_target(runtime, &wrapper.node.entry).map_err(refusal)?;
    if version(cli, Some(&target)).map_err(refusal)? != app_version {
        return Err(refusal(
            "The bundled runtime could not verify the installed app version.".into(),
        ));
    }
    let recovery = backup(
        &wrapper.path,
        "recovery",
        &serde_json::to_vec(&serde_json::json!({
            "wrapper": String::from_utf8_lossy(&wrapper.bytes),
            "service": capture_value(cli, wrapper.inspection_target(), false)?,
            "retainedNode": wrapper.node,
        }))
        .map_err(|error| error.to_string())?,
    )?;
    recheck(cli, &wrapper, &state, is_current, true)?;
    let original_binding = state.binding()?;
    let mut metadata = managed_metadata(&wrapper, target.clone(), app_version.into(), None)?;
    metadata.pending = Some(Pending {
        original: original_binding.clone(),
        original_wrapper: backup(&wrapper.path, "transition", &wrapper.bytes)?,
        original_wrapper_sha256: digest(&wrapper.bytes),
        mode,
    });
    publish(&wrapper, &render(&metadata)?)?;
    wrapper = read_wrapper(cli)?;
    // A rejected pre-install check grants no service recovery authority.
    recheck(cli, &wrapper, &state, is_current, true)?;
    let installed = install(cli, &target, &state, true);
    // The retained interpreter can inspect a failed Bun launch without depending on that Bun.
    let candidate = capture(cli, Some(&wrapper.node), false);
    let attempted_binding = candidate
        .as_ref()
        .ok()
        .filter(|value| value.matches_bun(&target))
        .and_then(|value| value.binding().ok());
    let switch = installed.and_then(|()| {
        let candidate = candidate?;
        if !candidate.matches_bun(&target) {
            return Err(CHANGED.into());
        }
        let expected = candidate.binding()?;
        wait_healthy(cli, &target, &expected, is_current)?;
        check_current(is_current)?;
        let verified = capture(cli, Some(&target), true)?;
        if verified.binding()? != expected
            || !verified.matches_bun(&target)
            || !verified.healthy_for(&target)
        {
            return Err(CHANGED.into());
        }
        let metadata =
            managed_metadata(&wrapper, target.clone(), app_version.into(), Some(expected))?;
        publish(&wrapper, &render(&metadata)?)
    });
    if let Err(error) = switch {
        let restoration = restore_after_failure(
            cli,
            &wrapper,
            &original_binding,
            &target,
            attempted_binding.as_ref(),
            app_version,
        );
        return Err(match restoration {
            Ok(()) => format!("Bundled Bun activation failed: {error} Recovery completed. Retry to use bundled Bun. Recovery backup: {}", recovery.display()),
            Err(restore) => format!("Bundled Bun activation failed: {error} Recovery needs attention: {restore} Recovery backup: {}", recovery.display()),
        });
    }
    Ok(MigrationOutcome::Migrated)
}

fn resume_pending(
    cli: &OpenClawCli,
    runtime: &BundledRuntime,
    app_version: &str,
    is_current: &dyn Fn() -> bool,
    wrapper: &Wrapper,
) -> Result<MigrationOutcome, String> {
    let metadata = wrapper.managed.as_ref().ok_or(CHANGED)?;
    let pending = metadata.pending.as_ref().ok_or(CHANGED)?;
    let state = capture(cli, Some(&wrapper.node), false);
    if state
        .as_ref()
        .ok()
        .and_then(|state| state.binding().ok())
        .as_ref()
        == Some(&pending.original)
    {
        let state = state?;
        if state.paused() {
            return Ok(MigrationOutcome::DeferredPaused);
        }
        require_retained_node(&state, &wrapper.node, &metadata.package_version)?;
        recheck(cli, wrapper, &state, is_current, true)?;
        publish(
            wrapper,
            &verified_backup(&pending.original_wrapper, &pending.original_wrapper_sha256)?,
        )?;
        return migrate(cli, runtime, app_version, pending.mode, is_current);
    }
    // Matching runtime bytes cannot prove who installed the current binding after a crash.
    // Preserve both recovery artifacts before retiring intent; do not touch the service.
    retained_bytes(wrapper)?;
    verified_backup(&pending.original_wrapper, &pending.original_wrapper_sha256)?;
    backup(&wrapper.path, "interrupted", &wrapper.bytes)?;
    check_current(is_current)?;
    let mut external = metadata.clone();
    external.purpose = Purpose::External;
    external.binding = None;
    external.pending = None;
    publish(wrapper, &render(&external)?)?;
    Ok(MigrationOutcome::PreservedExternal)
}

pub(crate) fn restore_retained_node(
    cli: &OpenClawCli,
    is_current: &dyn Fn() -> bool,
) -> Result<(), String> {
    let wrapper = read_wrapper(cli)?;
    let metadata = wrapper
        .managed
        .as_ref()
        .ok_or("This CLI is not managed by OpenClaw-Tauri.")?;
    let state = capture(cli, Some(&wrapper.node), false)?;
    if metadata.pending.is_some() || metadata.purpose == Purpose::External {
        state.binding()?;
        check_current(is_current)?;
    } else {
        admit(&wrapper, &state, Mode::OwnedUpdate)?;
    }
    if state.paused() {
        return Err(
            "The Gateway is paused. Start it before restoring the retained Node runtime.".into(),
        );
    }
    let original = retained_bytes(&wrapper)?;
    // A package update may succeed before runtime maintenance fails. Restore that current
    // package with Node; the last completed Bun activation's version may now be stale.
    let package_version = if metadata.pending.is_some() {
        metadata.package_version.clone()
    } else {
        version(cli, Some(&wrapper.node))?
    };
    require_retained_node(&state, &wrapper.node, &package_version)?;
    recheck(cli, &wrapper, &state, is_current, true)?;
    install_retained_node(cli, &wrapper.node, &state, is_current)?;
    check_current(is_current)?;
    publish(&wrapper, &original)
}

fn admit(wrapper: &Wrapper, state: &Snapshot, mode: Mode) -> Result<(), String> {
    state.binding()?;
    if mode == Mode::Fresh {
        return if state.absent()
            && state.unpinned()
            && wrapper
                .managed
                .as_ref()
                .is_none_or(|value| value.purpose == Purpose::Gateway && value.binding.is_none())
        {
            Ok(())
        } else {
            Err(CHANGED.into())
        };
    }
    if let Some(metadata) = &wrapper.managed {
        return if metadata.owns(state)
            || (mode == Mode::Adopt
                && metadata.purpose == Purpose::External
                && (state.matches_bun(&metadata.target)
                    || (state.unpinned()
                        && (state.absent() || state.matches_target(&wrapper.node)))))
        {
            Ok(())
        } else {
            Err(CHANGED.into())
        };
    }
    if mode == Mode::Adopt && !state.unpinned() {
        return Err("An existing runtime pin prevents switching to bundled Bun.".into());
    }
    if mode == Mode::Adopt && (state.absent() || state.matches_target(&wrapper.node)) {
        Ok(())
    } else {
        Err(CHANGED.into())
    }
}

fn restore_after_failure(
    cli: &OpenClawCli,
    wrapper: &Wrapper,
    original: &Binding,
    target: &Target,
    attempted: Option<&Binding>,
    expected_version: &str,
) -> Result<(), String> {
    same_wrapper(wrapper)?;
    let current = capture(cli, Some(&wrapper.node), false)?;
    let current_binding = current.binding()?;
    if &current_binding != original
        && (attempted != Some(&current_binding) || !current.matches_bun(target))
    {
        return Err(CHANGED.into());
    }
    if &current_binding == original && attempted.is_none() {
        let pending = wrapper
            .managed
            .as_ref()
            .and_then(|metadata| metadata.pending.as_ref())
            .ok_or(CHANGED)?;
        let bytes = verified_backup(&pending.original_wrapper, &pending.original_wrapper_sha256)?;
        let previous = decode_wrapper(wrapper.path.clone(), bytes)?;
        let previous_target = previous
            .managed
            .as_ref()
            .map(|metadata| &metadata.target)
            .unwrap_or(&previous.node);
        if let Ok(probed) = capture(cli, Some(previous_target), !current.absent()) {
            if probed.binding()? != *original {
                return Err(CHANGED.into());
            }
            if probed.absent() || probed.healthy_for(previous_target) {
                return publish(wrapper, &previous.bytes);
            }
        }
    }
    let before = current.binding()?;
    require_retained_node(&current, &wrapper.node, expected_version)?;
    if capture(cli, Some(&wrapper.node), false)?.binding()? != before {
        return Err(CHANGED.into());
    }
    install_retained_node(cli, &wrapper.node, &current, &|| true)?;
    if wrapper.managed.is_some() {
        publish(wrapper, &retained_bytes(wrapper)?)?;
    }
    Ok(())
}

fn install_retained_node(
    cli: &OpenClawCli,
    node: &Target,
    state: &Snapshot,
    is_current: &dyn Fn() -> bool,
) -> Result<(), String> {
    install(cli, node, state, false)?;
    let restored = capture(cli, Some(node), false)?;
    if !restored.matches_target(node) || !restored.unpinned() {
        return Err(CHANGED.into());
    }
    let binding = restored.binding()?;
    wait_healthy(cli, node, &binding, is_current)?;
    verify_healthy(cli, node, &binding)
}

fn verify_healthy(cli: &OpenClawCli, target: &Target, binding: &Binding) -> Result<(), String> {
    let verified = capture(cli, Some(target), true)?;
    if verified.binding()? != *binding || !verified.healthy_for(target) {
        return Err(CHANGED.into());
    }
    Ok(())
}

fn install(cli: &OpenClawCli, target: &Target, state: &Snapshot, bun: bool) -> Result<(), String> {
    let mut command = target_command(cli, target)?;
    command.args([
        "gateway",
        "install",
        "--force",
        "--json",
        "--runtime",
        if bun { "bun" } else { "node" },
    ]);
    if bun {
        command.arg("--runtime-path").arg(&target.runtime);
    }
    command.arg("--expected-runtime-pin").arg(
        serde_json::json!({
            "revision": state.binding()?.pin_revision,
            "definition": state.service.runtime_intent.as_ref().and_then(|intent| intent.definition.as_ref()),
        })
        .to_string(),
    );
    if let Some(port) = state.gateway.get("port").and_then(Value::as_u64) {
        command.args(["--port", &port.to_string()]);
    }
    if state.service.command.as_ref().is_some_and(|command| {
        command
            .program_arguments
            .iter()
            .any(|arg| arg == "--allow-unconfigured")
    }) {
        command.arg("--allow-unconfigured");
    }
    let output = checked_output(command, "Gateway runtime installation")?;
    let result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Gateway install returned invalid JSON.")?;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err("Gateway runtime installation did not succeed.".into());
    }
    Ok(())
}

fn wait_healthy(
    cli: &OpenClawCli,
    target: &Target,
    binding: &Binding,
    is_current: &dyn Fn() -> bool,
) -> Result<(), String> {
    // The core updater's first-run migrations can exceed the ordinary connection probe window.
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        check_current(is_current)?;
        let state = capture(cli, Some(target), true)?;
        if state.binding()? != *binding || !state.matches_target(target) {
            return Err(CHANGED.into());
        }
        if state.healthy_for(target) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "The Gateway did not become healthy within its migration startup window.".into(),
            );
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn recheck(
    cli: &OpenClawCli,
    wrapper: &Wrapper,
    expected: &Snapshot,
    is_current: &dyn Fn() -> bool,
    require_intent: bool,
) -> Result<(), String> {
    check_current(is_current)?;
    same_wrapper(wrapper)?;
    let current = capture(cli, wrapper.inspection_target(), false)?;
    if require_intent {
        if current.binding()? != expected.binding()? || current.paused() != expected.paused() {
            return Err(CHANGED.into());
        }
    } else if current.service.revision != expected.service.revision
        || current.service.command != expected.service.command
        || current.config != expected.config
        || current.gateway.get("port") != expected.gateway.get("port")
        || current.absent() != expected.absent()
        || current.paused() != expected.paused()
    {
        return Err(CHANGED.into());
    }
    same_wrapper(wrapper)?;
    check_current(is_current)
}

// Older published status has no layout projection. Accept only the ordinary generated
// Node/Bun launch shape; unfamiliar runtime flags remain with their existing owner.
fn legacy_service_entry(args: &[String]) -> Option<&str> {
    let script = args.iter().skip(1).position(|arg| {
        !matches!(arg.as_str(), "--no-install" | "--no-warnings")
            && !arg.starts_with("--max-old-space-size=")
    })? + 1;
    (args.get(script + 1).map(String::as_str) == Some("gateway")).then(|| args[script].as_str())
}

fn same_package_entry(service: &Path, cli: &Path) -> bool {
    // The daemon owner prefers dist/index while install-cli's wrapper uses dist/entry.
    // The full service revision still binds its exact argv after this package admission.
    service.parent() == cli.parent()
        && service
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "index.js" | "index.mjs" | "entry.js" | "entry.mjs"))
}

fn capture(cli: &OpenClawCli, target: Option<&Target>, probe: bool) -> Result<Snapshot, String> {
    serde_json::from_value(capture_value(cli, target, probe)?)
        .map_err(|_| "Gateway status returned incomplete runtime metadata.".into())
}

fn capture_value(cli: &OpenClawCli, target: Option<&Target>, probe: bool) -> Result<Value, String> {
    let mut command = cli_command(cli, target)?;
    command.args(["gateway", "status", "--deep", "--json"]);
    if !probe {
        command.arg("--no-probe");
    }
    let output = checked_output(command, "Gateway runtime inspection")?;
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "Gateway status returned invalid JSON.".into())
}

fn version(cli: &OpenClawCli, target: Option<&Target>) -> Result<String, String> {
    let mut command = cli_command(cli, target)?;
    command.arg("--version");
    let output = checked_output(command, "CLI version verification")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let normalized = text.trim().strip_prefix("OpenClaw ").unwrap_or(text.trim());
    normalized
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "The CLI did not report its version.".into())
}

fn cli_command(cli: &OpenClawCli, target: Option<&Target>) -> Result<Command, String> {
    match target {
        Some(target) => target_command(cli, target),
        None => cli
            .command([] as [&str; 0])
            .map_err(|error| error.to_string()),
    }
}

fn target_command(cli: &OpenClawCli, target: &Target) -> Result<Command, String> {
    let mut command = Command::new(&target.runtime);
    if target.bun {
        command.arg("--no-install");
    }
    let mut paths = vec![target
        .runtime
        .parent()
        .ok_or("Runtime path has no parent.")?
        .to_path_buf()];
    paths.extend(std::env::split_paths(
        &cli.command_path().map_err(|error| error.to_string())?,
    ));
    command
        .arg(&target.entry)
        .env(
            "PATH",
            std::env::join_paths(paths).map_err(|error| error.to_string())?,
        )
        .env_remove("OPENCLAW_SQLITE_LIBRARY")
        .env_remove("LD_LIBRARY_PATH");
    if let Some(sqlite) = &target.sqlite {
        command.env("OPENCLAW_SQLITE_LIBRARY", sqlite);
    }
    Ok(command)
}

fn checked_output(mut command: Command, label: &str) -> Result<Output, String> {
    command.stdin(std::process::Stdio::null());
    let output = command
        .output()
        .map_err(|error| format!("{label} could not start: {error}"))?;
    if output.status.success() {
        return Ok(output);
    }
    Err(format!(
        "{label} failed: {}",
        output_tail(&output.stderr).unwrap_or_else(|| output.status.to_string())
    ))
}

fn bundled_target(runtime: &BundledRuntime, entry: &Path) -> Result<Target, String> {
    for path in [
        &runtime.bun,
        runtime.sqlite.as_ref().unwrap_or(&runtime.bun),
    ] {
        if !path.is_absolute()
            || fs::canonicalize(path).map_err(|error| error.to_string())? != *path
        {
            return Err("Bundled runtime paths must be immutable, absolute files.".into());
        }
    }
    Ok(Target {
        runtime: runtime.bun.clone(),
        entry: entry.to_path_buf(),
        sqlite: runtime.sqlite.clone(),
        bun: true,
    })
}

fn read_wrapper(cli: &OpenClawCli) -> Result<Wrapper, String> {
    let path = cli
        .managed_wrapper()
        .ok_or("This CLI is independently managed; its runtime was preserved.")?;
    let bytes = read_regular(&path)?;
    decode_wrapper(path, bytes)
}

fn decode_wrapper(path: PathBuf, bytes: Vec<u8>) -> Result<Wrapper, String> {
    let text = std::str::from_utf8(&bytes).map_err(|_| "The managed CLI wrapper is not UTF-8.")?;
    if let Some(line) = text
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix(MARKER))
    {
        let managed: Managed = serde_json::from_str(line)
            .map_err(|_| "The app's runtime ownership marker is invalid.")?;
        if render(&managed)? != bytes
            || managed.retained_wrapper.parent() != path.parent()
            || !managed
                .retained_wrapper
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".openclaw-tauri-node-"))
        {
            return Err(CHANGED.into());
        }
        if managed.pending.as_ref().is_some_and(|pending| {
            pending.original_wrapper.parent() != path.parent()
                || !pending.original_wrapper.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(".openclaw-tauri-transition-")
                })
        }) {
            return Err(CHANGED.into());
        }
        return Ok(Wrapper {
            path,
            bytes,
            node: managed.node.clone(),
            managed: Some(managed),
        });
    }
    let prefix = path.parent().and_then(Path::parent).ok_or(CHANGED)?;
    let node = legacy_node(text, prefix)?;
    Ok(Wrapper {
        path,
        bytes,
        node,
        managed: None,
    })
}

fn legacy_node(text: &str, prefix: &Path) -> Result<Target, String> {
    let start = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nexec \"{}/tools/node/bin/node\" \"",
        prefix.display()
    );
    let entry = text
        .strip_prefix(&start)
        .and_then(|text| text.strip_suffix("\" \"$@\"\n"))
        .ok_or("The CLI wrapper is not a canonical managed Node installation; it was preserved.")?;
    if entry
        .chars()
        .any(|ch| matches!(ch, '\n' | '\r' | '$' | '`' | '"' | '\\'))
        || !entry.ends_with("/dist/entry.js")
    {
        return Err(CHANGED.into());
    }
    let entry = fs::canonicalize(entry).map_err(|error| error.to_string())?;
    let root = fs::canonicalize(prefix).map_err(|error| error.to_string())?;
    let runtime =
        fs::canonicalize(prefix.join("tools/node/bin/node")).map_err(|error| error.to_string())?;
    if !entry.starts_with(&root) || !runtime.starts_with(root.join("tools")) {
        return Err(CHANGED.into());
    }
    Ok(Target {
        runtime,
        entry,
        sqlite: None,
        bun: false,
    })
}

fn managed_metadata(
    wrapper: &Wrapper,
    target: Target,
    package_version: String,
    binding: Option<Binding>,
) -> Result<Managed, String> {
    let (retained_wrapper, retained_wrapper_sha256) = match &wrapper.managed {
        Some(metadata) => (
            metadata.retained_wrapper.clone(),
            metadata.retained_wrapper_sha256.clone(),
        ),
        None => (
            backup(&wrapper.path, "node", &wrapper.bytes)?,
            digest(&wrapper.bytes),
        ),
    };
    Ok(Managed {
        purpose: Purpose::Gateway,
        target,
        node: wrapper.node.clone(),
        retained_wrapper,
        retained_wrapper_sha256,
        package_version,
        binding,
        pending: None,
    })
}

fn retained_bytes(wrapper: &Wrapper) -> Result<Vec<u8>, String> {
    let metadata = wrapper.managed.as_ref().ok_or(CHANGED)?;
    verified_backup(
        &metadata.retained_wrapper,
        &metadata.retained_wrapper_sha256,
    )
}

fn verified_backup(path: &Path, expected: &str) -> Result<Vec<u8>, String> {
    let bytes = read_regular(path)?;
    if digest(&bytes) != expected {
        return Err("The recovery wrapper changed; it was not restored.".into());
    }
    Ok(bytes)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn render(metadata: &Managed) -> Result<Vec<u8>, String> {
    let quoted = |path: &Path| -> Result<String, String> {
        let value = path
            .to_str()
            .filter(|value| !value.contains(['\n', '\r', '\0']))
            .ok_or("Runtime paths must be single-line UTF-8.")?;
        Ok(format!("'{}'", value.replace('\'', "'\\''")))
    };
    let sqlite = metadata
        .target
        .sqlite
        .as_ref()
        .map(|path| quoted(path).map(|value| format!("export OPENCLAW_SQLITE_LIBRARY={value}\n")))
        .transpose()?
        .unwrap_or_default();
    Ok(format!(
        "#!/bin/sh\n{MARKER}{}\n{sqlite}exec {} --no-install {} \"$@\"\n",
        serde_json::to_string(metadata).map_err(|error| error.to_string())?,
        quoted(&metadata.target.runtime)?,
        quoted(&metadata.target.entry)?
    )
    .into_bytes())
}

fn read_regular(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect CLI ownership: {error}"))?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err(CHANGED.into());
    }
    fs::read(path).map_err(|error| error.to_string())
}

fn same_wrapper(wrapper: &Wrapper) -> Result<(), String> {
    if read_regular(&wrapper.path)? == wrapper.bytes {
        Ok(())
    } else {
        Err(CHANGED.into())
    }
}

fn backup(wrapper: &Path, kind: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = wrapper.with_file_name(format!(
        ".openclaw-tauri-{kind}-{}.backup",
        uuid::Uuid::new_v4()
    ));
    write_private_file(&path, bytes, 0o600)?;
    Ok(path)
}

fn write_private_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

fn publish(wrapper: &Wrapper, bytes: &[u8]) -> Result<(), String> {
    let temporary = wrapper
        .path
        .with_file_name(format!(".openclaw-tauri-launcher-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        write_private_file(&temporary, bytes, 0o700)?;
        same_wrapper(wrapper)?;
        fs::rename(&temporary, &wrapper.path).map_err(|error| error.to_string())
    })();
    let _ = fs::remove_file(temporary);
    result
}

fn check_current(is_current: &dyn Fn() -> bool) -> Result<(), String> {
    if is_current() {
        Ok(())
    } else {
        Err("Runtime setup was superseded; retry from the current Gateway selection.".into())
    }
}

#[cfg(test)]
#[path = "runtime_migration_tests.rs"]
mod tests;
