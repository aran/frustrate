#!/usr/bin/env bash
# Called by bazel-contrib's release_ruleset workflow with the tag; what it
# prints becomes the release notes. See overhead/release/prep.dart.
set -o errexit -o nounset -o pipefail

bazel run --noshow_progress --ui_event_filters=-info @overhead//release:prep -- "$1"
