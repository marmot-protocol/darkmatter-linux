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
# There is also a "chaotic" mode (`ruby scripts/dm-scenario.rb chaotic [N]`):
# instead of building the group step by step, it puts everyone in one group up
# front, then ALL VMs blast messages at the same time. It does not care about
# ordering — it only confirms that every VM eventually received every message,
# and exits non-zero if any VM is missing any.
#
# Usage:  ruby scripts/dm-scenario.rb [N]                # escalating build
#         ruby scripts/dm-scenario.rb chaotic [N]        # simultaneous blast
#         ruby scripts/dm-scenario.rb screenshots [N]    # re-capture GUIs only
# Env:    DM_SCENARIO_GOLDEN (default "default"), DM_CHAOS_MSGS (default 3),
#         DMVM_MEM_MB, DMVM_CPUS

require 'json'
require 'open3'
require 'fileutils'
require 'set'

DMVM      = File.expand_path('dmvm.rb', __dir__)
REPO_ROOT = File.expand_path('..', __dir__)
# Filesystem-safe ISO-ish timestamp for this run's artifact folder.
START_TAG = Time.now.utc.strftime('%Y-%m-%dT%H%M%SZ')
# `dm-scenario.rb screenshots [N]` re-captures the GUIs of existing VMs without
# re-running the whole scenario.
SCREENSHOTS_ONLY = ARGV[0] == 'screenshots'
ARGV.shift if SCREENSHOTS_ONLY
# `dm-scenario.rb chaotic [N]` runs the chaos scenario instead of the escalating
# build: one group with everyone in it, all VMs blast messages simultaneously,
# then we confirm every VM received every message (order irrelevant).
CHAOTIC = ARGV[0] == 'chaotic'
ARGV.shift if CHAOTIC
N      = (ARGV[0] || ENV['DM_SCENARIO_N'] || '5').to_i
# Messages each VM fires during the chaos round (total expected = N * CHAOS_MSGS).
CHAOS_MSGS = (ENV['DM_CHAOS_MSGS'] || '3').to_i
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
  dmvm!(GOLDEN, 'build', '--host') # ensure both host binaries exist (incremental)
  dmvm!(GOLDEN, 'up')              # provisions on first run (slow); no-op if already up
  dmvm!(GOLDEN, 'push')            # push BOTH binaries (dm-ctl for the scenario, the GUI for screenshots)
  # Seed relays + wipe any identity so every clone generates its own fresh nsec.
  # NOTE: the `[d]` bracket trick stops pkill -f from matching this very ssh
  # command line (which contains the pattern) and killing its own shell before
  # `find` runs — that exact bug left the vault in place and gave every clone
  # the same identity.
  dmvm!(GOLDEN, 'ssh',
        "pkill -9 -f '[d]m-ctl' 2>/dev/null; pkill -9 -f '[d]arkmatter-linux' 2>/dev/null; sleep 1; " \
        "mkdir -p ~/.config/darkmatter-linux && printf '%s' '#{RELAYS_JSON}' > ~/.config/darkmatter-linux/relays.json; " \
        "find ~/dm-home -mindepth 1 -delete 2>/dev/null; " \
        "echo \"cleared; vault present: $(test -f ~/dm-home/vault.db && echo YES || echo no)\"")
  dmvm!(GOLDEN, 'down') # must be stopped: clones back onto its disk
  log 'golden ready (stopped).'
end

# ---- phase 2: clone + boot N VMs --------------------------------------------

def spin_up_vms
  VMS.each do |vm|
    dmvm(vm, 'down') # in case a previous run left this clone running
    dmvm!(vm, 'clone', GOLDEN)
  end
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
  # Each clone must have generated its OWN nsec; identical npubs mean the golden
  # wasn't wiped (every clone would invite itself and group ops would fail).
  if npubs.values.uniq.size != npubs.size
    die("clones share an identity (#{npubs.values.uniq.size} unique of #{npubs.size}) — golden vault wasn't cleared")
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

# ---- phase 3b: chaotic scenario ---------------------------------------------

# Stand up ONE group containing everyone, up front. vm1 creates it inviting
# vm2..vmN in a single commit; each newcomer then accepts. Accepts are done
# sequentially on purpose — concurrent accepts race the MLS epoch.
def build_full_group(npubs)
  others = (2..N).map { |j| "vm#{j}" }
  log "vm1 creates a group with everyone (#{others.join(', ')})…"
  res = retry_until('group-create', tries: 8, wait: 6) do
    dm('vm1', 'group-create', 'Chaos', *others.map { |vm| npubs[vm] })
  end
  die('group-create failed') unless res.is_a?(Hash) && res['group_id_hex']
  group = res['group_id_hex']
  log "  group = #{group}"

  joined = ['vm1']
  others.each do |vm|
    log "#{vm} accepts…"
    accept_invite(vm, group) ? (joined << vm) : warn_("#{vm} could not accept")
  end

  # Everyone must be in the room before the blast, or the absentees can never
  # receive (they're not in the MLS group) and the run is meaningless.
  retry_until('all members present', tries: 20, wait: 6) do
    m = dm('vm1', 'group-members', group)
    m.is_a?(Array) && m.size >= N
  end or warn_("group has fewer than #{N} members; chaos verification may be incomplete")

  [group, joined]
end

# The unique payload VM `vm` emits as its k-th message. START_TAG makes it unique
# per run so a re-run's messages can never be mistaken for this one's.
def chaos_text(vm, k) = "chaos #{START_TAG} #{vm} seq#{k}"

# The blast: every joined VM fires CHAOS_MSGS messages AT THE SAME TIME. One
# thread per VM (the sends are blocking subprocess calls, so Ruby threads give
# real overlap), each thread firing its messages back-to-back. No ordering,
# no turn-taking — maximum interleave on the relays.
def chaos_blast(joined, group)
  total = joined.size * CHAOS_MSGS
  log "💥 chaos: #{joined.size} VMs × #{CHAOS_MSGS} msgs = #{total} messages, all at once…"
  sent = Hash.new(0)
  mutex = Mutex.new
  joined.map do |vm|
    Thread.new do
      CHAOS_MSGS.times do |k|
        ok = retry_until("#{vm} send seq#{k}", tries: 10, wait: 5) do
          dm(vm, 'send', group, chaos_text(vm, k))
        end
        mutex.synchronize { sent[vm] += 1 if ok }
      end
    end
  end.each(&:join)
  joined.each { |vm| warn_("#{vm} only sent #{sent[vm]}/#{CHAOS_MSGS}") if sent[vm] < CHAOS_MSGS }
  log "  sent #{sent.values.sum}/#{total} total"
end

# The whole point: confirm EVERY VM eventually sees EVERY message (order
# irrelevant). For each VM we fetch its message list and check the set of
# kind-9 plaintexts against the expected set, retrying because relay + MLS
# delivery is eventually consistent. Returns a per-VM map of the still-missing
# payloads (empty array = fully caught up).
def verify_all_received(joined, group)
  expected = joined.flat_map { |vm| (0...CHAOS_MSGS).map { |k| chaos_text(vm, k) } }.to_set
  log "verifying all #{joined.size} VMs each received all #{expected.size} messages…"
  limit = (expected.size + 100).to_s
  missing = {}

  retry_until('every VM has every message', tries: 30, wait: 8) do
    all_done = true
    joined.each do |vm|
      msgs = dm(vm, 'messages', group, limit)
      seen = msgs.is_a?(Array) ? msgs.select { |m| m['kind'] == 9 }.map { |m| m['plaintext'] }.to_set : Set.new
      gap = expected - seen
      missing[vm] = gap.to_a
      all_done = false unless gap.empty?
    end
    if all_done
      log '  ✅ all VMs are fully caught up'
      true
    else
      behind = missing.select { |_, g| g.any? }.transform_values(&:size)
      log "  …waiting: #{behind.map { |vm, n| "#{vm} missing #{n}" }.join(', ')}"
      false
    end
  end

  [expected, missing]
end

def run_chaotic(npubs)
  group, joined = build_full_group(npubs)
  chaos_blast(joined, group)
  expected, missing = verify_all_received(joined, group)

  log '─' * 60
  log "chaos result: #{joined.size} VMs, #{expected.size} unique messages each"
  failures = missing.select { |_, g| g.any? }
  if failures.empty?
    log '  🎉 PASS — every VM received every message'
  else
    log '  ❌ FAIL — some VMs never received all messages:'
    failures.each do |vm, gap|
      log "    #{vm} missing #{gap.size}: #{gap.first(5).join(', ')}#{gap.size > 5 ? ', …' : ''}"
    end
  end
  log '─' * 60
  [group, failures.empty?]
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

# ---- phase 5: per-VM GUI screenshots ----------------------------------------

# Launch each VM's GUI and grab a screenshot of the open chat into
# scenarios/<START_TAG>/<vm>.png (gitignored). The GUI and the dm-ctl daemon
# can't share the data dir, so we stop the daemon first — but no reboot/RAM bump
# is needed: `dmvm run` uses the Slint software renderer (~150MB), so the GUI
# runs fine in the same low-mem VM.
def capture_screenshots
  dir = File.join(REPO_ROOT, 'scenarios', START_TAG)
  FileUtils.mkdir_p(dir)
  log "capturing per-VM screenshots → scenarios/#{START_TAG}/"
  VMS.each do |vm|
    dmvm(vm, 'dm', 'stop')  # release the data dir for the GUI
    dmvm(vm, 'run')         # launches GUI, auto-unlocks, auto-opens the chat
    out, ok = dmvm(vm, 'ssh', 'sleep 8; pgrep -f "darkmatter-linux$" >/dev/null && echo alive || echo dead')
    warn_("#{vm} GUI not alive (#{out.strip}) — screenshot may be blank") unless ok && out.include?('alive')
    png = File.join(dir, "#{vm}.png")
    _, sok = dmvm(vm, 'screenshot', png)
    sok && File.exist?(png) ? log("  📸 #{vm} → #{png}") : warn_("#{vm} screenshot failed")
  end
  log "screenshots in #{dir}"
end

# ---- main -------------------------------------------------------------------

die("need at least 2 VMs (got #{N})") if N < 2

if SCREENSHOTS_ONLY
  log "screenshots-only: capturing GUIs of #{N} existing VMs"
  capture_screenshots
  exit 0
end

log "scenario: #{CHAOTIC ? 'chaotic, ' : ''}#{N} VMs, golden=#{GOLDEN}, mem=#{ENV['DMVM_MEM_MB']}MiB cpus=#{ENV['DMVM_CPUS']}"
# Clones back onto the golden's disk, so any still running from a prior run must
# be stopped before we touch the golden.
log 'stopping any clones from a previous run…'
VMS.each { |vm| dmvm(vm, 'down') }
prepare_golden
npubs = spin_up_vms
if CHAOTIC
  group, passed = run_chaotic(npubs)
  report(group)
  capture_screenshots
  exit(passed ? 0 : 1)
else
  group = run_scenario(npubs)
  report(group)
  capture_screenshots
end
