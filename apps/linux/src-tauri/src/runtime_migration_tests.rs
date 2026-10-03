use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("openclaw-runtime-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("tools/node-v26/bin")).unwrap();
        fs::create_dir_all(path.join("tools/node-v26/lib/node_modules/openclaw/dist")).unwrap();
        fs::create_dir_all(path.join("bin")).unwrap();
        fs::write(path.join("tools/node-v26/bin/node"), "fixture").unwrap();
        fs::write(
            path.join("tools/node-v26/lib/node_modules/openclaw/dist/entry.js"),
            "fixture",
        )
        .unwrap();
        symlink("node-v26", path.join("tools/node")).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn wrapper(&self) -> Wrapper {
        let path = self.0.join("bin/openclaw");
        let text = format!("#!/usr/bin/env bash\nset -euo pipefail\nexec \"{}/tools/node/bin/node\" \"{}/tools/node-v26/lib/node_modules/openclaw/dist/entry.js\" \"$@\"\n", self.0.display(), self.0.display());
        fs::write(&path, &text).unwrap();
        Wrapper {
            path,
            bytes: text.as_bytes().to_vec(),
            node: legacy_node(&text, &self.0).unwrap(),
            managed: None,
        }
    }

    fn state(&self, wrapper: &Wrapper) -> Snapshot {
        serde_json::from_value(self.state_json(wrapper)).unwrap()
    }

    fn state_json(&self, wrapper: &Wrapper) -> Value {
        serde_json::json!({
            "cli": { "version": "2026.10.1", "runtime": { "kind": "node", "execPath": wrapper.node.runtime, "supported": true } },
            "service": {
                "loaded": true,
                "targetRole": "target",
                "command": { "programArguments": [wrapper.node.runtime, wrapper.node.entry, "gateway", "--port", "18789"] },
                "runtime": { "status": "running", "pid": 4100 },
                "runtimeIntent": { "status": "known", "revision": "no-pin", "stored": false },
                "revision": "original-service",
                "definitionMutation": "writable",
                "layout": { "entrypointReal": wrapper.node.entry }
            },
            "gateway": { "port": 18789 },
            "config": { "daemon": { "path": self.0.join("openclaw.json") } },
            "rpc": { "ok": true },
            "port": { "port": 18789, "status": "busy", "listeners": [{ "pid": 4100 }] }
        })
    }
}

#[test]
fn healthy_rpc_requires_the_running_intended_service_and_owned_listener() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let ready = fixture.state(&wrapper);
    assert!(ready.healthy_for(&wrapper.node));
    for phase in ["stopped", "unknown"] {
        let mut state = ready.clone();
        state.service.runtime.as_mut().unwrap().status = phase.into();
        assert!(
            !state.healthy_for(&wrapper.node),
            "RPC cannot prove a {phase} service is healthy"
        );
    }
    let mut state = ready.clone();
    state.service.runtime = None;
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.service.loaded = Some(false);
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.service.target_role = Some("diagnostic-only".into());
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.service.runtime.as_mut().unwrap().pid = None;
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.port.as_mut().unwrap().listeners[0].pid = Some(9000);
    assert!(
        !state.healthy_for(&wrapper.node),
        "a foreign healthy listener is not the running service"
    );
    state.port.as_mut().unwrap().listeners[0].ppid = Some(4100);
    assert!(
        state.healthy_for(&wrapper.node),
        "the CLI attributes a direct child listener to its service"
    );
    state.port.as_mut().unwrap().listeners.push(PortListener {
        pid: Some(9001),
        ppid: None,
    });
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.port.as_mut().unwrap().listeners.clear();
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.port.as_mut().unwrap().status = "unknown".into();
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.port.as_mut().unwrap().port = 18790;
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.service.command.as_mut().unwrap().program_arguments[0] = "/other/runtime".into();
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state
        .cli
        .as_mut()
        .unwrap()
        .runtime
        .as_mut()
        .unwrap()
        .supported = false;
    assert!(!state.healthy_for(&wrapper.node));
    state = ready.clone();
    state.rpc = Some(serde_json::json!({ "ok": false }));
    assert!(!state.healthy_for(&wrapper.node));
}

#[test]
fn identical_runtime_pin_in_another_profile_does_not_transfer_app_ownership() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let mut state = fixture.state(&wrapper);
    state.service.runtime_intent.as_mut().unwrap().stored = Some(true);
    state.service.runtime_intent.as_mut().unwrap().pin = Some(RuntimePin {
        runtime: "bun".into(),
        path: wrapper.node.runtime.clone(),
    });
    let mut metadata = managed_metadata(
        &wrapper,
        wrapper.node.clone(),
        "2026.10.1".into(),
        Some(state.binding().unwrap()),
    )
    .unwrap();
    metadata.pending = Some(Pending {
        original: state.binding().unwrap(),
        original_wrapper: fixture.0.join("original.backup"),
        original_wrapper_sha256: "fixture".into(),
        mode: Mode::OwnedUpdate,
    });
    assert!(metadata.owns(&state));
    assert!(pending_state(&metadata, &state).is_ok());
    state.config["daemon"]["path"] =
        serde_json::to_value(fixture.0.join("other-profile/openclaw.json")).unwrap();
    assert!(!metadata.owns(&state));
    assert!(pending_state(&metadata, &state).is_err());
}

#[test]
fn runtime_install_transports_observed_pin_and_definition() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let calls = fixture.0.join("install-args");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '{{\"ok\":true}}\\n'\n",
        calls.display().to_string().replace('\'', "'\\''")
    );
    let bun = fixture.0.join("bun");
    for path in [&wrapper.node.runtime, &bun] {
        fs::write(path, &script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let bun_target =
        bundled_target(&BundledRuntime { bun, sqlite: None }, &wrapper.node.entry).unwrap();
    let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
    for (target, definition) in [
        (&bun_target, Some("original-definition")),
        (&wrapper.node, Some("bun-definition")),
        (&bun_target, None),
    ] {
        let mut state = fixture.state(&wrapper);
        state.service.runtime_intent.as_mut().unwrap().definition = definition.map(str::to_owned);
        install(&cli, target, &state, target.bun).unwrap();
        let argv = fs::read_to_string(&calls).unwrap();
        let argv: Vec<_> = argv.lines().collect();
        let expectation = argv
            .iter()
            .position(|arg| *arg == "--expected-runtime-pin")
            .unwrap();
        let observed: Value = serde_json::from_str(argv[expectation + 1]).unwrap();
        assert_eq!(
            observed,
            serde_json::json!({ "revision": "no-pin", "definition": definition })
        );
        let runtime = argv.iter().position(|arg| *arg == "--runtime").unwrap();
        assert_eq!(argv[runtime + 1], if target.bun { "bun" } else { "node" });
    }
}

#[test]
fn failed_install_does_not_restore_wrapper_from_a_foreign_healthy_rpc() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let original = fixture.state(&wrapper).binding().unwrap();
    let state_path = fixture.0.join("stopped-service.json");
    let calls = fixture.0.join("restore-calls");
    let mut state = fixture.state_json(&wrapper);
    state["service"]["runtime"]["status"] = Value::String("stopped".into());
    state["port"]["listeners"][0]["pid"] = Value::from(9000);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let bun = fixture.0.join("bun");
    let script = format!("#!/bin/sh\ncase \"$*\" in\n *--version*) printf 'OpenClaw 2026.10.1\\n' ;;\n *'gateway status'*) exec /bin/cat '{}' ;;\n *'gateway install'*) printf 'install\\n' >> '{}'; printf '{{\"ok\":false}}\\n' ;;\n *) exit 9 ;;\nesac\n", state_path.display().to_string().replace('\'', "'\\''"), calls.display().to_string().replace('\'', "'\\''"));
    for path in [&wrapper.node.runtime, &bun] {
        fs::write(path, &script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let target =
        bundled_target(&BundledRuntime { bun, sqlite: None }, &wrapper.node.entry).unwrap();
    let mut metadata =
        managed_metadata(&wrapper, target.clone(), "2026.10.1".into(), None).unwrap();
    metadata.pending = Some(Pending {
        original: original.clone(),
        original_wrapper: backup(&wrapper.path, "transition", &wrapper.bytes).unwrap(),
        original_wrapper_sha256: digest(&wrapper.bytes),
        mode: Mode::Adopt,
    });
    publish(&wrapper, &render(&metadata).unwrap()).unwrap();
    let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
    let pending = read_wrapper(&cli).unwrap();
    assert!(restore_after_failure(&cli, &pending, &original, &target, None, "2026.10.1").is_err());
    assert_eq!(fs::read_to_string(calls).unwrap(), "install\n");
    assert_eq!(fs::read(&wrapper.path).unwrap(), pending.bytes);
}

#[test]
fn retained_node_admission_rejects_diagnostics_and_recovery_to_another_executable() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let original = fixture.state(&wrapper);
    assert!(require_retained_node(&original, &wrapper.node, "2026.10.1").is_ok());
    let mut state = original.clone();
    state
        .cli
        .as_mut()
        .unwrap()
        .runtime
        .as_mut()
        .unwrap()
        .supported = false;
    assert!(require_retained_node(&state, &wrapper.node, "2026.10.1").is_err());
    let recovered = fixture.0.join("different-node");
    fs::write(&recovered, "recovered runtime").unwrap();
    state = original.clone();
    state
        .cli
        .as_mut()
        .unwrap()
        .runtime
        .as_mut()
        .unwrap()
        .exec_path = recovered;
    assert!(require_retained_node(&state, &wrapper.node, "2026.10.1").is_err());
    state = original.clone();
    state.cli.as_mut().unwrap().runtime.as_mut().unwrap().kind = "bun".into();
    assert!(require_retained_node(&state, &wrapper.node, "2026.10.1").is_err());
    assert!(require_retained_node(&original, &wrapper.node, "2026.10.2").is_err());
}

#[test]
fn partial_owned_update_qualifies_node_and_restores_the_current_package() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let bun = fixture.0.join("tools/bun/bin/bun");
    fs::create_dir_all(bun.parent().unwrap()).unwrap();
    let calls = fixture.0.join("maintenance-calls");
    let version_file = fixture.0.join("version");
    let blocked = fixture.0.join("node-blocked");
    let node_status = fixture.0.join("node-status.json");
    let blocked_status = fixture.0.join("blocked-status.json");
    let bun_status = fixture.0.join("bun-status.json");
    let restored_status = fixture.0.join("restored-status.json");
    fs::write(&version_file, "2026.10.1\n").unwrap();
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let node_script = format!("#!/bin/sh\ncase \"$*\" in\n *--version*) printf 'OpenClaw '; exec /bin/cat {} ;;\n *'gateway status'*) if test -e {}; then exec /bin/cat {}; else exec /bin/cat {}; fi ;;\n *'update --yes'*) printf 'update\\n' >> {}; printf '2026.10.2\\n' > {} ;;\n *'update repair'*) printf 'repair\\n' >> {}; : > {} ;;\n *'gateway install --force --json --runtime node --expected-runtime-pin '*' --port 18789') printf 'install-node\\n' >> {}; /bin/cp {} {}; printf '{{\"ok\":true}}\\n' ;;\n *) printf 'unexpected-node-mutation\\n' >> {}; exit 9 ;;\nesac\n", quote(&version_file), quote(&blocked), quote(&blocked_status), quote(&node_status), quote(&calls), quote(&version_file), quote(&calls), quote(&blocked), quote(&calls), quote(&restored_status), quote(&node_status), quote(&calls));
    let bun_script = format!("#!/bin/sh\ncase \"$*\" in\n *--version*) printf 'OpenClaw '; exec /bin/cat {} ;;\n *'gateway status'*) exec /bin/cat {} ;;\n *) printf 'unexpected-bun-maintenance\\n' >> {}; exit 9 ;;\nesac\n", quote(&version_file), quote(&bun_status), quote(&calls));
    for (path, script) in [(&wrapper.node.runtime, node_script), (&bun, bun_script)] {
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut state = fixture.state_json(&wrapper);
    state["cli"]["version"] = Value::String("2026.10.2".into());
    let mut restored = state.clone();
    restored["service"]["revision"] = Value::String("restored-node-service".into());
    fs::write(&restored_status, serde_json::to_vec(&restored).unwrap()).unwrap();
    state["service"]["command"]["programArguments"][0] = serde_json::to_value(&bun).unwrap();
    state["service"]["runtimeIntent"] = serde_json::json!({
        "status": "known", "revision": "bun-pin", "stored": true,
        "pin": { "runtime": "bun", "path": bun }
    });
    fs::write(&node_status, serde_json::to_vec(&state).unwrap()).unwrap();
    let mut unsupported = state.clone();
    unsupported["cli"]["runtime"]["supported"] = Value::Bool(false);
    fs::write(&blocked_status, serde_json::to_vec(&unsupported).unwrap()).unwrap();
    let mut bun_state = state.clone();
    bun_state["cli"]["runtime"]["kind"] = Value::String("bun".into());
    bun_state["cli"]["runtime"]["execPath"] = serde_json::to_value(&bun).unwrap();
    fs::write(&bun_status, serde_json::to_vec(&bun_state).unwrap()).unwrap();
    let runtime = BundledRuntime { bun, sqlite: None };
    let target = bundled_target(&runtime, &wrapper.node.entry).unwrap();
    let snapshot: Snapshot = serde_json::from_value(state).unwrap();
    let metadata = managed_metadata(
        &wrapper,
        target,
        "2026.10.1".into(),
        Some(snapshot.binding().unwrap()),
    )
    .unwrap();
    publish(&wrapper, &render(&metadata).unwrap()).unwrap();
    let original = fs::read(&wrapper.path).unwrap();
    let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
    let error = migrate(&cli, &runtime, "2026.10.2", Mode::OwnedUpdate, &|| true).unwrap_err();
    assert!(
        error.contains("retained Node"),
        "unexpected refusal: {error}"
    );
    assert_eq!(fs::read_to_string(&calls).unwrap(), "update\nrepair\n");
    assert_eq!(
        fs::read(&wrapper.path).unwrap(),
        original,
        "no pending intent or runtime replacement may publish without a qualified rollback Node"
    );
    assert!(restore_retained_node(&cli, &|| true)
        .unwrap_err()
        .contains("retained Node"));
    assert_eq!(fs::read_to_string(&calls).unwrap(), "update\nrepair\n");
    fs::remove_file(blocked).unwrap();
    restore_retained_node(&cli, &|| true).unwrap();
    assert_eq!(
        fs::read_to_string(calls).unwrap(),
        "update\nrepair\ninstall-node\n"
    );
    assert_eq!(version(&cli, Some(&wrapper.node)).unwrap(), "2026.10.2");
    assert_eq!(fs::read(&wrapper.path).unwrap(), wrapper.bytes);
    assert!(!is_app_managed(&cli).unwrap());
    let final_state = capture(&cli, Some(&wrapper.node), true).unwrap();
    assert!(final_state.unpinned() && final_state.healthy_for(&wrapper.node));
}

#[test]
fn explicit_adoption_updates_package_before_pin_refusal() {
    for (case, loaded, inspectable, known_before, update) in [
        ("legacy-loaded", true, true, false, true),
        ("legacy-absent", false, true, false, true),
        ("legacy-unknown", true, false, false, true),
        ("known-loaded", true, true, true, true),
        ("known-absent", false, true, true, true),
        ("known-current", true, true, true, false),
    ] {
        let fixture = Fixture::new();
        let wrapper = fixture.wrapper();
        fs::set_permissions(&wrapper.path, fs::Permissions::from_mode(0o700)).unwrap();
        let status_file = fixture.0.join("status.json");
        let candidate_file = fixture.0.join("candidate-status.json");
        let version_file = fixture.0.join("version");
        let calls = fixture.0.join("calls");
        let mut before = fixture.state_json(&wrapper);
        let installed = if update { "2026.9.5" } else { "2026.10.2" };
        before["cli"]["version"] = Value::String(installed.into());
        if !known_before {
            before["cli"].as_object_mut().unwrap().remove("runtime");
            for key in ["runtimeIntent", "revision", "definitionMutation"] {
                before["service"].as_object_mut().unwrap().remove(key);
            }
        }
        let mut after = fixture.state_json(&wrapper);
        after["cli"]["version"] = Value::String("2026.10.2".into());
        after["service"]["runtimeIntent"] = serde_json::json!({
            "status": "known", "revision": "operator-pin", "stored": true,
            "pin": { "runtime": "node", "path": wrapper.node.runtime }
        });
        if !loaded {
            for state in [&mut before, &mut after] {
                state["service"]["loaded"] = Value::Bool(false);
                state["service"]["command"] = Value::Null;
                state["service"]["runtime"]["status"] = Value::String("stopped".into());
            }
            after["service"]["runtimeIntent"]
                .as_object_mut()
                .unwrap()
                .remove("pin");
        }
        if !inspectable {
            after["service"]["runtimeIntent"] = serde_json::json!({ "status": "unknown" });
            for key in ["revision", "definitionMutation"] {
                after["service"].as_object_mut().unwrap().remove(key);
            }
        }
        if known_before {
            before["service"]["runtimeIntent"] = after["service"]["runtimeIntent"].clone();
        }
        fs::write(&status_file, serde_json::to_vec(&before).unwrap()).unwrap();
        fs::write(&candidate_file, serde_json::to_vec(&after).unwrap()).unwrap();
        fs::write(&version_file, format!("{installed}\n")).unwrap();
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let script = format!("#!/bin/sh\ncase \"$*\" in\n *--version*) printf 'OpenClaw '; exec /bin/cat {} ;;\n *'gateway status'*) exec /bin/cat {} ;;\n *'update --yes'*) printf 'update\\n' >> {}; /bin/cp {} {}; printf '2026.10.2\\n' > {} ;;\n *) printf 'unexpected-mutation\\n' >> {}; exit 9 ;;\nesac\n", quote(&version_file), quote(&status_file), quote(&calls), quote(&candidate_file), quote(&status_file), quote(&version_file), quote(&calls));
        fs::write(&wrapper.node.runtime, script).unwrap();
        fs::set_permissions(&wrapper.node.runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let bun = fixture.0.join("bun");
        fs::write(&bun, "#!/bin/sh\nexit 9\n").unwrap();
        fs::set_permissions(&bun, fs::Permissions::from_mode(0o700)).unwrap();
        let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
        let error = migrate(
            &cli,
            &BundledRuntime { bun, sqlite: None },
            "2026.10.2",
            Mode::Adopt,
            &|| true,
        )
        .unwrap_err();
        assert_eq!(
            error.contains("CLI package reached 2026.10.2"),
            update,
            "{case}: {error}"
        );
        if update {
            assert!(
                error.contains("bundled Bun was not activated"),
                "{case}: {error}"
            );
        }
        if inspectable {
            assert!(error.contains("existing runtime pin"), "{case}: {error}");
        } else {
            assert!(error.contains("could not be verified"), "{case}: {error}");
            assert!(
                !error.contains("pin"),
                "unknown inspection must not claim pin preservation: {error}"
            );
        }
        assert_eq!(
            fs::read_to_string(calls).unwrap_or_default(),
            if update { "update\n" } else { "" },
            "{case}"
        );
        assert_eq!(fs::read(&wrapper.path).unwrap(), wrapper.bytes, "{case}");
        assert!(!is_app_managed(&cli).unwrap(), "{case}");
        let observed: Value = serde_json::from_slice(&fs::read(status_file).unwrap()).unwrap();
        assert_eq!(
            observed["service"]["runtimeIntent"], after["service"]["runtimeIntent"],
            "{case}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn external_symlinks_and_large_launchers_do_not_claim_app_ownership() {
    let fixture = Fixture::new();
    let external = fixture.0.join("external-cli");
    // Ownership must not inspect the linked target, even when its bytes resemble an app marker.
    fs::write(
        &external,
        "#!/bin/sh\n# OpenClaw-Tauri runtime v1 invalid\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&external, fs::Permissions::from_mode(0o700)).unwrap();
    let path = fixture.0.join("bin/openclaw");
    symlink(&external, &path).unwrap();
    let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
    assert!(!is_app_managed(&cli).unwrap());
    assert!(
        read_wrapper(&cli).is_err(),
        "explicit adoption must remain strict"
    );
    fs::remove_file(&path).unwrap();
    symlink(fixture.0.join("missing-external-cli"), &path).unwrap();
    assert!(!is_app_managed(&cli).unwrap());
    fs::remove_file(&path).unwrap();
    fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert!(!is_app_managed(&cli).unwrap());
}

#[test]
fn explicit_adoption_preserves_pins_unknown_metadata_and_external_launchers() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let original = fixture.state(&wrapper);
    assert!(admit(&wrapper, &original, Mode::Adopt).is_ok());
    assert!(admit(&wrapper, &original, Mode::OwnedUpdate).is_err());

    let mut state = original.clone();
    state.service.runtime_intent = None;
    assert!(admit(&wrapper, &state, Mode::Adopt).is_err());
    state = original.clone();
    state.service.runtime_intent.as_mut().unwrap().stored = Some(true);
    assert!(admit(&wrapper, &state, Mode::Adopt).is_err());
    state = original.clone();
    state.service.launcher_overridden = true;
    assert!(admit(&wrapper, &state, Mode::Adopt).is_err());
    state = original;
    state.service.definition_mutation = Some("sealed".into());
    assert!(admit(&wrapper, &state, Mode::Adopt).is_err());
}

#[test]
fn old_status_without_layout_accepts_only_the_generated_runtime_command() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let mut state = fixture.state(&wrapper);
    state.service.layout = None;
    let command = state.service.command.as_mut().unwrap();
    command
        .program_arguments
        .insert(1, "--max-old-space-size=2048".into());
    assert!(state.matches_target(&wrapper.node));
    state
        .service
        .command
        .as_mut()
        .unwrap()
        .program_arguments
        .insert(1, "--import".into());
    assert!(!state.matches_target(&wrapper.node));
    assert!(same_package_entry(
        &wrapper.node.entry.with_file_name("index.js"),
        &wrapper.node.entry
    ));
    assert!(!same_package_entry(
        &wrapper.node.entry.with_file_name("custom.js"),
        &wrapper.node.entry
    ));
}

#[test]
fn browser_only_marker_cannot_adopt_an_existing_gateway() {
    let fixture = Fixture::new();
    let mut wrapper = fixture.wrapper();
    let mut state = fixture.state(&wrapper);
    wrapper.managed =
        Some(managed_metadata(&wrapper, wrapper.node.clone(), "2026.10.1".into(), None).unwrap());
    assert!(admit(&wrapper, &state, Mode::Fresh).is_err());
    assert!(admit(&wrapper, &state, Mode::OwnedUpdate).is_err());
    state.service.loaded = Some(false);
    state.service.command = None;
    wrapper.managed.as_mut().unwrap().purpose = Purpose::Browser;
    assert!(admit(&wrapper, &state, Mode::Fresh).is_err());
    wrapper.managed.as_mut().unwrap().purpose = Purpose::Gateway;
    assert!(admit(&wrapper, &state, Mode::Fresh).is_ok());
    state.service.runtime_intent.as_mut().unwrap().stored = Some(true);
    assert!(admit(&wrapper, &state, Mode::Fresh).is_err());
}

#[test]
fn changed_pin_revokes_ownership_but_service_environment_stays_core_owned() {
    let fixture = Fixture::new();
    let mut wrapper = fixture.wrapper();
    let mut state = fixture.state(&wrapper);
    state.service.runtime_intent.as_mut().unwrap().stored = Some(true);
    state.service.runtime_intent.as_mut().unwrap().pin = Some(RuntimePin {
        runtime: "bun".into(),
        path: wrapper.node.runtime.clone(),
    });
    wrapper.managed = Some(
        managed_metadata(
            &wrapper,
            wrapper.node.clone(),
            "2026.10.1".into(),
            Some(state.binding().unwrap()),
        )
        .unwrap(),
    );
    assert!(admit(&wrapper, &state, Mode::OwnedUpdate).is_ok());
    state.service.runtime_intent.as_mut().unwrap().revision = Some("operator-repinned".into());
    assert!(admit(&wrapper, &state, Mode::OwnedUpdate).is_err());
    state.service.runtime_intent.as_mut().unwrap().revision = Some("no-pin".into());
    state.service.revision = Some("operator-reconfigured".into());
    assert!(admit(&wrapper, &state, Mode::OwnedUpdate).is_ok());
    assert_ne!(
        state.binding().unwrap(),
        wrapper.managed.unwrap().binding.unwrap()
    );
}

#[test]
fn stopped_and_retained_unloaded_services_remain_paused() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let mut state = fixture.state(&wrapper);
    assert!(!state.paused());
    state.service.runtime.as_mut().unwrap().status = "stopped".into();
    assert!(state.paused());
    state.service.runtime = None;
    state.service.loaded = Some(false);
    assert!(state.paused());
    state.service.command = None;
    assert!(!state.paused());
}

#[test]
fn retained_wrapper_is_private_and_tampering_cannot_be_published() {
    let fixture = Fixture::new();
    let mut wrapper = fixture.wrapper();
    let metadata =
        managed_metadata(&wrapper, wrapper.node.clone(), "2026.10.1".into(), None).unwrap();
    assert_eq!(
        fs::metadata(&metadata.retained_wrapper)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    wrapper.managed = Some(metadata.clone());
    assert_eq!(retained_bytes(&wrapper).unwrap(), wrapper.bytes);
    fs::write(&metadata.retained_wrapper, "modified backup").unwrap();
    assert!(retained_bytes(&wrapper).is_err());
    fs::write(&wrapper.path, "operator replacement").unwrap();
    assert!(publish(&wrapper, b"candidate").is_err());
    assert_eq!(fs::read(&wrapper.path).unwrap(), b"operator replacement");
}

#[test]
fn launcher_keeps_literal_paths_arguments_and_bun_no_install() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let runtime = fixture.0.join("runtime's $literal bun");
    fs::write(&runtime, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let entry = fixture.0.join("entry's $literal.js");
    let target = Target {
        runtime,
        entry: entry.clone(),
        sqlite: None,
        bun: true,
    };
    let metadata = managed_metadata(&wrapper, target, "2026.10.1".into(), None).unwrap();
    publish(&wrapper, &render(&metadata).unwrap()).unwrap();
    let output = Command::new("/bin/sh")
        .arg(&wrapper.path)
        .args(["space argument", "$literal"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "--no-install\n{}\nspace argument\n$literal\n",
            entry.display()
        )
    );
}

#[test]
fn interrupted_install_finishes_from_persisted_intent_without_reinstalling() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    let original = fixture.state(&wrapper);
    let bun = fixture.0.join("tools/bun/bin/bun");
    fs::create_dir_all(bun.parent().unwrap()).unwrap();
    let state_path = fixture.0.join("service.json");
    let bun_state_path = fixture.0.join("bun-service.json");
    for (path, status_path) in [
        (&wrapper.node.runtime, &state_path),
        (&bun, &bun_state_path),
    ] {
        let script = format!("#!/bin/sh\ncase \"$*\" in\n  *--version*) printf 'OpenClaw 2026.10.1\\n' ;;\n  *'gateway status'*) exec /bin/cat '{}' ;;\n  *) echo 'unexpected service mutation' >&2; exit 7 ;;\nesac\n", status_path.display().to_string().replace('\'', "'\\''"));
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let write_state = |state: &Value| {
        fs::write(&state_path, serde_json::to_vec(state).unwrap()).unwrap();
        let mut bun_state = state.clone();
        bun_state["cli"]["runtime"]["kind"] = Value::String("bun".into());
        bun_state["cli"]["runtime"]["execPath"] = serde_json::to_value(&bun).unwrap();
        fs::write(&bun_state_path, serde_json::to_vec(&bun_state).unwrap()).unwrap();
    };
    let runtime = BundledRuntime {
        bun: bun.clone(),
        sqlite: None,
    };
    let target = bundled_target(&runtime, &wrapper.node.entry).unwrap();
    let mut metadata = managed_metadata(&wrapper, target, "2026.10.1".into(), None).unwrap();
    metadata.pending = Some(Pending {
        original: original.binding().unwrap(),
        original_wrapper: backup(&wrapper.path, "transition", &wrapper.bytes).unwrap(),
        original_wrapper_sha256: digest(&wrapper.bytes),
        mode: Mode::Adopt,
    });
    publish(&wrapper, &render(&metadata).unwrap()).unwrap();
    let service = serde_json::json!({
        "cli": { "version": "2026.10.1", "runtime": { "kind": "node", "execPath": wrapper.node.runtime, "supported": true } },
        "service": {
            "loaded": true, "targetRole": "target", "command": { "programArguments": [bun, wrapper.node.entry, "gateway"] },
            "runtime": { "status": "running", "pid": 4100 }, "runtimeIntent": {
                "status": "known", "revision": "bun-pin", "stored": true,
                "pin": { "runtime": "bun", "path": bun }
            },
            "revision": "installed-bun", "definitionMutation": "writable",
            "layout": { "entrypointReal": wrapper.node.entry }
        },
        "gateway": { "port": 18789 }, "config": { "daemon": { "path": fixture.0.join("openclaw.json") } }, "rpc": { "ok": true },
        "port": { "port": 18789, "status": "busy", "listeners": [{ "pid": 4100 }] }
    });
    let mut external = service.clone();
    external["service"]["runtimeIntent"]["pin"]["path"] = Value::String("/independent/bun".into());
    write_state(&external);
    let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
    let pending_bytes = fs::read(&wrapper.path).unwrap();
    assert!(is_app_managed(&cli).unwrap());
    assert!(
        !inspect(&cli).unwrap().managed,
        "startup must attach without updating an externally repinned service"
    );
    assert!(migrate(&cli, &runtime, "2026.10.1", Mode::OwnedUpdate, &|| true).is_err());
    assert_eq!(fs::read(&wrapper.path).unwrap(), pending_bytes);

    let mut fresh = metadata.clone();
    fresh.pending = None;
    publish(&read_wrapper(&cli).unwrap(), &render(&fresh).unwrap()).unwrap();
    let mut absent = service.clone();
    absent["service"]["loaded"] = Value::Bool(false);
    absent["service"]["command"] = Value::Null;
    absent["service"]["runtime"]["status"] = Value::String("stopped".into());
    absent["service"]["runtimeIntent"]["stored"] = Value::Bool(false);
    absent["service"]["runtimeIntent"]["pin"] = Value::Null;
    write_state(&absent);
    let fresh_status = inspect(&cli).unwrap();
    assert!(
        fresh_status.managed,
        "startup must resume an admitted fresh setup"
    );
    assert!(!fresh_status.paused);
    absent["service"]["runtimeIntent"]["stored"] = Value::Bool(true);
    write_state(&absent);
    assert!(
        !inspect(&cli).unwrap().managed,
        "an absent service's retained pin still owns runtime intent"
    );
    write_state(&service);
    assert!(
        !inspect(&cli).unwrap().managed,
        "a fresh marker cannot adopt an existing service"
    );
    publish(&read_wrapper(&cli).unwrap(), &pending_bytes).unwrap();
    write_state(&service);
    assert!(
        inspect(&cli).unwrap().managed,
        "startup must resume the admitted interrupted transaction"
    );
    let mut paused = service.clone();
    paused["service"]["runtime"]["status"] = Value::String("stopped".into());
    write_state(&paused);
    let paused_service = fs::read(&state_path).unwrap();
    let paused_bun_service = fs::read(&bun_state_path).unwrap();
    for mode in [Mode::Adopt, Mode::OwnedUpdate, Mode::Fresh] {
        assert_eq!(
            migrate(&cli, &runtime, "2026.10.1", mode, &|| true).unwrap(),
            MigrationOutcome::DeferredPaused,
            "{mode:?} must require Start Gateway even with a pending transition"
        );
        assert_eq!(fs::read(&wrapper.path).unwrap(), pending_bytes);
    }
    assert!(restore_retained_node(&cli, &|| true)
        .unwrap_err()
        .contains("Start it before"));
    assert_eq!(fs::read(&wrapper.path).unwrap(), pending_bytes);
    assert_eq!(fs::read(&state_path).unwrap(), paused_service);
    assert_eq!(fs::read(&bun_state_path).unwrap(), paused_bun_service);
    write_state(&service);
    assert_eq!(
        migrate(&cli, &runtime, "2026.10.1", Mode::OwnedUpdate, &|| true).unwrap(),
        MigrationOutcome::Current
    );
    let completed = read_wrapper(&cli).unwrap().managed.unwrap();
    assert!(completed.pending.is_none());
    assert_eq!(completed.binding.unwrap().pin_revision, "bun-pin");
    assert_eq!(
        verified_backup(
            &completed.retained_wrapper,
            &completed.retained_wrapper_sha256
        )
        .unwrap(),
        wrapper.bytes
    );
}
