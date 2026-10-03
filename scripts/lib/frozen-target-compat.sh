#!/usr/bin/env bash

openclaw_frozen_target_omissions_authorized() {
  case "${OPENCLAW_ALLOW_FROZEN_TARGET_SCENARIO_OMISSIONS:-0}" in
    0 | "")
      return 1
      ;;
    1) ;;
    *)
      echo "invalid OPENCLAW_ALLOW_FROZEN_TARGET_SCENARIO_OMISSIONS: expected 0 or 1" >&2
      return 2
      ;;
  esac

  if [[ ! "${OPENCLAW_SELECTED_SHA:-}" =~ ^[0-9a-f]{40}$ ]]; then
    echo "OPENCLAW_SELECTED_SHA must be a full lowercase commit SHA" >&2
    return 2
  fi
  if [[ ! "${OPENCLAW_TOOLING_SHA:-}" =~ ^[0-9a-f]{40}$ ]]; then
    echo "OPENCLAW_TOOLING_SHA must be a full lowercase commit SHA" >&2
    return 2
  fi
  if [[ "$OPENCLAW_SELECTED_SHA" == "$OPENCLAW_TOOLING_SHA" ]]; then
    echo "frozen-target omissions require distinct selected and tooling SHAs" >&2
    return 2
  fi
}

openclaw_prepare_frozen_target_context() {
  local source_root="${1:?missing selected source root}" authorization_status=0

  openclaw_frozen_target_omissions_authorized || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 1
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  openclaw_frozen_target_source validate "$source_root" || return 2
}

openclaw_resolve_frozen_target_file() {
  local source_root="${1:?missing selected source root}" \
    relative_path="${2:?missing selected relative path}" \
    fallback_path="${3:-}" context_status=0 source_status=0 source_operation=has
  local frozen_missing_path="${4-$fallback_path}"

  openclaw_prepare_frozen_target_context "$source_root" || context_status=$?
  case "$context_status" in
    0)
      # The shipped survivor scenario is the sole directory-owned caller.
      [ "$relative_path" != scripts/e2e/lib/upgrade-survivor ] || source_operation=directory
      openclaw_frozen_target_source "$source_operation" "$source_root" "$relative_path" || source_status=$?
      case "$source_status" in
        0) printf '%s\n' "$source_root/$relative_path" ;;
        1) printf '%s\n' "$frozen_missing_path" ;;
        *) return "$source_status" ;;
      esac
      return 0
      ;;
    1) ;;
    *) return "$context_status" ;;
  esac
  printf '%s\n' "$fallback_path"
}

openclaw_frozen_target_source() {
  local operation="${1:?missing source operation}" source_root="${2:?missing selected source root}" helper
  shift 2
  helper="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/frozen-target-source.mjs" || return 2
  node "$helper" "$operation" "$source_root" "${OPENCLAW_SELECTED_SHA:-}" "$@"
}

openclaw_frozen_target_source_has_path() {
  openclaw_frozen_target_source has "$@"
}

openclaw_frozen_target_source_contains() {
  openclaw_frozen_target_source contains "$@"
}

# Boolean data and read errors travel separately, including inside caller `if`s.
openclaw_frozen_target_source_flag() {
  local status=0
  openclaw_frozen_target_source "$@" || status=$?
  case "$status" in
    0) printf '1' ;;
    1) printf '0' ;;
    *) return "$status" ;;
  esac
}

openclaw_resolve_frozen_upgrade_survivor_capabilities() {
  local source_root="${1:?missing selected source root}" authorization_status=0 has_tool_search_recipe
  local has_membership_warning has_absent_membership

  export OPENCLAW_FROZEN_UPGRADE_SURVIVOR_TOOL_SEARCH_RECIPE="current" \
    OPENCLAW_FROZEN_UPGRADE_SURVIVOR_MEMBERSHIP_MODE="absent"
  openclaw_prepare_frozen_target_context "$source_root" || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 0
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  # New tooling may author a migration specimen absent from the selected cut.
  # Bind coverage to its committed recipe, not release version or migrated state.
  has_tool_search_recipe="$(openclaw_frozen_target_source_flag has "$source_root" \
    scripts/e2e/lib/upgrade-survivor/config-recipe/tools-tool-search.json)" || return 2
  if [ "$has_tool_search_recipe" = 0 ]; then
    export OPENCLAW_FROZEN_UPGRADE_SURVIVOR_TOOL_SEARCH_RECIPE="absent"
  fi

  # Preserve native containment for cuts predating absent-membership recovery.
  # Inspect the committed result owner, never package versions or observed output.
  has_membership_warning="$(openclaw_frozen_target_source_flag contains "$source_root" \
    src/cli/update-cli/update-command-terminal-publication.ts \
    'Service membership unverifiable on this host; using managed stop/update/start.')" || return 2
  has_absent_membership="$(openclaw_frozen_target_source_flag contains "$source_root" \
    src/cli/update-cli/update-command-terminal-publication.ts 'serviceMembershipSourceAbsent')" || return 2
  case "$has_membership_warning:$has_absent_membership" in
    1:1) ;;
    0:0) export OPENCLAW_FROZEN_UPGRADE_SURVIVOR_MEMBERSHIP_MODE="native" ;;
    *)
      echo "unrecognized selected managed-service membership warning contract" >&2
      return 2
      ;;
  esac
}

openclaw_resolve_frozen_plugin_harness_capabilities() {
  local source_root="${1:?missing selected source root}" authorization_status=0 has_old has_new

  export OPENCLAW_FROZEN_TARGET_PLUGIN_UNINSTALL_MODE="current"

  openclaw_prepare_frozen_target_context "$source_root" || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 0
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  # The old plugin sweep asserted removal but predated the canonical disabled
  # marker. Only that selected, packaged assertion dialect may relax the marker.
  has_old="$(openclaw_frozen_target_source_flag contains "$source_root" scripts/e2e/lib/plugins/assertions.mjs 'function assertPluginTgzRemoved()')" || return 2
  if [ "$has_old" = 1 ]; then
    has_new="$(openclaw_frozen_target_source_flag contains "$source_root" scripts/e2e/lib/plugins/assertions.mjs 'function assertPluginUninstallConfigState(')" || return 2
    if [ "$has_new" = 0 ]; then
      export OPENCLAW_FROZEN_TARGET_PLUGIN_UNINSTALL_MODE="legacy"
    fi
  fi
}

openclaw_append_frozen_plugin_harness_docker_env() {
  if [[ "${OPENCLAW_FROZEN_TARGET_PLUGIN_UNINSTALL_MODE:-current}" == "legacy" ]]; then
    DOCKER_ENV_ARGS+=( -e "OPENCLAW_FROZEN_TARGET_PLUGIN_UNINSTALL_MODE=legacy" )
  fi
}

openclaw_resolve_frozen_typed_onboarding_contract() {
  local source_root="${1:?missing selected source root}" harness_root="${2:?missing trusted harness root}" authorization_status=0
  local scenario assertions assertion_files mock_config

  export OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_SCENARIO_PATH="$harness_root/scripts/e2e/lib/release-typed-onboarding/scenario.sh" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTIONS_PATH="$harness_root/scripts/e2e/lib/release-scenarios/assertions.mjs" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTION_FILES_PATH="$harness_root/scripts/e2e/lib/release-assertion-files.mjs" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_MOCK_CONFIG_PATH="$harness_root/scripts/e2e/lib/fixtures/mock-openai-config.mjs"

  openclaw_prepare_frozen_target_context "$source_root" || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 0
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  # The shipped journey, assertions and config writer share one consumer owner.
  scenario="$(openclaw_resolve_frozen_target_file "$source_root" \
    scripts/e2e/lib/release-typed-onboarding/scenario.sh \
    "$OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_SCENARIO_PATH")" || return 2
  assertions="$(openclaw_resolve_frozen_target_file "$source_root" \
    scripts/e2e/lib/release-scenarios/assertions.mjs \
    "$OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTIONS_PATH")" || return 2
  assertion_files="$(openclaw_resolve_frozen_target_file "$source_root" \
    scripts/e2e/lib/release-assertion-files.mjs \
    "$OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTION_FILES_PATH")" || return 2
  mock_config="$(openclaw_resolve_frozen_target_file "$source_root" \
    scripts/e2e/lib/fixtures/mock-openai-config.mjs \
    "$OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_MOCK_CONFIG_PATH")" || return 2
  export OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_SCENARIO_PATH="$scenario" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTIONS_PATH="$assertions" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_ASSERTION_FILES_PATH="$assertion_files" \
    OPENCLAW_FROZEN_TARGET_TYPED_ONBOARDING_MOCK_CONFIG_PATH="$mock_config"
}

openclaw_resolve_frozen_session_cold_storage_contract() {
  local source_root="${1:?missing selected source root}" authorization_status=0 has_current has_cold has_legacy
  local has_legacy_duration

  export OPENCLAW_FROZEN_TARGET_SESSION_COLD_STORAGE_MODE="required"
  openclaw_prepare_frozen_target_context "$source_root" || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 0
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  # Cold storage introduced the split session schema. A current target with a
  # broken declaration must fail its tests rather than be treated as pre-feature.
  has_current="$(openclaw_frozen_target_source_flag has "$source_root" src/config/zod-schema.session-config.ts)" || return 2
  [ "$has_current" = 0 ] || return 0
  has_cold="$(openclaw_frozen_target_source_flag contains "$source_root" src/config/zod-schema.session.ts 'coldStorage:')" || return 2
  [ "$has_cold" = 0 ] || return 0
  has_legacy="$(openclaw_frozen_target_source_flag contains "$source_root" src/config/zod-schema.session.ts 'export const SessionSchema = z')" || return 2
  if [ "$has_legacy" = 1 ]; then
    has_legacy_duration="$(openclaw_frozen_target_source_flag contains "$source_root" src/config/zod-schema.session.ts 'pruneAfter: PositiveDurationSchema.optional()')" || return 2
    if [ "$has_legacy_duration" = 1 ]; then
      export OPENCLAW_FROZEN_TARGET_SESSION_COLD_STORAGE_MODE="unsupported"
      return 0
    fi
  fi
  echo "unable to resolve frozen session cold-storage contract from selected source" >&2
  return 2
}

openclaw_resolve_frozen_runtime_context_contract() {
  local source_root="${1:?missing selected source root}" authorization_status=0
  local has_migrations has_repair has_extract has_model_prompt has_fragments has_filter

  export OPENCLAW_FROZEN_TARGET_RUNTIME_CONTEXT_INPUT_MODE="producer-fragments" \
    OPENCLAW_FROZEN_TARGET_SESSION_REPAIR_MODE="sqlite"
  openclaw_prepare_frozen_target_context "$source_root" || authorization_status=$?
  [ "$authorization_status" -eq 1 ] && return 0
  [ "$authorization_status" -eq 0 ] || return "$authorization_status"

  has_migrations="$(openclaw_frozen_target_source_flag has "$source_root" src/state/openclaw-agent-db-session-migrations.ts)" || return 2
  if [ "$has_migrations" = 0 ]; then
    has_repair="$(openclaw_frozen_target_source_flag contains "$source_root" src/commands/doctor-session-transcripts.ts '.pre-doctor-branch-repair-')" || return 2
    if [ "$has_repair" = 1 ]; then
      export OPENCLAW_FROZEN_TARGET_SESSION_REPAIR_MODE="jsonl"
    fi
  fi

  local runtime_context_path="src/agents/embedded-agent-runner/run/runtime-context-prompt.ts"
  local has_legacy_runtime_context=0 has_producer_runtime_context=0
  has_extract="$(openclaw_frozen_target_source_flag contains "$source_root" "$runtime_context_path" 'extractInternalRuntimeContext')" || return 2
  if [ "$has_extract" = 1 ]; then
    has_model_prompt="$(openclaw_frozen_target_source_flag contains "$source_root" "$runtime_context_path" 'modelPrompt?: string;')" || return 2
    has_legacy_runtime_context="$has_model_prompt"
  fi
  has_fragments="$(openclaw_frozen_target_source_flag contains "$source_root" "$runtime_context_path" 'fragments?: RuntimeContextFragment[];')" || return 2
  if [ "$has_fragments" = 1 ]; then
    has_filter="$(openclaw_frozen_target_source_flag contains "$source_root" "$runtime_context_path" 'const fragments = params.fragments?.filter')" || return 2
    has_producer_runtime_context="$has_filter"
  fi
  case "$has_producer_runtime_context:$has_legacy_runtime_context" in
    1:0) ;;
    0:1)
      export OPENCLAW_FROZEN_TARGET_RUNTIME_CONTEXT_INPUT_MODE="legacy-marked-prompt"
      ;;
    *)
      echo "unable to resolve frozen runtime-context input contract from selected source" >&2
      return 2
      ;;
  esac
}
