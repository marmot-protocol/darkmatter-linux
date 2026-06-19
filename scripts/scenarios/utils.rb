# frozen_string_literal: true

# utils.rb — shared plumbing for the Dark Matter Linux multi-VM scenarios.
#
# Each scenario is its own runnable script in this directory (build.rb,
# chaotic.rb, soak.rb, screenshots.rb); they all `require_relative 'utils'` for:
#   • config (N, golden, relays, mem/cpu) read from ARGV[0] / env,
#   • the dmvm / dm-ctl command wrappers and retry helper,
#   • the shared lifecycle: prepare a golden image, clone+boot N VMs, accept
#     invites, build a full group, verify delivery, report, screenshot.
#
# A scenario typically looks like:
#   require_relative 'utils'
#   npubs = boot_scenario              # golden → clones → identities
#   group, passed = run_my_scenario(npubs)
#   finish(group, passed)              # report + screenshots + exit code
#
# Common env: DM_SCENARIO_GOLDEN (default "default"), DMVM_MEM_MB, DMVM_CPUS.

require 'json'
require 'open3'
require 'fileutils'
require 'set'

DMVM      = File.expand_path('../dmvm.rb', __dir__)
REPO_ROOT = File.expand_path('../..', __dir__)
# Filesystem-safe ISO-ish timestamp for this run's artifact folder; also used as
# a per-run nonce so message payloads from different runs can't be confused.
START_TAG = Time.now.utc.strftime('%Y-%m-%dT%H%M%SZ')
# Every scenario takes an optional VM count as its first positional arg.
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

def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)

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
  # Callers (retry_until, etc.) treat nil as "not ready / failed", so a response
  # we can't parse as JSON must be nil — not the raw string, which is truthy.
  nil
end

# Retry a block until it returns truthy, or give up.
def retry_until(desc, tries: 12, wait: 5)
  tries.times do
    r = yield
    return r if r
    sleep wait
  end
  warn_("gave up: #{desc}")
  nil
end

# ---- golden image + clone/boot ----------------------------------------------

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

def spin_up_vms
  VMS.each do |vm|
    dmvm(vm, 'down') # in case a previous run left this clone running
    dmvm!(vm, 'clone', GOLDEN)
  end
  log "booting #{N} clones in parallel…"
  # Thread#value joins and re-raises any exception that killed the thread, so a
  # crash in dmvm surfaces here instead of being swallowed. We also surface a
  # non-zero `up` (ok=false) as a warning rather than letting it vanish.
  VMS.map { |vm| Thread.new { [vm, dmvm(vm, 'up').last] } }.each do |t|
    vm, ok = t.value
    warn_("#{vm} failed to boot (continuing; whoami will catch a dead clone)") unless ok
  end

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

# Stop stale clones, (re)prepare the golden, clone+boot N VMs, return the npubs.
# The shared front half of every scenario.
def boot_scenario
  log "#{N} VMs, golden=#{GOLDEN}, mem=#{ENV['DMVM_MEM_MB']}MiB cpus=#{ENV['DMVM_CPUS']}"
  # Clones back onto the golden's disk, so any still running from a prior run
  # must be stopped before we touch the golden.
  log 'stopping any clones from a previous run…'
  VMS.each { |vm| dmvm(vm, 'down') }
  prepare_golden
  spin_up_vms
end

# ---- group helpers ----------------------------------------------------------

def accept_invite(vm, group)
  retry_until("#{vm} receives welcome", tries: 20, wait: 6) do
    inv = dm(vm, 'invites')
    inv.is_a?(Array) && inv.any? { |g| g['group_id_hex'].to_s.casecmp?(group) }
  end or return false
  dm(vm, 'accept', group) ? true : false
end

# Stand up ONE group containing everyone, up front. vm1 creates it inviting
# vm2..vmN in a single commit; each newcomer then accepts. Accepts are done
# sequentially on purpose — concurrent accepts race the MLS epoch. Returns
# [group_hex, joined_vms].
def build_full_group(npubs, name: 'Squad')
  others = (2..N).map { |j| "vm#{j}" }
  log "vm1 creates a group with everyone (#{others.join(', ')})…"
  res = retry_until('group-create', tries: 8, wait: 6) do
    dm('vm1', 'group-create', name, *others.map { |vm| npubs[vm] })
  end
  die('group-create failed') unless res.is_a?(Hash) && res['group_id_hex']
  group = res['group_id_hex']
  log "  group = #{group}"

  joined = ['vm1']
  others.each do |vm|
    log "#{vm} accepts…"
    accept_invite(vm, group) ? (joined << vm) : warn_("#{vm} could not accept")
  end

  # Everyone must be in the room before messaging, or the absentees can never
  # receive (they're not in the MLS group) and verification is meaningless.
  retry_until('all members present', tries: 20, wait: 6) do
    m = dm('vm1', 'group-members', group)
    m.is_a?(Array) && m.size >= N
  end or warn_("group has fewer than #{N} members; verification may be incomplete")

  [group, joined]
end

# One VM's current view of a group: its name, admin set, and member set (admins
# and members returned as sorted arrays so views compare cleanly). Returns nil
# if the VM can't see the group yet. Reads group-list (carries profile.name +
# admin_policy.admins) and group-members.
def group_view(vm, group)
  rec = begin
    list = dm(vm, 'group-list')
    list.is_a?(Array) && list.find { |g| g['group_id_hex'].to_s.casecmp?(group) }
  end
  return nil unless rec
  members = dm(vm, 'group-members', group)
  {
    name: rec.dig('profile', 'name'),
    admins: (rec.dig('admin_policy', 'admins') || []).sort,
    members: members.is_a?(Array) ? members.map { |m| m['member_id_hex'] }.compact.sort : nil,
  }
end

# Confirm EVERY VM eventually sees EVERY message in `expected` (order
# irrelevant). For each VM we fetch its message list and check the set of kind-9
# plaintexts against the expected set, retrying because relay + MLS delivery is
# eventually consistent. Returns a per-VM map of the still-missing payloads
# (empty array = fully caught up).
def verify_all_received(joined, group, expected, tries: 30)
  log "verifying all #{joined.size} VMs each received all #{expected.size} messages…"
  limit = (expected.size + 100).to_s
  missing = {}

  retry_until('every VM has every message', tries: tries, wait: 8) do
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

  missing
end

# Standard pass/fail print for the set-delivery scenarios. Returns whether it
# passed.
def report_delivery(joined, expected, missing, headline)
  log '─' * 60
  log headline
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
  failures.empty?
end

# ---- report + screenshots ---------------------------------------------------

def report(group)
  log '─' * 60
  log 'final group state (from vm1):'
  members = dm('vm1', 'group-members', group)
  msgs = dm('vm1', 'messages', group, '500')
  log "  members: #{members.is_a?(Array) ? members.size : '?'}"
  if msgs.is_a?(Array)
    chat = msgs.select { |m| m['kind'] == 9 }
    log "  chat messages seen by vm1: #{chat.size}"
    chat.last(12).each { |m| log "    #{m['sender'].to_s[0, 8]}…: #{m['plaintext']}" }
  end
  log '─' * 60
  log "done. inspect any VM with:  ruby scripts/dmvm.rb --vm vm1 dm messages #{group}"
end

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

# Shared tail of a scenario: report state, grab screenshots, exit with a code
# reflecting pass/fail.
def finish(group, passed)
  report(group)
  capture_screenshots
  exit(passed ? 0 : 1)
end
