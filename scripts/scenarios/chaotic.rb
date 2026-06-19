#!/usr/bin/env ruby
# frozen_string_literal: true

# chaotic.rb — simultaneous-blast scenario.
#
# Everyone in one group, then ALL VMs blast messages AT THE SAME TIME. Ordering
# is irrelevant — this only confirms that every VM eventually received every
# message, and exits non-zero if any VM is missing any.
#
# Usage:  ruby scripts/scenarios/chaotic.rb [N]   (default 5; min 2)
# Env:    DM_CHAOS_MSGS — messages each VM fires (default 3; total = N × this)

require_relative 'utils'

CHAOS_MSGS = (ENV['DM_CHAOS_MSGS'] || '3').to_i

# The unique payload VM `vm` emits as its k-th message. START_TAG makes it
# unique per run so a re-run's messages can never be mistaken for this one's.
def chaos_text(vm, k) = "chaos #{START_TAG} #{vm} seq#{k}"

# Every joined VM fires CHAOS_MSGS messages at once. One thread per VM (the sends
# are blocking subprocess calls, so Ruby threads give real overlap), each thread
# firing back-to-back. No ordering, no turn-taking — maximum interleave.
def chaos_blast(joined, group)
  total = joined.size * CHAOS_MSGS
  log "💥 chaos: #{joined.size} VMs × #{CHAOS_MSGS} msgs = #{total} messages, all at once…"
  sent = Hash.new(0)
  mutex = Mutex.new
  joined.map do |vm|
    Thread.new do
      CHAOS_MSGS.times do |k|
        ok = retry_until("#{vm} send seq#{k}", tries: 10, wait: 5) { dm(vm, 'send', group, chaos_text(vm, k)) }
        mutex.synchronize { sent[vm] += 1 if ok }
      end
    end
  end.each(&:join)
  joined.each { |vm| warn_("#{vm} only sent #{sent[vm]}/#{CHAOS_MSGS}") if sent[vm] < CHAOS_MSGS }
  log "  sent #{sent.values.sum}/#{total} total"
end

def run_chaotic(npubs)
  group, joined = build_full_group(npubs, name: 'Chaos')
  chaos_blast(joined, group)
  expected = joined.flat_map { |vm| (0...CHAOS_MSGS).map { |k| chaos_text(vm, k) } }.to_set
  missing, status = verify_all_received(joined, group, expected)
  outcome = report_delivery(joined, expected, missing,
                            "chaos result: #{joined.size} VMs, #{expected.size} unique messages each",
                            status: status)
  [group, outcome]
end

die("need at least 2 VMs (got #{N})") if N < 2
log 'mode: chaotic blast'
npubs = boot_scenario
group, outcome = run_chaotic(npubs)
finish(group, outcome == :pass, forked: outcome == :fork)
