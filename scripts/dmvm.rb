#!/usr/bin/env ruby
# frozen_string_literal: true

# dmvm — spawn and control a QEMU/KVM VM that builds & runs Dark Matter Linux.
#
# Dark Matter Linux is a Slint GUI app, not an OS image. This tool boots a stock
# Ubuntu cloud image, provisions it (Rust + Slint/Wayland build deps + a sway
# session), syncs your local working copy in, builds it, and launches the GUI
# inside the VM's display — all driven from the CLI via SSH + the QEMU QMP/serial
# sockets.
#
# Stdlib only (json, socket, fileutils, open3) — no gems.
#
# Quick start:
#   scripts/dmvm.rb up                # download image, boot, provision (first boot ~minutes)
#   scripts/dmvm.rb sync              # rsync this repo into the guest
#   scripts/dmvm.rb build             # cargo build inside the VM
#   scripts/dmvm.rb run               # launch the GUI in the VM window
#   scripts/dmvm.rb screenshot s.png  # grab the framebuffer via QMP
#   scripts/dmvm.rb ssh               # interactive shell
#   scripts/dmvm.rb down              # graceful shutdown
#
# Run `scripts/dmvm.rb help` for the full command list.

require 'json'
require 'socket'
require 'fileutils'
require 'open3'
require 'shellwords'

module DMVM
  REPO_ROOT = File.expand_path('..', __dir__)
  HOME      = ENV['DMVM_HOME'] || File.expand_path('~/.local/share/dmvm')

  # Tunables (override via env).
  IMAGE_URL = ENV['DMVM_IMAGE_URL'] ||
              'https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img'
  DISK_GB   = (ENV['DMVM_DISK_GB']  || '20').to_i
  MEM_MB    = (ENV['DMVM_MEM_MB']   || '4096').to_i
  CPUS      = (ENV['DMVM_CPUS']     || '4').to_i
  PORT_BASE = (ENV['DMVM_PORT_BASE'] || '2222').to_i
  GUEST_USER = 'ubuntu'
  GUEST_UID  = 1000
  GUEST_DIR  = "/home/#{GUEST_USER}/darkmatter-linux"
  # Shared data dir for BOTH the GUI and the dm-ctl control daemon, so they
  # drive the same vault/account. Vault password for headless control:
  GUEST_DM_HOME = "/home/#{GUEST_USER}/dm-home"
  CTL_PW     = ENV['DMVM_CTL_PW'] || 'darkmatter'
  DMCTL      = "#{GUEST_DIR}/target/debug/dm-ctl"
  # Default relays written to the guest's config so load_relays() is non-empty
  # for BOTH the GUI and dm-ctl. Mirrors the app's dev-policy DISCOVERY_RELAYS
  # (whitenoise official relays only, until release).
  DEFAULT_RELAYS = %w[wss://relay.eu.whitenoise.chat wss://relay.us.whitenoise.chat].freeze

  # Named screen sizes for `dmvm size <preset>` (WxH, portrait unless -l).
  SIZE_PRESETS = {
    'phone'    => '412x915',  'phone-l'   => '915x412',
    'tablet'   => '820x1180', 'tablet-l'  => '1180x820',
    'small'    => '800x600',  'desktop'   => '1280x800',
    'hd'       => '1920x1080', 'square'    => '900x900'
  }.freeze

  module_function

  # ---- instance selection ------------------------------------------------
  # Multiple VMs coexist under HOME/vm/<name>/. The base image and SSH key are
  # shared at the top level; everything else (overlay, seed, sockets, pid,
  # logs, assigned SSH port) is per-instance, so VMs run fully concurrently.

  def vm_name = (@vm ||= ENV['DMVM_VM'] || 'default')
  def vm_name=(n)
    @vm = n
  end
  def instances_root = File.join(HOME, 'vm')
  def instance_dir = File.join(instances_root, vm_name)

  def all_instances
    Dir.glob(File.join(instances_root, '*')).select { |p| File.directory?(p) }.map { |p| File.basename(p) }.sort
  end

  # ---- paths -------------------------------------------------------------

  # Shared across instances:
  def base_image = File.join(HOME, 'base.img')
  def ssh_key    = File.join(HOME, 'id_ed25519')
  def ssh_pub    = "#{ssh_key}.pub"
  # Per-instance:
  def overlay     = File.join(instance_dir, 'overlay.qcow2')
  def seed_iso    = File.join(instance_dir, 'seed.iso')
  def qmp_sock    = File.join(instance_dir, 'qmp.sock')
  def serial_sock = File.join(instance_dir, 'serial.sock')
  def pid_file    = File.join(instance_dir, 'qemu.pid')
  def log_file    = File.join(instance_dir, 'qemu.log')
  def port_file   = File.join(instance_dir, 'port')
  def provisioned_marker = File.join(instance_dir, '.provisioned')

  # ---- ssh port (assigned per instance) ----------------------------------

  def ssh_port
    @ports ||= {}
    @ports[vm_name] ||=
      if File.exist?(port_file)
        File.read(port_file).to_i
      elsif vm_name == 'default' && ENV['DMVM_SSH_PORT']
        ENV['DMVM_SSH_PORT'].to_i
      else
        0 # not assigned yet (VM never brought up)
      end
  end

  # Lowest free TCP port >= PORT_BASE not already claimed by another instance.
  def assign_port
    taken = all_instances.reject { |n| n == vm_name }.filter_map do |n|
      f = File.join(instances_root, n, 'port')
      File.read(f).to_i if File.exist?(f)
    end
    port = PORT_BASE
    loop do
      unless taken.include?(port)
        begin
          TCPServer.new('127.0.0.1', port).close
          break
        rescue Errno::EADDRINUSE, Errno::EACCES
          # in use by something else; skip
        end
      end
      port += 1
    end
    FileUtils.mkdir_p(instance_dir)
    File.write(port_file, port.to_s)
    @ports&.delete(vm_name)
    port
  end

  # VNC display number (for --headless), unique per instance.
  def vnc_display = [ssh_port - PORT_BASE, 0].max

  # ---- small helpers -----------------------------------------------------

  def die(msg)
    warn "dmvm: #{msg}"
    exit 1
  end

  def info(msg) = warn("\e[36m▸\e[0m #{msg}")

  def which(bin)
    ENV['PATH'].split(File::PATH_SEPARATOR).each do |dir|
      p = File.join(dir, bin)
      return p if File.executable?(p) && !File.directory?(p)
    end
    nil
  end

  def qemu_bin    = ENV['DMVM_QEMU']     || which('qemu-system-x86_64') || die('qemu-system-x86_64 not found (install qemu)')
  def qemu_img    = ENV['DMVM_QEMU_IMG'] || which('qemu-img')           || die('qemu-img not found (install qemu-utils/qemu-img)')

  def run!(*cmd, **opts)
    ok = system(*cmd, **opts)
    die("command failed: #{cmd.join(' ')}") unless ok
  end

  def running?
    return false unless File.exist?(pid_file)
    pid = File.read(pid_file).to_i
    return false if pid <= 0
    Process.kill(0, pid)
    true
  rescue Errno::ESRCH, Errno::EPERM
    false
  end

  def require_running
    die('VM is not running — `dmvm up` first') unless running?
  end

  # Pick whatever NoCloud ISO builder exists on the host.
  def iso_builder
    if (cl = which('cloud-localds'))
      ->(out, ud, md) { run!(cl, out, ud, md) }
    elsif (x = which('xorriso'))
      ->(out, ud, md) { run!(x, '-as', 'mkisofs', '-output', out, '-volid', 'CIDATA', '-joliet', '-rock', ud, md) }
    elsif (g = which('genisoimage') || which('mkisofs'))
      ->(out, ud, md) { run!(g, '-output', out, '-volid', 'CIDATA', '-joliet', '-rock', ud, md) }
    end
  end

  # ---- QMP client --------------------------------------------------------

  def qmp(command, arguments = nil)
    require_running
    UNIXSocket.open(qmp_sock) do |s|
      s.gets # greeting
      s.puts({ execute: 'qmp_capabilities' }.to_json)
      s.gets
      msg = { execute: command }
      msg[:arguments] = arguments if arguments
      s.puts(msg.to_json)
      # Skip async events; return the first reply that has return/error.
      loop do
        line = s.gets or break
        obj = JSON.parse(line) rescue next
        return obj if obj.key?('return') || obj.key?('error')
      end
    end
  end

  # ---- ssh ---------------------------------------------------------------

  SSH_OPTS = %w[
    -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null
    -o LogLevel=ERROR
    -o ConnectTimeout=5
    -o ControlPath=none
  ].freeze

  def ssh_base
    ['ssh', *SSH_OPTS, '-i', ssh_key, '-p', ssh_port.to_s, "#{GUEST_USER}@127.0.0.1"]
  end

  def ssh_exec(remote_cmd, tty: false)
    cmd = ssh_base
    cmd << '-t' if tty
    cmd << '--'
    cmd << remote_cmd
    system(*cmd)
  end

  def ssh_ready?
    out = IO.popen([*ssh_base, '-o', 'BatchMode=yes', '--', 'true'], err: %i[child out], &:read)
    $?.success?
  rescue StandardError
    false
  end

  # Run a remote command and capture [stdout+stderr, ok?].
  def ssh_capture(remote_cmd)
    out = IO.popen([*ssh_base, '--', remote_cmd], err: %i[child out], &:read)
    [out, $?.success?]
  rescue StandardError => e
    ["#{e}", false]
  end

  # ---- dm-ctl control daemon -------------------------------------------

  def guest_env = "DM_HOME=#{GUEST_DM_HOME} DM_CTL_PW=#{Shellwords.escape(CTL_PW)}"

  # Build one dm-ctl invocation (env + escaped args) to run in the guest.
  def dmctl_cmdline(args) = "#{guest_env} #{DMCTL} #{args.map { |a| Shellwords.escape(a) }.join(' ')}"

  def dmctl_built? = ssh_capture("test -x #{DMCTL}").last

  def dm_build
    require_running
    info 'building dm-ctl in the guest (cargo build --bin dm-ctl)…'
    ssh_exec("source ~/.cargo/env && cd #{GUEST_DIR} && cargo build --bin dm-ctl", tty: true) ||
      die('dm-ctl build failed')
  end

  def daemon_running? = ssh_capture(dmctl_cmdline(['ping'])).last

  def start_daemon
    require_running
    dm_build unless dmctl_built?
    if daemon_running?
      info 'control daemon already running'
      return
    end
    info 'starting dm-ctl control daemon (telemetry + audit ON by default on first run)…'
    # First boot blocks on relay directory sync; give it room.
    boot = <<~SH.gsub("\n", ' ')
      mkdir -p #{GUEST_DM_HOME};
      cp -n #{GUEST_DIR}/observability.toml #{GUEST_DM_HOME}/ 2>/dev/null || true;
      source ~/.cargo/env;
      #{guest_env} setsid #{DMCTL} serve >#{GUEST_DM_HOME}/dm-ctl.log 2>&1 < /dev/null &
    SH
    ssh_exec(boot)
    info 'waiting for daemon to accept commands (first boot syncs relays — up to 2 min)…'
    deadline = monotonic + 150
    until daemon_running?
      die('daemon exited (see `dmvm dm log`)') if monotonic > deadline
      sleep 3
    end
    info 'control daemon ready'
  end

  def ensure_daemon
    start_daemon unless daemon_running?
  end

  def wait_for_ssh(timeout: 180)
    info "waiting for SSH on port #{ssh_port} …"
    deadline = monotonic + timeout
    until ssh_ready?
      die('VM exited while waiting for SSH (see qemu.log)') unless running?
      die('timed out waiting for SSH') if monotonic > deadline
      sleep 3
    end
    info 'SSH is up'
  end

  def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)

  # ---- cloud-init payload ------------------------------------------------

  def write_seed
    pub = File.read(ssh_pub).strip

    user_data = <<~YAML
      #cloud-config
      hostname: dmvm
      users:
        - name: #{GUEST_USER}
          sudo: ALL=(ALL) NOPASSWD:ALL
          shell: /bin/bash
          lock_passwd: false
          ssh_authorized_keys:
            - #{pub}
      # password login for the local console, in case you poke at the VT directly
      chpasswd:
        expire: false
        users:
          - name: #{GUEST_USER}
            password: darkmatter
            type: text
      ssh_pwauth: true
      package_update: true
      packages:
        - build-essential
        - pkg-config
        - curl
        - git
        - rsync
        - ca-certificates
        - libssl-dev
        - libfontconfig1-dev
        - libfreetype-dev
        - libxkbcommon-dev
        - libxkbcommon-x11-dev
        - libwayland-dev
        - libxcb1-dev
        - libxcb-render0-dev
        - libxcb-shape0-dev
        - libxcb-xfixes0-dev
        - libgl1-mesa-dev
        - libegl1-mesa-dev
        - libgles2-mesa-dev
        - mesa-utils
        - libinput-dev
        - libmpv-dev
        - sway
        - swaybg
        - xwayland
        - foot
        - wtype
        - grim
        - wl-clipboard
        - xclip
        - seatd
        - fonts-noto-color-emoji
        - fonts-dejavu
      write_files:
        - path: /etc/systemd/system/getty@tty1.service.d/autologin.conf
          content: |
            [Service]
            ExecStart=
            ExecStart=-/sbin/agetty --autologin #{GUEST_USER} --noclear %I $TERM
        - path: /home/#{GUEST_USER}/.bash_profile
          owner: #{GUEST_USER}:#{GUEST_USER}
          # Deferred to the final stage: the early write_files pass runs BEFORE
          # the users module creates `ubuntu`, so an ubuntu-owned file there
          # aborts the whole module.
          defer: true
          content: |
            [ -f ~/.bashrc ] && . ~/.bashrc
            [ -f ~/.cargo/env ] && . ~/.cargo/env
            if [ "$(tty)" = "/dev/tty1" ] && [ -z "$WAYLAND_DISPLAY" ]; then
              export XDG_RUNTIME_DIR=/run/user/$(id -u)
              exec sway
            fi
        - path: /etc/profile.d/dmvm.sh
          content: |
            export DM_HOME=#{GUEST_DM_HOME}
        - path: /home/#{GUEST_USER}/.config/darkmatter-linux/relays.json
          owner: #{GUEST_USER}:#{GUEST_USER}
          defer: true
          content: '#{JSON.generate(DEFAULT_RELAYS)}'
        - path: /usr/local/bin/dm-run
          permissions: '0755'
          content: |
            #!/bin/bash
            # Launch the built GUI inside the running sway session.
            set -e
            export DM_HOME=#{GUEST_DM_HOME}
            export XDG_RUNTIME_DIR=/run/user/#{GUEST_UID}
            sock=$(ls "$XDG_RUNTIME_DIR"/wayland-* 2>/dev/null | grep -v '\\.lock' | head -1)
            if [ -z "$sock" ]; then echo "no wayland display (is sway running on tty1?)" >&2; exit 1; fi
            export WAYLAND_DISPLAY=$(basename "$sock")
            export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* 2>/dev/null | head -1)
            cd #{GUEST_DIR}
            [ -f ~/.cargo/env ] && . ~/.cargo/env
            exec ./target/debug/darkmatter-linux "$@"
      runcmd:
        - [ systemctl, enable, --now, seatd ]
        - [ systemctl, daemon-reload ]
        - su - #{GUEST_USER} -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
        - [ systemctl, restart, getty@tty1 ]
        - [ touch, /var/lib/dmvm-provisioned ]
    YAML

    meta_data = "instance-id: dmvm-#{Time.now.to_i}\nlocal-hostname: dmvm\n"

    Dir.mktmpdir do |d|
      ud = File.join(d, 'user-data')
      md = File.join(d, 'meta-data')
      File.write(ud, user_data)
      File.write(md, meta_data)
      build = iso_builder or die(<<~MSG)
        no NoCloud ISO builder found. Install one:
          Arch:   sudo pacman -S cloud-image-utils   (or: libisoburn for xorriso)
          Debian: sudo apt install cloud-image-utils  (or: genisoimage)
      MSG
      build.call(seed_iso, ud, md)
    end
  end

  # ---- lifecycle ---------------------------------------------------------

  def ensure_key
    return if File.exist?(ssh_key)
    info 'generating SSH keypair'
    run!('ssh-keygen', '-t', 'ed25519', '-N', '', '-q', '-f', ssh_key, '-C', 'dmvm')
  end

  def ensure_base_image
    return if File.exist?(base_image)
    curl = which('curl') || die('curl not found (needed to download the cloud image)')
    info "downloading cloud image → #{base_image}"
    run!(curl, '-L', '--fail', '--progress-bar', '-o', "#{base_image}.part", IMAGE_URL)
    FileUtils.mv("#{base_image}.part", base_image)
  end

  def ensure_overlay
    return if File.exist?(overlay)
    info 'creating disk overlay'
    run!(qemu_img, 'create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b', base_image, overlay)
    run!(qemu_img, 'resize', '-q', overlay, "#{DISK_GB}G")
  end

  def cmd_up(args)
    gui = args.delete('--gui') || args.delete('--window')
    args.delete('--headless') # the default now; accepted for explicitness
    no_gl = args.delete('--no-gl')
    headless = !gui
    if running?
      info "VM '#{vm_name}' already running (pid #{File.read(pid_file).to_i}); ssh on port #{ssh_port}"
      return
    end
    FileUtils.mkdir_p(instance_dir)
    assign_port unless File.exist?(port_file)
    ensure_key
    ensure_base_image
    ensure_overlay
    write_seed
    [qmp_sock, serial_sock].each { |s| File.unlink(s) if File.exist?(s) }

    accel = File.exist?('/dev/kvm') ? 'kvm' : 'tcg'
    info "no /dev/kvm — falling back to slow TCG emulation" if accel == 'tcg'

    # GPU + display. virtio-vga is the PRIMARY VGA so the boot console and sway
    # share the one visible scanout. The GL variant (virgl) gives sway a real
    # GLES renderer — clean, accelerated, no software-render tearing. `--no-gl`
    # falls back to software 2D if the host lacks virgl.
    gl = !headless && !no_gl
    if headless
      gpu = ['-vga', 'virtio']
      display = ['-display', 'none', '-vnc', ":#{vnc_display}"]
    elsif gl
      gpu = ['-vga', 'none', '-device', 'virtio-vga-gl']
      display = ['-display', 'gtk,gl=on']
    else
      gpu = ['-vga', 'virtio']
      display = ['-display', 'gtk,gl=off']
    end

    qemu = [
      qemu_bin,
      '-name', "dmvm-#{vm_name}",
      '-machine', "q35,accel=#{accel}",
      '-cpu', accel == 'kvm' ? 'host' : 'max',
      '-smp', CPUS.to_s,
      '-m', MEM_MB.to_s,
      '-drive', "if=virtio,file=#{overlay},format=qcow2",
      '-drive', "file=#{seed_iso},media=cdrom",
      '-device', 'virtio-net-pci,netdev=net0',
      '-netdev', "user,id=net0,hostfwd=tcp::#{ssh_port}-:22",
      *gpu,
      '-device', 'virtio-keyboard-pci',
      '-device', 'virtio-tablet-pci',
      *display,
      '-qmp', "unix:#{qmp_sock},server,nowait",
      '-serial', "unix:#{serial_sock},server,nowait",
      '-pidfile', pid_file
    ]

    vnc_note = headless ? " [headless/vnc :#{5900 + vnc_display}]" : (gl ? ' [virgl]' : ' [soft-gl]')
    info "booting VM '#{vm_name}' (#{accel}, #{CPUS} cpu, #{MEM_MB} MiB, ssh :#{ssh_port})#{vnc_note}"
    log = File.open(log_file, 'w')
    pid = Process.spawn(*qemu, in: :close, out: log, err: log, pgroup: true)
    Process.detach(pid)
    sleep 1
    die("qemu failed to start (see #{log_file})") unless running?

    first_boot = !File.exist?(provisioned_marker)
    wait_for_ssh(timeout: first_boot ? 600 : 180)

    if first_boot
      info 'first boot: waiting for cloud-init provisioning to finish (Rust + deps, several minutes) …'
      ssh_exec('cloud-init status --wait || true')
      detail, = ssh_capture('cloud-init status --long')
      if detail.include?('status: error')
        warn detail
        die("cloud-init provisioning FAILED (not marking provisioned). Inspect with " \
            "`dmvm ssh -- cloud-init status --long`; fix and re-run `dmvm destroy && dmvm up`.")
      end
      FileUtils.touch(provisioned_marker)
      info 'provisioned. Next: `dmvm sync && dmvm build && dmvm run`'
    end
    info "ready. ssh: dmvm ssh   |   gui: dmvm run"
  end

  def cmd_sync(_args)
    require_running
    rsync = which('rsync') || die('rsync not found on host')
    info "syncing #{REPO_ROOT} → guest:#{GUEST_DIR}"
    ssh = "ssh #{SSH_OPTS.join(' ')} -i #{ssh_key} -p #{ssh_port}"
    run!(rsync, '-az', '--delete',
         '--exclude', 'target/', '--exclude', '.git/', '--exclude', '.dmvm/',
         '-e', ssh,
         "#{REPO_ROOT}/", "#{GUEST_USER}@127.0.0.1:#{GUEST_DIR}/")
    info 'sync complete'
  end

  def cmd_build(args)
    return build_on_host(release: args.include?('--release')) if args.delete('--host')
    require_running
    profile = args.include?('--release') ? '--release' : ''
    ssh_exec("source ~/.cargo/env && cd #{GUEST_DIR} && cargo build #{profile}", tty: true) ||
      die('build failed')
  end

  # ---- host-side build + binary push -----------------------------------
  # Building on the host is far faster than in the VM, but the host (Arch,
  # glibc 2.43) is newer than the guest (Ubuntu 2.39), so a native binary won't
  # load. cargo-zigbuild links against the guest's glibc version; the binary
  # then runs in the VM (all its .so deps are installed there).

  GUEST_RUST_TARGET = 'x86_64-unknown-linux-gnu'
  APP_BIN_GUEST = "#{GUEST_DIR}/target/debug/darkmatter-linux"

  # Guest glibc version (drives the zigbuild target). Falls back to noble's
  # 2.39 when the VM isn't up, so host builds don't depend on a running VM.
  def guest_glibc
    @guest_glibc ||=
      if ENV['DMVM_GUEST_GLIBC']
        ENV['DMVM_GUEST_GLIBC']
      elsif running?
        out, ok = ssh_capture('ldd --version')
        (ok && out[/(\d+\.\d+)/, 1]) || '2.39'
      else
        '2.39'
      end
  end

  # Binaries cross-built on the host and pushed in: name => guest destination.
  def push_bins = { 'darkmatter-linux' => APP_BIN_GUEST, 'dm-ctl' => DMCTL }

  def host_bin(profile, name)
    [File.join(REPO_ROOT, 'target', GUEST_RUST_TARGET, profile, name),
     File.join(REPO_ROOT, 'target', profile, name)].find { |p| File.exist?(p) }
  end

  def bin_glibc_max(path)
    od = which('objdump') or return nil
    out = IO.popen([od, '-T', path], err: File::NULL, &:read)
    out.scan(/GLIBC_(\d+\.\d+(?:\.\d+)?)/).flatten.max_by { |v| v.split('.').map(&:to_i) }
  end

  def ver_gt(a, b) = (a.split('.').map(&:to_i) <=> b.split('.').map(&:to_i)).positive?

  def ensure_zigbuild
    return if which('cargo-zigbuild')
    which('zig') || die('zig not found on host (needed by cargo-zigbuild)')
    info 'installing cargo-zigbuild (one-time)…'
    run!('cargo', 'install', 'cargo-zigbuild')
  end

  def build_on_host(release:)
    ensure_zigbuild
    target = "#{GUEST_RUST_TARGET}.#{guest_glibc}"
    cmd = ['cargo', 'zigbuild', '--target', target, '--bin', 'darkmatter-linux', '--bin', 'dm-ctl']
    cmd << '--release' if release
    info "building on host for #{target} (cargo-zigbuild): darkmatter-linux + dm-ctl …"
    run!(*cmd, chdir: REPO_ROOT)
    info "host build done. Push it in with `dmvm push#{release ? ' --release' : ''}`"
  end

  def push_one(bin, dest, keepdbg)
    local = bin
    if !keepdbg && (strip = which('strip'))
      FileUtils.mkdir_p(instance_dir)
      local = File.join(instance_dir, "#{File.basename(bin)}.push")
      FileUtils.cp(bin, local)
      run!(strip, '-s', local)
    end
    info "  #{File.basename(bin)} → #{dest} (#{(File.size(local) / 1_048_576.0).round} MiB#{keepdbg ? '' : ', stripped'})"
    ssh_exec("mkdir -p #{File.dirname(dest)}")
    if (rsync = which('rsync'))
      ssh = "ssh #{SSH_OPTS.join(' ')} -i #{ssh_key} -p #{ssh_port}"
      run!(rsync, '-z', '--inplace', '-e', ssh, local, "#{GUEST_USER}@127.0.0.1:#{dest}")
    else
      run!('scp', *SSH_OPTS, '-i', ssh_key, '-P', ssh_port.to_s, local, "#{GUEST_USER}@127.0.0.1:#{dest}")
    end
    ssh_exec("chmod +x #{dest}")
  end

  def cmd_push(args)
    require_running
    force = args.delete('--force')
    keepdbg = args.delete('--debug-symbols')
    profile = args.delete('--release') ? 'release' : 'debug'

    pushed = 0
    push_bins.each do |name, dest|
      bin = host_bin(profile, name) or next
      maxv = bin_glibc_max(bin)
      if maxv && !force && ver_gt(maxv, guest_glibc)
        die("#{name} needs GLIBC_#{maxv} but the guest has #{guest_glibc} — it won't load. " \
            "Build against the guest glibc: `dmvm build --host`. (Override with --force.)")
      end
      push_one(bin, dest, keepdbg)
      pushed += 1
    end
    die("no host binaries for #{profile}. Build with `dmvm build --host#{profile == 'release' ? ' --release' : ''}`.") if pushed.zero?
    info 'pushed. launch with `dmvm run`'
  end

  def guest_vault? = ssh_capture("test -f #{GUEST_DM_HOME}/vault.db").last

  # First run: create a fresh nsec + vault (telemetry/audit on) via dm-ctl, so
  # the user lands on the app's unlock screen instead of generating a key by
  # hand. No-op once a vault exists.
  # Seed the guest's relays.json (in XDG config, read by load_relays) so the
  # backend can publish account relay lists on first login.
  def ensure_relays_config
    json = JSON.generate(DEFAULT_RELAYS)
    ssh_exec("mkdir -p ~/.config/darkmatter-linux && printf '%s' #{Shellwords.escape(json)} > ~/.config/darkmatter-linux/relays.json")
  end

  def ensure_identity
    return if guest_vault?
    unless dmctl_built?
      die('dm-ctl not in the guest yet — push it first with `dmvm run --host` (or `dmvm push`).')
    end
    info 'first run: generating a new nsec + vault (telemetry + audit on)…'
    out, ok = ssh_capture(dmctl_cmdline(['init']))
    die("identity setup failed:\n#{out}") unless ok
    npub = out.lines.reverse_each.lazy.filter_map { |l| (JSON.parse(l)['npub'] rescue nil) }.first
    info "identity ready#{npub ? " — #{npub}" : ''} (vault password: #{CTL_PW})"
  end

  # Type the known vault password into the app's unlock screen. The password
  # LineInput isn't auto-focused, so click it first (centered card, ~59% down),
  # type, and Enter. Coordinates scale with the output resolution.
  def auto_unlock
    return unless guest_vault?
    # The app needs a moment to render the unlock screen; retry get_outputs
    # since it can transiently report nothing right after launch.
    w = h = 0
    10.times do
      m = (sway_outputs.first || {})['current_mode'] || {}
      w = m['width'].to_i
      h = m['height'].to_i
      break if w.positive? && h.positive?
      sleep 1
    end
    return if w.zero?
    fx = (w * 0.5).round
    fy = (h * 0.587).round
    info 'unlocking the app (vault password)…'
    ssh_exec("#{sway_env}; " \
             "swaymsg seat seat0 cursor set #{fx} #{fy}; " \
             "swaymsg seat seat0 cursor press button1; swaymsg seat seat0 cursor release button1; " \
             "sleep 0.3; wtype #{Shellwords.escape(CTL_PW)}; sleep 0.2; wtype -k Return")
  end

  def cmd_run(args)
    require_running
    release = args.include?('--release')
    no_unlock = args.delete('--no-unlock')
    if args.delete('--host')
      build_on_host(release: release)
      cmd_push(release ? ['--release'] : [])
    elsif args.delete('--push')
      cmd_push(release ? ['--release'] : [])
    elsif args.include?('--build')
      cmd_sync([])
      cmd_build([])
    end
    bin = APP_BIN_GUEST
    unless ssh_capture("test -x #{bin}").last
      die("app not built yet (#{bin} missing). Run `dmvm run --build`, `--host`, or `--push` first.")
    end
    unless ssh_capture("#{sway_env}; [ -n \"$WAYLAND_DISPLAY\" ]").last
      die('no Wayland session — sway is not up on tty1 yet. Check `dmvm screenshot` / `dmvm ssh -- pgrep sway`.')
    end
    ensure_relays_config
    ensure_identity
    info 'launching GUI in the VM window (logs → guest ~/dm.log)'
    # Anchor the pattern at end-of-cmdline so this very shell command (whose
    # text contains 'darkmatter-linux') isn't matched by pgrep.
    started = ssh_exec("nohup dm-run >~/dm.log 2>&1 & sleep 2; " \
             "pgrep -f 'darkmatter-linux$' >/dev/null && echo 'app running' || " \
             "{ echo 'app exited immediately — last log:'; tail -n 15 ~/dm.log; }")
    auto_unlock if started && !no_unlock
  end

  # ---- display / screen size (sway over the GUI session) ---------------

  # Shell prelude exporting the running GUI session's Wayland + sway sockets,
  # so swaymsg (needs SWAYSOCK) and wtype (needs WAYLAND_DISPLAY) both work over
  # a non-login ssh command.
  def sway_env
    # SWAYSOCK must point at the RUNNING sway's socket: the name embeds sway's
    # pid, and dead sways leave stale sockets behind, so a glob+head picks the
    # wrong one. Build it from the live pid; pick the newest wayland socket.
    "export XDG_RUNTIME_DIR=/run/user/#{GUEST_UID}; " \
      "export SWAYSOCK=/run/user/#{GUEST_UID}/sway-ipc.#{GUEST_UID}.$(pgrep -x sway | head -1).sock; " \
      "export WAYLAND_DISPLAY=$(basename $(ls -t /run/user/#{GUEST_UID}/wayland-* 2>/dev/null | grep -v '\\.lock' | head -1))"
  end

  def sway_outputs
    out, ok = ssh_capture("#{sway_env}; swaymsg -t get_outputs -r")
    return [] unless ok
    # Strip any leading noise (e.g. ssh warnings) before the JSON array.
    json = out[/\[.*\]/m] or return []
    JSON.parse(json)
  rescue StandardError
    []
  end

  def swaymsg!(arg)
    ssh_exec("#{sway_env}; swaymsg #{arg}") || die("swaymsg #{arg} failed")
  end

  # ---- input injection (clicks, typing, keys) --------------------------
  # Clicks go through sway's own `seat … cursor` IPC — no uinput/ydotool, so it
  # works headless and for every concurrent VM independently. Typing uses wtype
  # (Wayland virtual-keyboard protocol).

  # sway's `cursor press` uses X button numbers (1=left, 2=middle, 3=right),
  # not libinput event codes.
  MOUSE_BUTTONS = { 'left' => 1, 'middle' => 2, 'right' => 3 }.freeze

  def cmd_click(args)
    require_running
    button = 'left'
    if (i = args.index('--button'))
      button = args[i + 1]; args = args[0...i] + args[(i + 2)..]
    end
    code = MOUSE_BUTTONS[button] or die("unknown button '#{button}' (left/right/middle)")
    x, y = args
    die('usage: dmvm click <x> <y> [--button left|right|middle]') unless x =~ /\A\d+\z/ && y =~ /\A\d+\z/
    ssh_exec("#{sway_env}; swaymsg seat seat0 cursor set #{x} #{y}; " \
             "swaymsg seat seat0 cursor press button#{code}; " \
             "swaymsg seat seat0 cursor release button#{code}") || die('click failed')
    info "click #{button} @ #{x},#{y}"
  end

  def cmd_move(args)
    require_running
    x, y = args
    die('usage: dmvm move <x> <y>') unless x =~ /\A\d+\z/ && y =~ /\A\d+\z/
    swaymsg!("seat seat0 cursor set #{x} #{y}")
  end

  def cmd_type(args)
    require_running
    text = args.join(' ')
    die('usage: dmvm type <text…>') if text.empty?
    ssh_exec("#{sway_env}; wtype #{Shellwords.escape(text)}") || die('type failed (is wtype installed in the guest?)')
  end

  # dmvm key Return | Escape | Tab | ctrl+a | ctrl+shift+k …
  def cmd_key(args)
    require_running
    die('usage: dmvm key <keysym|mod+key> …') if args.empty?
    args.each do |combo|
      parts = combo.split('+')
      key = parts.pop
      mods = parts
      cmd = +'wtype '
      mods.each { |m| cmd << "-M #{Shellwords.escape(m)} " }
      cmd << "-k #{Shellwords.escape(key)} "
      mods.reverse_each { |m| cmd << "-m #{Shellwords.escape(m)} " }
      ssh_exec("#{sway_env}; #{cmd}") || die("key '#{combo}' failed")
    end
  end

  def cmd_size(args)
    require_running
    output = nil
    scale = nil
    size = nil
    i = 0
    while i < args.length
      a = args[i]
      case a
      when '--output' then output = args[i + 1]; i += 2
      when '--scale'  then scale = args[i + 1];  i += 2
      when /\A\d+x\d+\z/ then size = a; i += 1
      else
        if SIZE_PRESETS.key?(a)
          size = SIZE_PRESETS[a]; i += 1
        else
          die "unknown size argument '#{a}' (try WxH, a preset #{SIZE_PRESETS.keys.join('/')}, --scale, --output)"
        end
      end
    end

    outs = sway_outputs
    die 'no sway outputs — start the GUI session first with `dmvm run`' if outs.empty?
    name = output || outs.first['name']

    if size.nil? && scale.nil?
      outs.each do |o|
        m = o['current_mode'] || {}
        puts "#{o['name']}  #{m['width']}x#{m['height']}@#{(m['refresh'].to_i / 1000.0).round}Hz  scale #{o['scale']}  #{o['active'] ? '' : '(inactive)'}"
      end
      return
    end

    if size
      w, h = size.split('x')
      swaymsg!("output #{name} mode --custom #{w}x#{h}@60Hz")
    end
    swaymsg!("output #{name} scale #{scale}") if scale
    info "#{name}: #{size || '(size unchanged)'}#{scale ? ", scale #{scale}" : ''}"
  end

  def cmd_dm(args)
    sub = args.first
    return dm_help if sub.nil? || sub == 'help' || sub == '-h' || sub == '--help'
    require_running
    case sub
    when 'build' then return dm_build
    when 'serve', 'start' then return start_daemon
    when 'stop'  then return (ssh_exec(dmctl_cmdline(['shutdown'])); info('daemon stopped'))
    when 'log'   then return ssh_exec("tail -n 200 #{GUEST_DM_HOME}/dm-ctl.log")
    when 'status' then return ssh_exec(dmctl_cmdline(['ping']))
    when 'watch' then return dm_watch(args[1..] || [])
    end
    ensure_daemon
    ssh_exec(dmctl_cmdline(args)) || die("dm #{sub} failed")
  end

  # Live-tail a conversation. Streams newline-delimited JSON from `dm-ctl watch`
  # over SSH and formats each line, until Ctrl-C.
  def dm_watch(args)
    raw = args.delete('--json')
    group = args.first or die('usage: dmvm dm watch <group_hex> [--json]')
    ensure_daemon
    info "watching #{group[0, 12]}…  (Ctrl-C to stop)" unless raw
    remote = dmctl_cmdline(['watch', group])
    io = IO.popen([*ssh_base, '--', remote])
    trap('INT') { Process.kill('TERM', io.pid) rescue nil }
    io.each_line do |line|
      if raw
        print line
        next
      end
      ev = JSON.parse(line) rescue next
      case ev['type']
      when 'ready'   then info 'subscription live'
      when 'message' then puts format_watch_message(ev)
      end
    end
  ensure
    trap('INT', 'DEFAULT')
    io&.close rescue nil
  end

  def format_watch_message(ev)
    ts = (Time.at(ev['recorded_at'].to_i).strftime('%H:%M:%S') rescue '--:--:--')
    who = ev['sender_name']
    who = "#{ev['sender'].to_s[0, 8]}…" if who.nil? || who.empty?
    body =
      case ev['kind']
      when 9    then ev['text'].to_s
      when 7    then "reacted #{ev['text']}"
      when 5    then 'deleted a message'
      when 1009 then "edited: #{ev['text']}"
      else           "[kind #{ev['kind']}] #{ev['text']}"
      end
    "\e[2m#{ts}\e[0m  \e[1m#{who}\e[0m  #{body}"
  end

  def dm_help
    puts <<~TXT
      dmvm dm — drive Dark Matter Linux headlessly via the dm-ctl daemon

        dmvm dm serve                       boot the control daemon (auto-started on demand)
        dmvm dm stop | log | status         daemon lifecycle / logs
        dmvm dm build                       (re)build the dm-ctl binary in the guest

        dmvm dm whoami                       active account + npub
        dmvm dm accounts                     list local accounts
        dmvm dm account-add <nsec>           add another identity
        dmvm dm flags                        telemetry / audit state
        dmvm dm telemetry on|off
        dmvm dm audit on|off
        dmvm dm profile-get
        dmvm dm profile-set name=Alice about="hi" picture=https://…
        dmvm dm follow <npub|hex> ; dmvm dm follows
        dmvm dm group-create "<name>" [npub …]   -> group_id_hex
        dmvm dm group-list ; dmvm dm group-members <group_hex>
        dmvm dm invite <group_hex> <npub …>
        dmvm dm rename <group_hex> "<name>"
        dmvm dm send <group_hex> <text…>
        dmvm dm messages <group_hex> [limit]
        dmvm dm watch <group_hex> [--json]       live-tail a conversation (Ctrl-C to stop)
        dmvm dm react <group_hex> <msg_hex> <emoji>
        dmvm dm relays
        dmvm dm settings-get
        dmvm dm settings-set theme=light locale=ja accent=ocean

      The daemon and the GUI (`dmvm run`) share #{GUEST_DM_HOME}, so messages you
      send headlessly show up in the GUI account (vault password: #{CTL_PW}).
    TXT
  end

  def cmd_ssh(args)
    require_running
    if args.empty?
      exec(*ssh_base, '-t')
    else
      ssh_exec(args.join(' '), tty: true)
    end
  end

  def scp_from(remote, local)
    system('scp', *SSH_OPTS, '-i', ssh_key, '-P', ssh_port.to_s,
           "#{GUEST_USER}@127.0.0.1:#{remote}", local)
  end

  # Wayland-native capture via grim (works with gl=on/virgl, pixel-perfect).
  def grim_capture(out)
    return false unless ssh_capture("#{sway_env}; command -v grim >/dev/null && [ -n \"$WAYLAND_DISPLAY\" ]").last
    return false unless ssh_capture("#{sway_env}; grim /tmp/dmvm-shot.png").last
    scp_from('/tmp/dmvm-shot.png', out)
  end

  def cmd_screenshot(args)
    require_running
    out = args[0] || 'screenshot.png'
    if grim_capture(out)
      info "wrote #{out} (grim)"
      return
    end
    # Fallback: host-side QMP screendump (boot console / headless / no grim).
    # Note: fails under gl=on (texture scanout has no readable surface).
    ppm = "#{out}.ppm"
    res = qmp('screendump', { filename: File.absolute_path(ppm) })
    if res&.key?('error')
      die("grim unavailable and QMP screendump failed (#{res['error']['desc']}). " \
          'Under virgl, capture needs grim in the guest — `dmvm ssh -- sudo apt install -y grim`.')
    end
    if (conv = which('magick') || which('convert'))
      run!(conv, ppm, out)
      File.unlink(ppm)
      info "wrote #{out} (qmp)"
    else
      FileUtils.mv(ppm, out.sub(/\.png\z/i, '.ppm'))
      info "wrote #{out.sub(/\.png\z/i, '.ppm')} (qmp; install ImageMagick for PNG)"
    end
  end

  def cmd_console(_args)
    require_running
    info 'serial console — Ctrl-C to detach'
    UNIXSocket.open(serial_sock) do |s|
      threads = []
      threads << Thread.new { IO.copy_stream(s, $stdout) }
      threads << Thread.new { IO.copy_stream($stdin, s) }
      threads.each(&:join)
    end
  rescue Interrupt
    puts
  end

  def cmd_qmp(args)
    die('usage: dmvm qmp <command> [json-args]') if args.empty?
    parsed = args[1] ? JSON.parse(args[1]) : nil
    puts JSON.pretty_generate(qmp(args[0], parsed))
  end

  def cmd_status(_args)
    puts "vm:      #{vm_name}"
    if running?
      pid = File.read(pid_file).to_i
      puts "state:   running (pid #{pid})"
      puts "ssh:     ssh -p #{ssh_port} #{GUEST_USER}@127.0.0.1  (#{ssh_ready? ? 'reachable' : 'not yet'})"
      puts "qmp:     #{qmp_sock}"
      puts "serial:  #{serial_sock}"
    else
      puts 'state:   stopped'
    end
    puts "dir:     #{instance_dir}"
  end

  def cmd_ls(_args)
    names = all_instances
    names << 'default' if names.empty?
    puts format('%-16s %-9s %-7s %s', 'VM', 'STATE', 'PORT', 'DIR')
    names.uniq.sort.each do |n|
      self.vm_name = n
      @ports&.delete(n)
      state = running? ? 'running' : 'stopped'
      port = File.exist?(port_file) ? ssh_port : '-'
      puts format('%-16s %-9s %-7s %s', n, state, port, instance_dir)
    end
  end

  def cmd_down(_args)
    unless running?
      info 'VM is not running'
      return
    end
    info 'sending ACPI powerdown …'
    qmp('system_powerdown')
    30.times { running? ? sleep(1) : break }
    if running?
      info 'still up — forcing quit'
      qmp('quit') rescue nil
      pid = File.read(pid_file).to_i
      Process.kill('TERM', pid) rescue nil
    end
    info 'stopped'
  end

  def cmd_destroy(_args)
    cmd_down([]) if running?
    FileUtils.rm_rf(instance_dir)
    info "destroyed instance '#{vm_name}' (#{instance_dir}); base image + ssh key kept in #{HOME}"
  end

  def cmd_help(_args = nil)
    puts <<~TXT
      dmvm — spawn & control a Dark Matter Linux build/run VM (QEMU/KVM)

      Usage: scripts/dmvm.rb [--vm <name>] <command> [options]

        up [--gui]          download image (first run), boot, provision
                            (default: headless w/ VNC; --gui opens a QEMU window)
        sync                rsync this working copy into the guest
        build [--release]   cargo build inside the VM
        build --host        build on the HOST for the guest's glibc (cargo-zigbuild)
        push [--release]    copy host-built binaries (app + dm-ctl) into the VM
        run [--build]       launch the app (--build=in-VM build; --host=build-on-host
                            +push; --push=copy host binaries, then run). On first
                            run, auto-creates an nsec+vault and unlocks the app
                            (--no-unlock to skip the auto-unlock)
        dm <subcommand>     drive the app headlessly (accounts/groups/messages/settings);
                            run `dmvm dm help` for the full list
        size [WxH|preset]   resize the VM screen live (no args = show current);
                            presets: phone, tablet, small, desktop, hd, square (+ -l);
                            options: --scale <n> (HiDPI), --output <name>
        click <x> <y>       click at pixel x,y (--button left|right|middle)
        move <x> <y>        move the pointer without clicking
        type <text…>        type text into the focused widget
        key <keysym…>       press keys, e.g. `key Return` or `key ctrl+a`
        ssh [cmd...]        shell into the guest, or run a command
        screenshot [file]   capture the VM framebuffer (PNG via ImageMagick, else PPM)
        console             attach to the serial console (Ctrl-C detaches)
        qmp <cmd> [json]    send a raw QMP command, e.g. qmp query-status
        status              show this VM's state and connection info
        ls                  list all VM instances and their state/ports
        down                graceful ACPI shutdown (force-quit fallback)
        destroy             remove this instance's overlay/state (keeps base image)
        help                this message

      Multiple VMs: pass `--vm <name>` (or set DMVM_VM) to target a named
      instance; each gets its own disk, SSH port and display, fully concurrent.
      Without it, the instance is 'default'.

      Env overrides: DMVM_HOME, DMVM_VM, DMVM_IMAGE_URL, DMVM_MEM_MB, DMVM_CPUS,
                     DMVM_DISK_GB, DMVM_PORT_BASE, DMVM_SSH_PORT, DMVM_QEMU, DMVM_QEMU_IMG

      State dir: #{HOME}
    TXT
  end

  DISPATCH = {
    'up' => :cmd_up, 'sync' => :cmd_sync, 'build' => :cmd_build, 'run' => :cmd_run,
    'dm' => :cmd_dm, 'size' => :cmd_size, 'resize' => :cmd_size, 'push' => :cmd_push,
    'click' => :cmd_click, 'move' => :cmd_move, 'type' => :cmd_type, 'key' => :cmd_key,
    'ssh' => :cmd_ssh, 'screenshot' => :cmd_screenshot, 'shot' => :cmd_screenshot,
    'console' => :cmd_console, 'qmp' => :cmd_qmp, 'status' => :cmd_status, 'ls' => :cmd_ls,
    'down' => :cmd_down, 'stop' => :cmd_down, 'destroy' => :cmd_destroy,
    'help' => :cmd_help, '-h' => :cmd_help, '--help' => :cmd_help
  }.freeze

  def main(argv)
    # Global `--vm <name>` (or DMVM_VM) selects the instance; may appear anywhere.
    if (i = argv.index('--vm'))
      name = argv[i + 1] or die('--vm needs a name')
      self.vm_name = name
      argv = argv[0...i] + argv[(i + 2)..]
    end
    cmd = argv.shift || 'help'
    meth = DISPATCH[cmd] or die("unknown command '#{cmd}' (try `dmvm help`)")
    require 'tmpdir' if meth == :cmd_up
    send(meth, argv)
  end
end

DMVM.main(ARGV) if $PROGRAM_NAME == __FILE__
