# The boot and install screen (#618)

Owner, 2026-11-15: "I really want a install screen/graphical on the install
screen showing progress. Not a log. Log can be another function key away.
Make it look professional", and "an enterprise boot screen (As in trek)".

Its look (the palette, the typeface, the grid, the frame and the shapes)
is `crates/screen-spec` (`stormblock-screen-spec`, #637), a `no_std` crate
that stormbootx's boot panel builds from too. `src/screen` keeps no copy of
them.

`stormblock screen` draws the install's progress full-screen on the
machine's own display: VGA, or a BMC's virtual console. Its look is in the
spirit of a starship bridge status display, but it is our own design: a
black field, rounded elbow frames and pill bars in amber, lavender, pale
blue, peach and warm grey, condensed capitals (Antonio Bold, OFL 1.1,
bundled: `crates/screen-spec/fonts/README.md`) and large numeric readouts. Nothing
is copied and nothing is named after anyone else's.

![forge1 formatting](screen-forge1-formatting.png)

## What it shows

It draws #616's record (`install_progress::record`, built from what `/init`
and the engine write under `/run/stormblock`), the same data as the web page
on :9090, so the two never disagree. It redraws every second.

- **Header:** STORMCOS INSTALL (or BOOT), the node's name and address, the
  machine's vendor, model and service tag (DMI), the release, and when the
  install started. On the right is the big readout: the formatting drives'
  average percent with the ETA, else the current stage, or STOPPED, or
  COMPLETE.
- **Drives:** one row per drive (up to five; more say so): slot, device and
  model, size, how it was found (`LINUX` / `CARD`), its state as a pill
  (`FORMATTING`, `FORMATTED`, `ALREADY 4096 + PI`, `LAID`, `FAILED`, `NOT
  READY`), a progress bar, percent, elapsed and ETA.
- **Steps:** inventory, formats, layout, data set, system legs, hand-over,
  laid, own disk, as pills: done (blue, with when), running (amber, with how
  long so far), pending. An ordinary boot shows only the steps it has.
- **Health:** inlet, exhaust, fans, power from the record's `health`
  (stormipmi#111); until then they read `—`.
- **The end:** a failed install turns the frames red and replaces steps and
  health with a red panel quoting the FATAL/STOP words. A finished one (laid,
  then booted) shows a blue panel with the node's name, address and the
  install record's URL. Either stays until the reboot.

## Keys, terminals and the console

The screen is on tty7 in graphics mode (#638: the node's gettys hold
tty1–tty4), so no console text is drawn over it. **F2** gives the display
back and shows tty8, where the screen writes
`/init`'s log (`init.log`) as it grows. **F1** brings the screen back. Keys
are read raw from both terminals: no key reaches a shell. The serial console
is untouched, except for one line from `/init` saying where the screen draws:
`screen: drawing on /dev/dri/card0 connector 39 crtc 37 1024x768 (1024x768)
(#618)`, or `screen: no display connected: nothing drawn`.

## When it runs

`/init` starts it once the command line is read, on every boot, and it opens
the first DRM card with a connected display. On hardware that is `simpledrm`
over the firmware's framebuffer (the UEFI GOP), with no driver needed.
Headless, it says so and exits. Since #638 it follows every boot past
switch_root into the node's root, until stormpump ends it at Ready (below).
`rd.stormblock.screen=off` runs none.

The layout is drawn on 1920 × 1080 and stretched to the display across and
down apart, so a 4:3 or 5:4 BMC console is filled. Type and corners take the
smaller scale.

![QEMU, 1024 × 768, read back from the display](screen-qemu-1024.png)

## Checked

- `cargo test --lib screen::` covers the canvas shapes and text, the DRM
  structs at the kernel's sizes and ioctl numbers, the keys and the log
  follower, and the screen as pictures: forge1's four drives formatting, the
  failure panel, the finished panel at 1280 × 720, an ordinary boot at
  1024 × 768. With `STORMBLOCK_SCREEN_SNAPSHOTS=<dir>` each is written as a
  PNG. `stormblock screen --snapshot out.png` draws the current record.
- `ci-screen-verify.sh` (`sc-build 'bash ci-screen-verify.sh'`) boots the
  build host's kernel in QEMU with a linear framebuffer (`vga=0x318`, so
  simpledrm binds), runs the screen over a sample install, and reads the
  display back with QEMU's `screendump`. It checks that the frame is drawn,
  that F2 shows the log (no frame colours, text) and that F1 brings the
  screen back.

**No flash at the start (#637).** The display is opened without being
scanned out, tty7 is put in graphics mode, the first frame is drawn into the
buffer, and only then is it set on the CRTC. So the display goes from the
firmware's last picture (stormbootx's panel, the same frame) to the screen's
first with no empty frame between. fbcon's cursor blink is turned off while
the screen holds the display. The kernel's own text can still show on the
framebuffer before the screen starts. `vt.global_cursor_default=0` on the
kernel line hides its cursor; keeping fbcon off the display entirely would
also take away the log on F2.

## After switch_root: stormpump's steps until Ready (#638)

The screen stays up past switch_root on every boot, and stormpump (#237)
ends it.
- **The hand-over.** Just before the mounts move, `/init` sends the screen
  SIGUSR1. It opens `/sysroot` as a directory and says so (`screen.follow`),
  and from then on draws nothing new: its last frame stays up through the
  moves and the switch. Once `/sysroot` in the initramfs's root is empty (its
  mount moved to `/`), it does `fchdir` + `chroot`, says `screen: following
  the node's root after switch_root`, and draws again. Its DRM and terminal
  fds were open all along, so there is no flash. A screen that does not
  answer within 2 s is stopped there, as before.
- **stormpump's steps.** The screen reads `/run/stormpump/boot-progress.events`
  (the install record's line format). Its steps follow the initramfs's own:
  init, early, config, modules, units, network, manifest, coldplug,
  services, ready. With more than nine they go on two rows, under them
  `SERVICES n READY · n STARTED · n FAILED: UNIT (its words)`. `ready` done is
  the NODE READY panel with stormpump's console address; `ready` failed is
  the red NODE NOT READY panel with stormpump's words and the failed
  services. On an ordinary boot the initramfs's part is the boot's own steps
  as they were before the switch. The engine puts the last install's
  record back at its start (#616), and those steps are not this boot's.
- **The end.** stormpump sends SIGTERM at Ready, or after
  `stormpump.screen.max=` (600 s); the screen gives the display back and
  stormpump puts tty1 or tty6 forward. If stormpump never writes its file
  (one older than #237), the screen gives the display back 120 s after the
  switch, and it never holds it past 900 s.
- **Its VTs.** The screen is on tty7 and the log (F2) on tty8, clear of the
  gettys on tty1–tty4 (stormcos#218) and stormpump's tty6.

![forge1 after the hand-over: stormpump's steps, a failed service](screen-forge1-boot-services.png)

`ci-screen-handover-verify.sh` checks it in QEMU. It does the hand-over as
`/init` does (the signal, the moves, busybox switch_root), then the new
root's init writes stormpump's events ending in `ready done`, and the display
read back must show the READY panel.

## Not yet
- A GPU driver that replaces simpledrm after switch_root (mgag200 on an
  iDRAC) takes the display from it.
- The stormbootx panel's look (its own issue), and the health values
  (stormipmi#111).
