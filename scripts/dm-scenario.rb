#!/usr/bin/env ruby
# frozen_string_literal: true

# dm-scenario.rb — first end-to-end multi-VM automation for Dark Matter Linux.
#
# Spins up N VMs (default 5), each a distinct Nostr identity, then builds a group
# incrementally and exercises real cross-device messaging over the whitenoise
# relays:
#
#   1. vm1 creates a group inviting vm2
#   2. vm2 accepts; vm1 and vm2 each send a message
#   3. rename the group
#   4. invite vm3; it accepts; all 3 send
#   5. invite vm4; all 4 send … and so on through vmN
#
# Each VM is a thin clone of an already-provisioned "golden" VM (default), so we
# pay cloud-init once, not N times. All control goes through the headless dm-ctl
# daemon (`dmvm --vm X dm …`); no GUI is involved. Cross-VM delivery is
# eventually-consistent (relay round-trips + MLS epoch commits), so every step
# retries.
#
# Usage:  ruby scripts/dm-scenario.rb [N]
# Env:    DM_SCENARIO_GOLDEN (default "default"), DMVM_MEM_MB, DMVM_CPUS

require 'json'
require 'open3'

DMVM   = File.expand_path('dmvm.rb', __dir__)
N      = (ARGV[0] || ENV['DM_SCENARIO_N'] || '5').to_i
GOLDEN = ENV['DM_SCENARIO_GOLDEN'] || 'default'
VMS    = (1..N).map { |i| "vm#{i}" }
RELAYS_JSON = JSON.generate(%w[wss://relay.eu.whitenoise.chat wss://relay.us.whitenoise.chat])

# Keep each VM small so N of them fit in host RAM.
ENV['DMVM_MEM_MB'] ||= '2048'
ENV['DMVM_CPUS']   ||= '2'

def log(msg) = puts("\e[36m[scenario]\e[0m #{msg}")
def warn_(msg) = puts("\e[33m[warn]\e[0m #{msg}")
def die(msg) = (puts("\e[31m[fatal]\e[0m #{msg}"); exit(1))

# Run a dmvm command for a VM. Returns [stdout, ok?].
def dmvm(vm, *args)
  out, _err, st = Open3.capture3('ruby', DMVM, '--vm', vm, *args)
  [out, st.success?]
end

def dmvm!(vm, *args)
  out, ok = dmvm(vm, *args)
  die("`dmvm --vm #{vm} #{args.join(' ')}` failed:\n#{out}") unless ok
  out
end

# Run a dm-ctl command and parse its JSON result (nil on failure).
def dm(vm, *cmd)
  out, ok = dmvm(vm, 'dm', *cmd)
  return nil unless ok
  JSON.parse(out)
rescue JSON::ParserError
  out.strip
end

# Retry a block until it returns truthy, or give up.
def retry_until(desc, tries: 12, wait: 5)
  tries.times do |i|
    r = yield
    return r if r
    sleep wait
  end
  warn_("gave up: #{desc}")
  nil
end

# ---- phase 1: prepare the golden image --------------------------------------

def prepare_golden
  log "preparing golden VM '#{GOLDEN}' (provision once, then clone)…"
  dmvm!(GOLDEN, 'up') # provisions on first run (slow); no-op if already up
  dmvm!(GOLDEN, 'push', 'dm-ctl') # the freshly cross-built control binary
  # Seed relays + wipe any identity so every clone generates its own fresh nsec.
  dmvm!(GOLDEN, 'ssh',
        "pkill -9 -f 'dm-ctl' 2>/dev/null; pkill -9 -f 'darkmatter-linux$' 2>/dev/null; " \
        "mkdir -p ~/.config/darkmatter-linux && printf '%s' '#{RELAYS_JSON}' > ~/.config/darkmatter-linux/relays.json; " \
        'find ~/dm-home -mindepth 1 -delete 2>/dev/null; true')
  dmvm!(GOLDEN, 'down') # must be stopped: clones back onto its disk
  log 'golden ready (stopped).'
end

# ---- phase 2: clone + boot N VMs --------------------------------------------

def spin_up_vms
  VMS.each { |vm| dmvm!(vm, 'clone', GOLDEN) }
  log "booting #{N} clones in parallel…"
  VMS.map { |vm| Thread.new { dmvm(vm, 'up') } }.each(&:join)

  npubs = {}
  VMS.each do |vm|
    log "starting daemon + creating identity on #{vm}…"
    who = retry_until("#{vm} whoami", tries: 20, wait: 6) { dm(vm, 'whoami') }
    die("#{vm} never produced an identity") unless who.is_a?(Hash) && who['npub']
    npubs[vm] = who['npub']
    log "  #{vm} = #{who['npub']}"
  end
  npubs
end

# ---- phase 3: build the group incrementally ---------------------------------

def send_from_all(joined, group)
  joined.each do |vm|
    r = retry_until("send from #{vm}", tries: 10, wait: 5) { dm(vm, 'send', group, "hi from #{vm} — #{joined.size} in the room") }
    if r
      log "  ✉️  #{vm} sent (published to #{r['published'] || '?'} relays)"
    else
      warn_("#{vm} could not send")
    end
  end
end

def accept_invite(vm, group)
  retry_until("#{vm} receives welcome", tries: 20, wait: 6) do
    inv = dm(vm, 'invites')
    inv.is_a?(Array) && inv.any? { |g| g['group_id_hex'].to_s.casecmp?(group) }
  end or return false
  dm(vm, 'accept', group) ? true : false
end

def run_scenario(npubs)
  group = nil
  joined = ['vm1'] # vm1 is the creator/admin

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
        warn_("invite #{newcomer} failed")
    end

    log "#{newcomer} accepts the invite…"
    accept_invite(newcomer, group) ? (joined << newcomer) : warn_("#{newcomer} could not accept")

    name = "Squad of #{joined.size}"
    log "renaming group → \"#{name}\""
    retry_until('rename', tries: 6, wait: 4) { dm('vm1', 'rename', group, name) } || warn_('rename failed')

    log "everyone (#{joined.join(', ')}) sends a message…"
    send_from_all(joined, group)
    sleep 4 # let the epoch/messages settle before the next escalation
  end

  group
end

# ---- phase 4: report --------------------------------------------------------

def report(group)
  log '─' * 60
  log 'final group state (from vm1):'
  members = dm('vm1', 'group-members', group)
  msgs = dm('vm1', 'messages', group, '200')
  log "  members: #{members.is_a?(Array) ? members.size : '?'}"
  if msgs.is_a?(Array)
    chat = msgs.select { |m| m['kind'] == 9 }
    log "  chat messages seen by vm1: #{chat.size}"
    chat.last(12).each { |m| log "    #{m['sender'].to_s[0, 8]}…: #{m['plaintext']}" }
  end
  log '─' * 60
  log "done. inspect any VM with:  ruby scripts/dmvm.rb --vm vm1 dm messages #{group}"
end

# ---- main -------------------------------------------------------------------

die("need at least 2 VMs (got #{N})") if N < 2
log "scenario: #{N} VMs, golden=#{GOLDEN}, mem=#{ENV['DMVM_MEM_MB']}MiB cpus=#{ENV['DMVM_CPUS']}"
prepare_golden
npubs = spin_up_vms
group = run_scenario(npubs)
report(group)
