#!/usr/bin/env ruby
# frozen_string_literal: true

# screenshots.rb — re-capture the GUIs of already-running VMs, without re-running
# any scenario. Launches each VM's software-rendered GUI (auto-unlock, auto-open
# chat) and writes scenarios/<timestamp>/<vm>.png (gitignored).
#
# Usage:  ruby scripts/scenarios/screenshots.rb [N]   (default 5)

require_relative 'utils'

log "screenshots-only: capturing GUIs of #{N} existing VMs"
capture_screenshots
