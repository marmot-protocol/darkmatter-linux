#!/usr/bin/env ruby
# frozen_string_literal: true

# cheeky.rb — "cheeky admins" convergence scenario (default 10 min).
#
# Everyone in one group, and EVERYONE is made an admin. Then for the duration,
# every VM independently and at RANDOM intervals does a random cheeky thing:
#   • send a message,
#   • rename the group,
#   • promote a random other member to admin,
#   • demote a random other member.
#
# Renames/promotes/demotes are MLS commits, and MLS allows only one commit per
# epoch — so concurrent admins constantly race, one wins and the rest are
# rejected and must catch up. The whole point of the test is the AFTERMATH:
# once the chaos stops and things settle, do all VMs converge on a single agreed
# state? We check that every VM agrees on:
#   • the group name,
#   • the admin set,
#   • the member set,
# and that every VM received every message that was sent (order irrelevant).
#
# (Admin role IS observable: group-list carries admin_policy.admins; group name
# is profile.name. There's no global oracle for "the correct" final state —
# it's whatever the MLS DAG converged to — so we assert all VMs are IDENTICAL,
# which is exactly "we can agree on a state".)
#
# Usage:  ruby scripts/scenarios/cheeky.rb [N]   (default 5; min 3 to be cheeky)
# Env:    DM_CHEEKY_SECS — run duration in seconds (default 600)

require_relative 'utils'

def cheeky_text(vm, k) = "cheeky #{START_TAG} #{vm} seq#{k}"

# Make every member an admin up front. vm1 is already admin (creator); it
# promotes the rest one at a time (each is its own commit, so serialise and
# retry past epoch races). Best-effort: a member that never lands as admin just
# gets fewer cheeky powers.
def promote_everyone(joined, npubs, group)
  (joined - ['vm1']).each do |vm|
    ok = retry_until("promote #{vm}", tries: 8, wait: 5) { dm('vm1', 'promote', group, npubs[vm]) }
    log(ok ? "  👑 #{vm} promoted to admin" : "  (#{vm} promote never landed)")
  end
  retry_until('all admins visible', tries: 15, wait: 6) do
    v = group_view('vm1', group)
    v && v[:admins].size >= joined.size
  end
end

# One VM's cheeky loop: until time's up, sleep a random beat then do one random
# action. Admin actions are commits that lose ~most epoch races under load, so
# we DON'T retry them — a lost race is realistic churn; the next beat tries
# again. Only message sends are recorded (into `sent`), since messages are the
# thing we verify everyone received. `counters` tracks action tallies for the
# report. All shared state is touched under `lock`.
def cheeky_loop(vm, group, others, st, end_at, counters, lock)
  Thread.new do
    until monotonic >= end_at
      sleep(rand(2.0..8.0))
      break if monotonic >= end_at
      case rand(100)
      when 0...45 # send — never conflicts (application message, not a commit)
        k = nil
        lock.synchronize { k = st[vm].size }
        text = cheeky_text(vm, k)
        if dm(vm, 'send', group, text)
          lock.synchronize { st[vm] << text }
        end
      when 45...70 # rename
        n = nil
        lock.synchronize { n = (counters[:rename] += 1) }
        dm(vm, 'rename', group, "#{vm}-name-#{n}")
      when 70...88 # promote a random other
        target = others.sample
        dm(vm, 'promote', group, target[:npub]) && lock.synchronize { counters[:promote] += 1 }
      else # demote a random other
        target = others.sample
        dm(vm, 'demote', group, target[:npub]) && lock.synchronize { counters[:demote] += 1 }
      end
    end
  end
end

# After the storm, poll until every VM reports an IDENTICAL (name, admins,
# members) view — i.e. the group has quiesced and converged. Mirrors
# verify_all_received's fork detection: if the VMs stay split the same way for
# `fork_after` checks, the group has FORKED on its state, not just lagging.
# Returns [status, views_by_vm] with status :converged / :forked / :incomplete.
def await_convergence(joined, group, tries: 30, fork_after: 5)
  log 'waiting for all VMs to converge on one group state…'
  views = {}
  status = :incomplete
  prev_sig = nil
  stuck = 0
  retry_until('all VMs agree on group state', tries: tries, wait: 8) do
    joined.each { |vm| views[vm] = group_view(vm, group) }
    present = views.values.compact
    if present.size == joined.size && present.uniq.size == 1
      status = :converged
      next true
    end
    if views == prev_sig
      stuck += 1
    else
      stuck = 1
      prev_sig = views.dup
    end
    distinct = present.uniq.size
    log "  …not agreed yet (#{distinct} distinct views among #{present.size}/#{joined.size})" \
        "#{stuck > 1 ? " (no change ×#{stuck})" : ''}"
    if stuck >= fork_after
      log "  🔱 FORK DETECTED — group state stuck split across #{distinct} views"
      status = :forked
      next true
    end
    false
  end
  [status, views]
end

def run_cheeky(npubs)
  group, joined = build_full_group(npubs, name: 'Cheeky')
  die("need at least 3 VMs to be cheeky (got #{joined.size})") if joined.size < 3
  promote_everyone(joined, npubs, group)

  secs = (ENV['DM_CHEEKY_SECS'] || '600').to_i
  st = joined.each_with_object({}) { |vm, h| h[vm] = [] }       # per-VM sent payloads
  counters = { rename: 0, promote: 0, demote: 0 }
  lock = Mutex.new
  end_at = monotonic + secs

  log "😈 cheeky admins: #{joined.size} all-admin VMs jwoobing about for #{secs}s…"
  threads = joined.map do |vm|
    # Everyone else, as {vm,npub}, for random promote/demote targets.
    others = (joined - [vm]).map { |o| { vm: o, npub: npubs[o] } }
    cheeky_loop(vm, group, others, st, end_at, counters, lock)
  end
  threads.each(&:join)

  log "storm over (#{counters[:rename]} renames, #{counters[:promote]} promotes, " \
      "#{counters[:demote]} demotes) — letting MLS settle…"

  # 1) message delivery: everyone got every sent message.
  expected = joined.flat_map { |vm| st[vm] }.to_set
  log "sent during storm: #{expected.size} messages (#{joined.map { |vm| "#{vm}:#{st[vm].size}" }.join(' ')})"
  missing, msg_status = verify_all_received(joined, group, expected, tries: 45)
  msg_outcome = report_delivery(joined, expected, missing, "message delivery: #{expected.size} messages",
                                status: msg_status)

  # 2) state convergence: everyone agrees on name + admins + members.
  conv_status, views = await_convergence(joined, group)
  log '─' * 60
  log 'final agreed state:'
  if conv_status == :converged
    v = views[joined.first]
    log "  🎉 PASS — all #{joined.size} VMs agree:"
    log "    name:    #{v[:name].inspect}"
    log "    admins:  #{v[:admins].size} (#{v[:admins].map { |a| a[0, 8] }.join(', ')})"
    log "    members: #{v[:members]&.size}"
  else
    log(conv_status == :forked ? '  🔱 FORK — VMs wedged on different group states:' \
                               : '  ❌ FAIL — VMs did not converge on one state:')
    views.each do |vm, view|
      log "    #{vm}: name=#{view&.dig(:name).inspect} admins=#{view&.dig(:admins)&.size} members=#{view&.dig(:members)&.size}"
    end
  end
  log '─' * 60

  # Either a wedged message set or a wedged group state is a fork — exactly the
  # divergence this scenario exists to surface.
  forked = msg_outcome == :fork || conv_status == :forked
  passed = msg_outcome == :pass && conv_status == :converged
  [group, passed, forked]
end

log 'mode: cheeky admins'
npubs = boot_scenario
group, passed, forked = run_cheeky(npubs)
finish(group, passed, forked: forked)
