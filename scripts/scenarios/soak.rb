#!/usr/bin/env ruby
# frozen_string_literal: true

# soak.rb — long-running endurance / churn scenario (default 10 min).
#
# Everyone in one group; then, concurrently and at RANDOM intervals:
#   • every VM sends messages,
#   • the admin (vm1) renames the group,
#   • every non-admin VM disconnects and later reconnects (daemon stop/start —
#     a real client offline→online cycle, which resyncs from the relays on
#     reconnect).
# vm1 stays online so renames keep flowing. After the duration elapses we bring
# everyone back online, let delivery settle, and verify every VM received every
# message that was actually sent (order irrelevant). The expected set is the
# ground truth of what each VM *successfully* sent while online — so this proves
# offline VMs catch up across both missed messages AND missed epoch/rename
# commits.
#
# Usage:  ruby scripts/scenarios/soak.rb [N]   (default 5; min 2)
# Env:    DM_SOAK_SECS — run duration in seconds (default 600)

require_relative 'utils'

def soak_text(vm, k) = "soak #{START_TAG} #{vm} seq#{k}"

# Per-VM shared state: online flag + a lock serialising that VM's dm ops (so a
# send can't race the daemon being stopped) + the list of payloads it confirmed
# sending.
def soak_state(vms)
  vms.each_with_object({}) { |vm, h| h[vm] = { online: true, lock: Mutex.new, sent: [] } }
end

# One VM's message generator: while time remains, wait a random beat and — only
# if currently online — fire one uniquely-tagged message, recording it on
# success. No retry: a failed/offline beat is simply skipped, keeping cadence.
def soak_sender(vm, group, st, end_at)
  Thread.new do
    until monotonic >= end_at
      sleep(rand(2.0..9.0))
      st[:lock].synchronize do
        next unless st[:online]
        text = soak_text(vm, st[:sent].size)
        st[:sent] << text if dm(vm, 'send', group, text)
      end
    end
  end
end

# One non-admin VM's connection churn: alternate online/offline windows. We flip
# the flag under the lock (so any in-flight send finishes first and no new one
# starts), then stop/start the daemon outside the lock (start can take a while
# as it resyncs relays on reconnect).
def soak_churn(vm, st, end_at)
  Thread.new do
    until monotonic >= end_at
      sleep(rand(25.0..70.0)) # online window
      break if monotonic >= end_at
      st[:lock].synchronize { st[:online] = false }
      dmvm(vm, 'dm', 'stop')
      log "  🔌 #{vm} disconnected"
      sleep(rand(15.0..45.0)) # offline window
      dmvm(vm, 'dm', 'start') # reconnect: reboots backend → resyncs from relays
      st[:lock].synchronize { st[:online] = true }
      log "  🔁 #{vm} reconnected"
    end
  end
end

# The admin renames the group at random intervals; the running count lives in
# the shared `counter` so the report can read the final value.
def soak_renamer(group, end_at, counter)
  Thread.new do
    until monotonic >= end_at
      sleep(rand(20.0..50.0))
      break if monotonic >= end_at
      n = (counter[:n] += 1)
      dm('vm1', 'rename', group, "Chaos ##{n} #{START_TAG[-6..]}") && log("  ✏️  vm1 renamed → Chaos ##{n}")
    end
  end
end

def run_soak(npubs)
  group, joined = build_full_group(npubs, name: 'Soak')
  secs = (ENV['DM_SOAK_SECS'] || '600').to_i
  churners = joined - ['vm1'] # admin stays online so renames keep flowing
  st = soak_state(joined)
  counter = { n: 0 }
  end_at = monotonic + secs

  log "🌀 soak: #{joined.size} VMs messaging at random, vm1 renaming, " \
      "#{churners.join(', ')} churning — for #{secs}s…"
  threads = []
  joined.each   { |vm| threads << soak_sender(vm, group, st[vm], end_at) }
  churners.each { |vm| threads << soak_churn(vm, st[vm], end_at) }
  threads << soak_renamer(group, end_at, counter)
  threads.each(&:join)

  log 'duration elapsed — bringing everyone back online and letting delivery settle…'
  joined.each do |vm|
    st[vm][:online] = true
    dmvm(vm, 'dm', 'start') # make sure every daemon is up for the read-back
  end

  expected = joined.flat_map { |vm| st[vm][:sent] }.to_set
  sent_per_vm = joined.map { |vm| "#{vm}:#{st[vm][:sent].size}" }.join(' ')
  log "sent during soak: #{expected.size} messages (#{sent_per_vm}), #{counter[:n]} renames"
  # Longer retry budget than the blast: offline VMs may have a lot of backlog to
  # resync after their last reconnect.
  missing, status = verify_all_received(joined, group, expected, tries: 45)
  outcome = report_delivery(joined, expected, missing,
                            "soak result: #{joined.size} VMs, #{expected.size} messages sent, #{counter[:n]} renames",
                            status: status)
  [group, outcome]
end

die("need at least 2 VMs (got #{N})") if N < 2
log 'mode: soak / endurance'
npubs = boot_scenario
group, outcome = run_soak(npubs)
finish(group, outcome == :pass, forked: outcome == :fork)
