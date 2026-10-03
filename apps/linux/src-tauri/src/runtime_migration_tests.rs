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
    assert!(metadata.owns(&state));
    metadata.pending = Some(Pending {
        original: state.binding().unwrap(),
        original_wrapper: fixture.0.join("original.backup"),
        original_wrapper_sha256: "fixture".into(),
        mode: Mode::OwnedUpdate,
    });
    assert!(!metadata.owns(&state), "intent is not committed ownership");
    metadata.pending = None;
    state.config["daemon"]["path"] =
        serde_json::to_value(fixture.0.join("other-profile/openclaw.json")).unwrap();
    assert!(!metadata.owns(&state));
}

#[test]
fn runtime_install_transports_observed_pin_and_definition() {
    let fixture = Fixture::new();
    let wrapper = fixture.wrapper();
    fs::set_permissions(&wrapper.path, fs::Permissions::from_mode(0o700)).unwrap();
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
    assert!(!has_gateway_runtime_record(&cli).unwrap());
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
        assert!(!has_gateway_runtime_record(&cli).unwrap(), "{case}");
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
fn marker_preflight_does_not_follow_external_symlinks_or_large_launchers() {
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
    assert!(!has_gateway_runtime_record(&cli).unwrap());
    assert!(
        read_wrapper(&cli).is_err(),
        "explicit adoption must remain strict"
    );
    fs::remove_file(&path).unwrap();
    symlink(fixture.0.join("missing-external-cli"), &path).unwrap();
    assert!(!has_gateway_runtime_record(&cli).unwrap());
    fs::remove_file(&path).unwrap();
    fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert!(!has_gateway_runtime_record(&cli).unwrap());
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

struct MigrationFixture {
    fixture: Fixture,
    wrapper: Wrapper,
    runtime: BundledRuntime,
    cli: OpenClawCli,
}

impl MigrationFixture {
    fn new() -> Self {
        let fixture = Fixture::new();
        let wrapper = fixture.wrapper();
        fs::set_permissions(&wrapper.path, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = BundledRuntime {
            bun: fixture.0.join("bun"),
            sqlite: None,
        };
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        for (kind, executable) in [("node", &wrapper.node.runtime), ("bun", &runtime.bun)] {
            let script = format!(
                "#!/bin/sh\ncase \"$*\" in\n *--version*) printf 'OpenClaw 2026.10.1\\n' ;;\n *'gateway status'*) printf '%s\\n' \"{kind} $*\" >> {}; exec /bin/cat {} ;;\n *'update repair'*) printf 'repair\\n' >> {} ;;\n *'gateway install'*'--runtime bun '*) printf 'install-bun\\n' >> {}; /bin/cp {} {}; /bin/cp {} {}; printf '{{\"ok\":true}}\\n' ;;\n *'gateway install'*'--runtime node '*) printf 'install-node\\n' >> {}; /bin/cp {} {}; /bin/cp {} {}; printf '{{\"ok\":true}}\\n' ;;\n *) printf 'unexpected-mutation\\n' >> {}; exit 9 ;;\nesac\n",
                quote(&fixture.0.join("inspections")), quote(&fixture.0.join(format!("current-{kind}.json"))),
                quote(&fixture.0.join("calls")), quote(&fixture.0.join("calls")),
                quote(&fixture.0.join("installed-node.json")), quote(&fixture.0.join("current-node.json")),
                quote(&fixture.0.join("installed-bun.json")), quote(&fixture.0.join("current-bun.json")),
                quote(&fixture.0.join("calls")),
                quote(&fixture.0.join("restored-node.json")), quote(&fixture.0.join("current-node.json")),
                quote(&fixture.0.join("restored-bun.json")), quote(&fixture.0.join("current-bun.json")),
                quote(&fixture.0.join("calls")),
            );
            fs::write(executable, script).unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cli = OpenClawCli::browser_runtime(fixture.0.clone()).unwrap();
        let result = Self {
            fixture,
            wrapper,
            runtime,
            cli,
        };
        let original = result.fixture.state_json(&result.wrapper);
        result.write_state("current", &original);
        result.write_state("installed", &result.bun_state());
        let mut restored = original;
        restored["service"]["revision"] = "restored-node".into();
        result.write_state("restored", &restored);
        result
    }

    fn write_state(&self, name: &str, state: &Value) {
        for (kind, runtime) in [
            ("node", &self.wrapper.node.runtime),
            ("bun", &self.runtime.bun),
        ] {
            let mut projected = state.clone();
            projected["cli"]["runtime"]["kind"] = kind.into();
            projected["cli"]["runtime"]["execPath"] = serde_json::to_value(runtime).unwrap();
            fs::write(
                self.fixture.0.join(format!("{name}-{kind}.json")),
                serde_json::to_vec(&projected).unwrap(),
            )
            .unwrap();
        }
    }

    fn bun_state(&self) -> Value {
        let mut state = self.fixture.state_json(&self.wrapper);
        state["service"]["command"]["programArguments"][0] =
            serde_json::to_value(&self.runtime.bun).unwrap();
        state["service"]["revision"] = "installed-bun".into();
        state["service"]["runtimeIntent"] = serde_json::json!({
            "status": "known", "revision": "bun-pin", "stored": true,
            "pin": { "runtime": "bun", "path": self.runtime.bun }
        });
        state
    }

    fn publish_pending(&self, original: &Value, mode: Mode) -> Managed {
        let mut metadata = managed_metadata(
            &self.wrapper,
            bundled_target(&self.runtime, &self.wrapper.node.entry).unwrap(),
            "2026.10.1".into(),
            None,
        )
        .unwrap();
        let original: Snapshot = serde_json::from_value(original.clone()).unwrap();
        metadata.pending = Some(Pending {
            original: original.binding().unwrap(),
            original_wrapper: backup(&self.wrapper.path, "transition", &self.wrapper.bytes)
                .unwrap(),
            original_wrapper_sha256: digest(&self.wrapper.bytes),
            mode,
        });
        self.publish_metadata(&metadata);
        metadata
    }

    fn publish_metadata(&self, metadata: &Managed) {
        publish(
            &read_wrapper(&self.cli).unwrap(),
            &render(metadata).unwrap(),
        )
        .unwrap();
    }

    fn migrate(&self, mode: Mode) -> Result<MigrationOutcome, String> {
        migrate(&self.cli, &self.runtime, "2026.10.1", mode, &|| true)
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.fixture.0.join("calls")).unwrap_or_default()
    }

    fn current_bytes(&self) -> [Vec<u8>; 2] {
        ["node", "bun"]
            .map(|kind| fs::read(self.fixture.0.join(format!("current-{kind}.json"))).unwrap())
    }
}

#[test]
fn interrupted_bun_bindings_become_external_without_health_or_service_mutation() {
    for case in [
        "same-bun",
        "changed-definition",
        "changed-pin",
        "different-runtime",
        "different-profile",
        "unknown-intent",
        "failed-health",
        "paused",
    ] {
        let fixture = MigrationFixture::new();
        let metadata =
            fixture.publish_pending(&fixture.fixture.state_json(&fixture.wrapper), Mode::Adopt);
        let pending_bytes = fs::read(&fixture.wrapper.path).unwrap();
        let mut state = fixture.bun_state();
        match case {
            "changed-definition" => state["service"]["revision"] = "operator-definition".into(),
            "changed-pin" => {
                state["service"]["runtimeIntent"]["revision"] =
                    "operator-repinned-identical-bun".into()
            }
            "different-runtime" => {
                state["service"]["runtimeIntent"]["pin"]["path"] = "/independent/bun".into()
            }
            "different-profile" => {
                state["config"]["daemon"]["path"] =
                    serde_json::to_value(fixture.fixture.0.join("other-profile/openclaw.json"))
                        .unwrap()
            }
            "unknown-intent" => state["service"]["runtimeIntent"]["status"] = "unknown".into(),
            "failed-health" => state["rpc"]["ok"] = false.into(),
            "paused" => state["service"]["runtime"]["status"] = "stopped".into(),
            _ => {}
        }
        fixture.write_state("current", &state);
        let service_bytes = fixture.current_bytes();
        assert!(has_gateway_runtime_record(&fixture.cli).unwrap());
        assert_eq!(
            fixture.migrate(Mode::OwnedUpdate).unwrap(),
            MigrationOutcome::PreservedExternal,
            "{case}"
        );
        let external = read_wrapper(&fixture.cli).unwrap().managed.unwrap();
        let mut expected = metadata.clone();
        expected.purpose = Purpose::External;
        expected.binding = None;
        expected.pending = None;
        assert_eq!(
            external, expected,
            "recovery metadata is retained for {case}"
        );
        let snapshot: Snapshot = serde_json::from_value(state).unwrap();
        assert!(!external.owns(&snapshot));
        assert_eq!(interrupted_notice(&fixture.cli), Some(INTERRUPTED));
        assert!(
            has_gateway_runtime_record(&fixture.cli).unwrap(),
            "a record is not ownership"
        );
        let pending = metadata.pending.unwrap();
        assert_eq!(
            verified_backup(&pending.original_wrapper, &pending.original_wrapper_sha256).unwrap(),
            fixture.wrapper.bytes
        );
        assert_eq!(
            verified_backup(
                &external.retained_wrapper,
                &external.retained_wrapper_sha256
            )
            .unwrap(),
            fixture.wrapper.bytes
        );
        let archives: Vec<_> = fs::read_dir(fixture.wrapper.path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".openclaw-tauri-interrupted-")
            })
            .collect();
        assert_eq!(archives.len(), 1);
        assert_eq!(fs::read(&archives[0]).unwrap(), pending_bytes);
        assert_eq!(
            fs::metadata(&archives[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let external_bytes = fs::read(&fixture.wrapper.path).unwrap();
        assert_eq!(
            fixture.migrate(Mode::OwnedUpdate).unwrap(),
            MigrationOutcome::PreservedExternal
        );
        assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), external_bytes);
        assert_eq!(fixture.current_bytes(), service_bytes, "{case}");
        assert_eq!(
            fixture.calls(),
            "",
            "no automatic failure rollback for {case}"
        );
        assert!(fs::read_to_string(fixture.fixture.0.join("inspections"))
            .unwrap()
            .lines()
            .all(|line| line.starts_with("node ") && line.contains("--no-probe")));
    }
}

#[test]
fn interrupted_original_binding_recovers_before_installing_and_keeps_pause() {
    for paused in [false, true] {
        let fixture = MigrationFixture::new();
        let mut original = fixture.fixture.state_json(&fixture.wrapper);
        if paused {
            original["service"]["runtime"]["status"] = "stopped".into();
        }
        fixture.write_state("current", &original);
        let metadata = fixture.publish_pending(&original, Mode::Adopt);
        let pending_bytes = fs::read(&fixture.wrapper.path).unwrap();
        if paused {
            for mode in [Mode::OwnedUpdate, Mode::Adopt, Mode::Fresh] {
                assert_eq!(
                    fixture.migrate(mode).unwrap(),
                    MigrationOutcome::DeferredPaused
                );
                assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), pending_bytes);
            }
            assert!(restore_retained_node(&fixture.cli, &|| true)
                .unwrap_err()
                .contains("Start it before"));
            assert_eq!(fixture.calls(), "");
        } else {
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::Migrated
            );
            assert_eq!(fixture.calls(), "repair\ninstall-bun\n");
            let completed = read_wrapper(&fixture.cli).unwrap().managed.unwrap();
            assert_eq!(completed.purpose, Purpose::Gateway);
            assert!(completed.pending.is_none());
            assert_eq!(completed.binding.as_ref().unwrap().pin_revision, "bun-pin");
            assert_eq!(
                verified_backup(
                    &metadata.retained_wrapper,
                    &metadata.retained_wrapper_sha256
                )
                .unwrap(),
                fixture.wrapper.bytes
            );
            assert_eq!(
                verified_backup(
                    &completed.retained_wrapper,
                    &completed.retained_wrapper_sha256
                )
                .unwrap(),
                fixture.wrapper.bytes
            );
        }
    }
}

#[test]
fn external_runtime_requires_explicit_readoption_or_verified_node_restore() {
    for action in ["readopt", "readopt-pending", "restore"] {
        let fixture = MigrationFixture::new();
        let metadata =
            fixture.publish_pending(&fixture.fixture.state_json(&fixture.wrapper), Mode::Adopt);
        fixture.write_state("current", &fixture.bun_state());
        if action != "readopt-pending" {
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::PreservedExternal
            );
        }
        assert_eq!(fixture.calls(), "");
        if action == "restore" {
            let mut unsupported = fixture.bun_state();
            unsupported["cli"]["runtime"]["supported"] = false.into();
            fixture.write_state("current", &unsupported);
            let external_bytes = fs::read(&fixture.wrapper.path).unwrap();
            assert!(restore_retained_node(&fixture.cli, &|| true)
                .unwrap_err()
                .contains("retained Node"));
            assert_eq!(fixture.calls(), "");
            assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), external_bytes);
            fixture.write_state("current", &fixture.bun_state());
            restore_retained_node(&fixture.cli, &|| true).unwrap();
            assert_eq!(fixture.calls(), "install-node\n");
            assert_eq!(
                fs::read(&fixture.wrapper.path).unwrap(),
                fixture.wrapper.bytes
            );
            assert!(!has_gateway_runtime_record(&fixture.cli).unwrap());
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::PreservedExternal
            );
        } else {
            assert_eq!(
                fixture.migrate(Mode::Adopt).unwrap(),
                MigrationOutcome::Migrated
            );
            assert_eq!(fixture.calls(), "repair\ninstall-bun\n");
            let completed = read_wrapper(&fixture.cli).unwrap().managed.unwrap();
            assert_eq!(completed.purpose, Purpose::Gateway);
            assert_eq!(completed.retained_wrapper, metadata.retained_wrapper);
            assert!(completed.owns(&capture(&fixture.cli, None, true).unwrap()));
            assert_eq!(interrupted_notice(&fixture.cli), None);
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::Current
            );
        }
    }
}

#[test]
fn external_readoption_preserves_other_pins_and_paused_services() {
    for case in ["other-pin", "paused"] {
        let fixture = MigrationFixture::new();
        fixture.publish_pending(&fixture.fixture.state_json(&fixture.wrapper), Mode::Adopt);
        let mut state = fixture.bun_state();
        if case == "other-pin" {
            state["service"]["runtimeIntent"]["pin"]["path"] = "/independent/bun".into();
        } else {
            state["service"]["runtime"]["status"] = "stopped".into();
        }
        fixture.write_state("current", &state);
        assert_eq!(
            fixture.migrate(Mode::OwnedUpdate).unwrap(),
            MigrationOutcome::PreservedExternal
        );
        let external_bytes = fs::read(&fixture.wrapper.path).unwrap();
        let service_bytes = fixture.current_bytes();
        if case == "other-pin" {
            assert!(fixture.migrate(Mode::Adopt).is_err());
        } else {
            assert_eq!(
                fixture.migrate(Mode::Adopt).unwrap(),
                MigrationOutcome::DeferredPaused
            );
            assert!(restore_retained_node(&fixture.cli, &|| true)
                .unwrap_err()
                .contains("Start it before"));
        }
        assert_eq!(fixture.calls(), "");
        assert_eq!(fixture.current_bytes(), service_bytes);
        assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), external_bytes);
    }
}

#[test]
fn interrupted_intent_keeps_pending_marker_when_a_recovery_backup_is_modified() {
    for retained_node in [false, true] {
        let fixture = MigrationFixture::new();
        let metadata =
            fixture.publish_pending(&fixture.fixture.state_json(&fixture.wrapper), Mode::Adopt);
        fixture.write_state("current", &fixture.bun_state());
        let pending_bytes = fs::read(&fixture.wrapper.path).unwrap();
        let backup = if retained_node {
            &metadata.retained_wrapper
        } else {
            &metadata.pending.as_ref().unwrap().original_wrapper
        };
        fs::write(backup, "changed recovery bytes").unwrap();
        assert!(fixture
            .migrate(Mode::OwnedUpdate)
            .unwrap_err()
            .contains("recovery wrapper changed"));
        assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), pending_bytes);
        assert_eq!(fixture.calls(), "");
    }
}

#[test]
fn fresh_startup_uses_migration_admission_and_preserves_existing_services_or_pins() {
    for case in ["absent", "retained-pin", "existing"] {
        let fixture = MigrationFixture::new();
        let metadata = managed_metadata(
            &fixture.wrapper,
            bundled_target(&fixture.runtime, &fixture.wrapper.node.entry).unwrap(),
            "2026.10.1".into(),
            None,
        )
        .unwrap();
        fixture.publish_metadata(&metadata);
        let mut state = fixture.fixture.state_json(&fixture.wrapper);
        if case != "existing" {
            state["service"]["loaded"] = false.into();
            state["service"]["command"] = Value::Null;
            state["service"]["runtime"]["status"] = "stopped".into();
        }
        if case == "retained-pin" {
            state["service"]["runtimeIntent"]["stored"] = true.into();
        }
        fixture.write_state("current", &state);
        let wrapper_bytes = fs::read(&fixture.wrapper.path).unwrap();
        let service_bytes = fixture.current_bytes();
        if case == "absent" {
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::Migrated
            );
            assert_eq!(fixture.calls(), "install-bun\n");
        } else {
            assert_eq!(
                fixture.migrate(Mode::OwnedUpdate).unwrap(),
                MigrationOutcome::PreservedExternal
            );
            assert!(fixture.migrate(Mode::Fresh).is_err());
            assert_eq!(fixture.calls(), "");
            assert_eq!(fixture.current_bytes(), service_bytes);
            assert_eq!(fs::read(&fixture.wrapper.path).unwrap(), wrapper_bytes);
        }
    }
}
