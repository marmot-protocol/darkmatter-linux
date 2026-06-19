#!/usr/bin/env ruby
# frozen_string_literal: true

# build.rb — escalating group-build scenario (the original end-to-end test).
#
# Spins up N VMs, each a distinct Nostr identity, then grows a group one member
# at a time over the whitenoise relays:
#   1. vm1 creates a group inviting vm2
#   2. vm2 accepts; vm1 and vm2 each send a message
#   3. rename the group
#   4. invite vm3; it accepts; all 3 send … and so on through vmN
#
# Cross-VM delivery is eventually-consistent (relay round-trips + MLS epoch
# commits), so every step retries.
#
# Usage:  ruby scripts/scenarios/build.rb [N]   (default 5; min 2)

require_relative 'utils'

# Returns true only if every VM in `joined` managed to send.
def send_from_all(joined, group)
  ok = true
  joined.each do |vm|
    r = retry_until("send from #{vm}", tries: 10, wait: 5) { dm(vm, 'send', group, "hi from #{vm} — #{joined.size} in the room") }
    if r
      log "  ✉️  #{vm} sent (published to #{r['published'] || '?'} relays)"
    else
      warn_("#{vm} could not send")
      ok = false
    end
  end
  ok
end

# Returns [group, passed]. `passed` is false if any newcomer failed to join or
# any VM failed to send — so a degraded run no longer exits 0.
def run_build(npubs)
  group = nil
  joined = ['vm1'] # vm1 is the creator/admin
  passed = true

  (2..N).each do |j|
    newcomer = "vm#{j}"
    if j == 2
      log "vm1 creates a group inviting #{newcomer}…"
      res = retry_until('group-create', tries: 8, wait: 6) { dm('vm1', 'group-create', 'Squad', npubs[newcomer]) }
      die('group-create failed') unless res.is_a?(Hash) && res['group_id_hex']
      group = res['group_id_hex']
      log "  group = #{group}"
    else
      log "vm1 invites #{newcomer}…"
      retry_until("invite #{newcomer}", tries: 10, wait: 6) { dm('vm1', 'invite', group, npubs[newcomer]) } ||
        (warn_("invite #{newcomer} failed"); passed = false)
    end

    log "#{newcomer} accepts the invite…"
    if accept_invite(newcomer, group)
      joined << newcomer
    else
      warn_("#{newcomer} could not accept")
      passed = false
    end

    name = "Squad of #{joined.size}"
    log "renaming group → \"#{name}\""
    retry_until('rename', tries: 6, wait: 4) { dm('vm1', 'rename', group, name) } || warn_('rename failed')

    log "everyone (#{joined.join(', ')}) sends a message…"
    passed = false unless send_from_all(joined, group)
    sleep 4 # let the epoch/messages settle before the next escalation
  end

  [group, passed]
end

die("need at least 2 VMs (got #{N})") if N < 2
log 'mode: escalating build'
npubs = boot_scenario
group, passed = run_build(npubs)
finish(group, passed)
